//! The web front-end as the loop sees it: the server's lifetime, requests, queries, and auxiliary
//! requests from the bridge, and the terminal actions that open or share the web interface.
//!
//! Commands are applied to the active pane exactly as terminal input would be, then the resulting
//! changes are published with the requesting client as their origin. Queries are answered from a
//! snapshot of loop state; work they need beyond it runs off the loop.

use super::{EventLoop, links, remote};
use crate::{
    app::{
        config::Config,
        error::{Result, RuntimeError},
    },
    core::{
        pane::PaneId,
        protocol::{
            AuxiliaryError, AuxiliaryRequest, ClientId, Command, CommandError, Origin,
            QueryRequest, Reply, Request,
        },
        worker::{AuxiliaryContext, WorkerCommand},
    },
    tui::{
        clipboard,
        components::{AppEvent, RenderRequest, RootEffect},
    },
    web::{self, WebAssets, bridge::WebEnd},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How long the web server may take to stop once the loop has finished.
const WEB_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// The web server task, when the web interface is enabled.
pub(super) struct WebServer {
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl WebServer {
    /// Starts the server beside the terminal. It never stops the terminal: its failure only
    /// changes the status that "Open in browser" reports.
    pub(super) fn spawn(
        config: &Config,
        workspace: &Path,
        end: WebEnd,
        shutdown: &CancellationToken,
    ) -> Self {
        let shutdown = shutdown.child_token();
        let task = config
            .web()
            .enabled()
            .then(|| web::spawn(config, workspace, end, shutdown.clone()));
        Self { shutdown, task }
    }

    /// Starts the server as the only front-end. Failing to serve is fatal here, so the server is
    /// listening, and published to the tailnet when configured, once this returns.
    pub(super) async fn serve(
        config: &Config,
        workspace: &Path,
        end: WebEnd,
        shutdown: &CancellationToken,
    ) -> Result<Self> {
        let shutdown = shutdown.child_token();
        let task = web::serve(config, workspace, end, shutdown.clone())
            .await
            .map_err(|error| RuntimeError::Web(error.into()))?;
        Ok(Self {
            shutdown,
            task: Some(task),
        })
    }

    /// Stops the server, waiting a short grace period for it to finish.
    pub(super) async fn stop(self) {
        self.shutdown.cancel();
        if let Some(task) = self.task {
            // A server that does not stop promptly must not hold the process's exit.
            drop(tokio::time::timeout(WEB_SHUTDOWN_GRACE, task).await);
        }
    }
}

/// The outcome of a task that opens or shares the web interface.
pub(super) struct WebTaskCompletion {
    pub(super) pane: PaneId,
    /// A sign-in link to show as a QR code, when the task was a QR request; otherwise a
    /// user-facing failure message is the only thing worth reporting.
    pub(super) result: std::result::Result<Option<String>, String>,
}

impl EventLoop {
    pub(super) async fn on_web_request(&mut self, request: Request) -> Result<()> {
        let Request {
            command,
            client,
            reply,
        } = request;
        let active = self.app.active_pane();
        let result = match command {
            Command::DeleteMemory { key } => {
                remote::delete_memory(self.memory.store(), key).deliver(reply);
                return Ok(());
            }
            Command::Open(spec) => {
                self.open_session(spec, reply).await;
                self.scheduler
                    .request(RenderRequest::Immediate, Instant::now());
                return Ok(());
            }
            Command::ReloadConfig => self.reload_config(active),
            Command::WriteConfig { text, revision } => self
                .config
                .replace_document(&text, &revision)
                .map_err(remote::config_edit_error)
                .and_then(|()| self.reload_config(active)),
            Command::SetMaxSubagents { limit } => self
                .apply_pane_effect(active, RootEffect::SetMaxSubagents(limit))
                .map_err(|error| CommandError::Failed(error.to_string())),
            command => match self.app.remote_command(command) {
                Ok(update) => {
                    self.apply_update(update).await?;
                    Ok(())
                }
                Err(error) => Err(error),
            },
        };
        self.publish_to(client);
        drop(reply.send(result.map(|()| Reply::Done)));
        Ok(())
    }

    fn publish_to(&mut self, client: ClientId) {
        let render = self.app.publish_changes(Origin::Web(client));
        self.scheduler.request(render, Instant::now());
    }

    pub(super) fn on_web_query(&self, request: QueryRequest) {
        let state = remote::QueryState {
            app: &self.app,
            config: &self.config,
            workspace: self.active_workspace(),
            memory_store: self.memory.store(),
            recent_prompts: self.recent_prompts.cached(),
        };
        remote::answer(request.query, &state).deliver(request.reply);
    }

    /// Runs a web client's one-off prompt in a clean context beside the session's conversation.
    pub(super) fn on_auxiliary_request(&mut self, request: AuxiliaryRequest) -> Result<()> {
        let AuxiliaryRequest {
            session,
            prompt,
            shutdown,
            completion,
        } = request;
        if shutdown.is_cancelled() {
            drop(completion.send(Err(AuxiliaryError::Cancelled)));
            return Ok(());
        }
        let Some((pane, runtime)) = self
            .app
            .pane_for_session(&session)
            .and_then(|pane| Some((pane, self.panes.get_mut(pane)?)))
        else {
            drop(completion.send(Err(AuxiliaryError::Failed("unknown session".to_owned()))));
            return Ok(());
        };
        let id = runtime.next_turn_id();
        self.worker.send(WorkerCommand::Auxiliary {
            pane,
            id,
            prompt: prompt.into(),
            context: AuxiliaryContext::Clean,
            shutdown,
            completion,
        })
    }

    pub(super) fn on_web_task_finished(&mut self, completion: WebTaskCompletion) {
        let pane = completion.pane;
        match completion.result {
            Ok(Some(link)) => self.show(AppEvent::ShowWebQr { pane, link }),
            Ok(None) => {}
            Err(error) => self.show(AppEvent::NotifyError { pane, error }),
        }
    }

    /// Opens the web interface in the browser, downloading its assets first when they are absent
    /// and the user confirmed the install.
    pub(super) fn open_web_interface(&mut self, pane: PaneId, install: bool) {
        let home = self
            .config
            .path()
            .parent()
            .unwrap_or(Path::new("."))
            .to_owned();
        let development = crate::app::installation::current().is_development();
        let download = match WebAssets::locate(&home) {
            Ok(web::Located::Ready(_)) => false,
            Ok(web::Located::Absent) if !development && !install => {
                self.show(AppEvent::ConfirmWebInstall { pane });
                return;
            }
            Ok(web::Located::Absent) if !development => true,
            Ok(web::Located::Absent) => {
                let error = "You are running a development build of Tact, which cannot download the web interface automatically. Run `cd web && bun install --frozen-lockfile && just install-dev`, or set TACT_WEB_ASSETS to the absolute `web/dist` path."
                    .to_owned();
                self.show(AppEvent::NotifyError { pane, error });
                return;
            }
            Err(error) => {
                self.show(AppEvent::NotifyError {
                    pane,
                    error: format!("Could not load the web interface: {error}"),
                });
                return;
            }
        };
        let status = self.web_status.clone();
        let enabled = self.config.web().enabled();
        self.web_tasks.spawn(async move {
            let result = async {
                if download {
                    WebAssets::download(&home)
                        .await
                        .map_err(|error| format!("Could not install the web interface: {error}"))?;
                }
                let url = links::web_link(&status, enabled)?;
                crate::app::browser::open(&url).await.map_err(|error| {
                    format!("Could not open the browser: {error}. Use Copy web link instead.")
                })?;
                Ok(None)
            }
            .await;
            WebTaskCompletion { pane, result }
        });
    }

    pub(super) fn copy_web_link(&mut self, pane: PaneId) {
        let copied =
            links::web_link(&self.web_status, self.config.web().enabled()).and_then(|url| {
                let session = self.frontend.session().map_err(|error| error.to_string())?;
                clipboard::copy_selection(session, &url)
            });
        self.show(match copied {
            Ok(()) => AppEvent::NotifySuccess {
                pane,
                message: "Copied the web link. It signs in to Tact; share it carefully.".to_owned(),
            },
            Err(error) => AppEvent::NotifyError { pane, error },
        });
    }

    pub(super) fn show_web_qr(&mut self, pane: PaneId) {
        let status = self.web_status.clone();
        let enabled = self.config.web().enabled();
        self.web_tasks.spawn(async move {
            let result = async {
                let link = links::web_link(&status, enabled)?;
                links::phone_link(link, &status).await.map(Some)
            }
            .await;
            WebTaskCompletion { pane, result }
        });
    }
}
