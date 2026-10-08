//! Sessions opened beside the existing panes: fresh, resumed, and forked sessions requested by the
//! web interface or the Sessions action.
//!
//! An open runs on a task in [`PendingOpens`] and its pane is filled in when the task completes. A
//! fork is opened by the worker, which reports it as a worker event. A web caller waiting for an
//! open or fork is answered once the pane's session is live, or with the reason it failed.

use super::{
    EventLoop,
    panes::{InstalledAgent, InstalledSession, PaneAgent, PaneSession, PaneSettings},
    sessions::{self, RestoredSession},
};
use crate::{
    app::{
        config::{Config, ReasoningMode},
        error::{Result, RuntimeError},
    },
    core::{
        ConfiguredAgent,
        pane::PaneId,
        protocol::{CommandError, OpenSpec, Reply},
        session::{self, SessionLock},
        worker::WorkerCommand,
    },
    tui::components::{AppEvent, AppNode, DraftReset},
};
use nanocodex::{AgentEvents, HarnessModel as Model};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    sync::oneshot,
    task::{JoinError, JoinSet},
};

type CommandReply = oneshot::Sender<std::result::Result<Reply, CommandError>>;

/// A session being started for a pane that was opened beside the others.
pub(super) enum OpenedSession {
    Fresh {
        configured: Box<ConfiguredAgent>,
        settings: PaneSettings,
    },
    Restored {
        restored: Box<RestoredSession>,
        settings: PaneSettings,
        preferred_reasoning_mode: ReasoningMode,
    },
}

pub(super) type OpenTask = (PaneId, Result<OpenedSession>);

/// How a request to open a session began.
pub(super) enum OpenStart {
    /// The session is opening in this pane; the reply follows when it is live.
    Pending(PaneId),
    /// The session was already live here and is now active.
    Activated(String),
    /// The pane was created for a fork of `parent`, which the worker has yet to open.
    Fork { pane: PaneId, parent: PaneId },
}

/// Open tasks in flight and the web callers waiting for their panes.
#[derive(Default)]
pub(super) struct PendingOpens {
    tasks: JoinSet<OpenTask>,
    replies: HashMap<PaneId, CommandReply>,
}

impl PendingOpens {
    /// Configures a fresh agent with the configured defaults for `model` for `pane`.
    pub(super) fn spawn_fresh(&mut self, config: &Config, pane: PaneId, model: Model) {
        let settings = sessions::fresh_settings(config, model);
        let config = config.clone();
        self.tasks.spawn(async move {
            let configured = tokio::task::spawn_blocking(move || {
                ConfiguredAgent::from_config_with_session(
                    &config,
                    settings.effort,
                    settings.reasoning_mode,
                    settings.model,
                    None,
                    None,
                )
            })
            .await
            .map_err(|error| RuntimeError::SessionTask(error).into())
            .and_then(|configured| configured);
            (
                pane,
                configured.map(|configured| OpenedSession::Fresh {
                    configured: Box::new(configured),
                    settings,
                }),
            )
        });
    }

    /// Starts opening a session for the web interface with the preconditions of the Sessions
    /// action.
    pub(super) fn start(
        &mut self,
        spec: OpenSpec,
        app: &mut AppNode,
        config: &Config,
    ) -> std::result::Result<OpenStart, CommandError> {
        match spec {
            OpenSpec::New { model, workspace } => {
                let config = match workspace {
                    Some(path) => config.with_workspace(open_workspace(&path)?),
                    None => config.clone(),
                };
                let model = match model {
                    Some(model) => {
                        crate::app::model::parse(&model).map_err(CommandError::Invalid)?
                    }
                    None => app
                        .root(app.active_pane())
                        .map(|root| root.composer().model())
                        .ok_or(CommandError::UnknownSession)?,
                };
                let pane = app.begin_open("Starting new session…")?;
                self.spawn_fresh(&config, pane, model);
                Ok(OpenStart::Pending(pane))
            }
            OpenSpec::Resume { session } => {
                if let Some(pane) = app.pane_for_session(&session) {
                    app.activate(pane);
                    return Ok(OpenStart::Activated(session));
                }
                app.can_open()?;
                let lock =
                    SessionLock::acquire(config.path(), &session).map_err(|error| match error {
                        session::SessionError::Locked { .. } => CommandError::SessionLocked,
                        error => CommandError::Failed(error.to_string()),
                    })?;
                let pane = app.begin_open("Resuming session…")?;
                let agent = config.agent();
                let (effort, speed) = (agent.thinking(), agent.speed());
                let preferred_reasoning_mode = agent.reasoning_mode();
                let config = config.clone();
                self.tasks.spawn(async move {
                    let restored = sessions::restore_session(config, session, effort, lock).await;
                    let opened = restored.map(|restored| OpenedSession::Restored {
                        settings: PaneSettings::new(
                            effort,
                            restored.reasoning_mode,
                            speed,
                            restored.model,
                        ),
                        restored: Box::new(restored),
                        preferred_reasoning_mode,
                    });
                    (pane, opened)
                });
                Ok(OpenStart::Pending(pane))
            }
            OpenSpec::Fork { session } => {
                let parent = app
                    .pane_for_session(&session)
                    .ok_or(CommandError::UnknownSession)?;
                let pane = app.begin_fork(parent)?;
                Ok(OpenStart::Fork { pane, parent })
            }
        }
    }

    /// Answers `reply` once `pane`'s session is live or has failed to open.
    pub(super) fn await_reply(&mut self, pane: PaneId, reply: CommandReply) {
        self.replies.insert(pane, reply);
    }

    /// Answers the web caller waiting for `pane`, if there is one.
    fn reply(&mut self, pane: PaneId, result: std::result::Result<Reply, CommandError>) {
        if let Some(reply) = self.replies.remove(&pane) {
            drop(reply.send(result));
        }
    }

    pub(super) async fn join_next(&mut self) -> Option<std::result::Result<OpenTask, JoinError>> {
        self.tasks.join_next().await
    }

    pub(super) fn abort_all(&mut self) {
        self.tasks.abort_all();
    }
}

/// The canonical form of a workspace requested by the web interface, which must be an absolute
/// path to an existing directory.
fn open_workspace(path: &str) -> std::result::Result<PathBuf, CommandError> {
    let directory = Path::new(path);
    if !directory.is_absolute() {
        return Err(CommandError::Invalid(format!(
            "workspace must be an absolute directory: {path}"
        )));
    }
    directory
        .canonicalize()
        .ok()
        .filter(|directory| directory.is_dir())
        .ok_or_else(|| {
            CommandError::Invalid(format!("workspace is not an existing directory: {path}"))
        })
}

impl EventLoop {
    /// Starts opening a session for a web caller, answering it now or once the session is live.
    pub(super) async fn open_session(&mut self, spec: OpenSpec, reply: CommandReply) {
        let pane = match self.opens.start(spec, &mut self.app, &self.config) {
            Ok(OpenStart::Pending(pane)) => pane,
            Ok(OpenStart::Fork { pane, parent }) => {
                if let Err(error) = self.request_fork(pane, parent).await {
                    drop(reply.send(Err(CommandError::Failed(error.to_string()))));
                    return;
                }
                pane
            }
            Ok(OpenStart::Activated(session)) => {
                drop(reply.send(Ok(Reply::Opened { session })));
                return;
            }
            Err(error) => {
                drop(reply.send(Err(error)));
                return;
            }
        };
        self.opens.await_reply(pane, reply);
    }

    /// Installs an opened session into its pane, unless the pane closed while it opened.
    pub(super) async fn on_open_finished(&mut self, (pane, opened): OpenTask) -> Result<()> {
        let opened = match opened {
            Ok(opened) if self.app.root(pane).is_some() => opened,
            Ok(_) => {
                self.opens.reply(
                    pane,
                    Err(CommandError::Failed(
                        "the session was closed while it opened".to_owned(),
                    )),
                );
                return Ok(());
            }
            Err(error) => {
                let error = error.to_string();
                self.apply(AppEvent::OpenFailed {
                    pane,
                    error: error.clone(),
                })
                .await?;
                self.opens.reply(pane, Err(CommandError::Failed(error)));
                return Ok(());
            }
        };
        let (session, records) = match opened {
            OpenedSession::Fresh {
                configured,
                settings,
            } => {
                let InstalledAgent { session_id, skills } =
                    self.install_agent(pane, *configured, InstalledSession::Fresh, settings)?;
                self.show(AppEvent::NewSessionReady {
                    pane,
                    effort: settings.effort,
                    reasoning_mode: settings.reasoning_mode,
                    speed: settings.speed,
                    model: settings.model,
                    draft_reset: DraftReset::Clear,
                    skills,
                });
                (session_id, Vec::new())
            }
            OpenedSession::Restored {
                restored,
                settings,
                preferred_reasoning_mode,
            } => {
                let RestoredSession {
                    configured,
                    lock,
                    records,
                    projection,
                    next_sequence,
                    ..
                } = *restored;
                let InstalledAgent { session_id, skills } = self.install_agent(
                    pane,
                    configured,
                    InstalledSession::Restored {
                        next_sequence,
                        lock,
                    },
                    settings,
                )?;
                self.show(AppEvent::SessionRestored {
                    pane,
                    projection: Box::new(projection),
                    effort: settings.effort,
                    reasoning_mode: settings.reasoning_mode,
                    preferred_reasoning_mode,
                    speed: settings.speed,
                    model: settings.model,
                    skills,
                });
                (session_id, records)
            }
        };
        self.app.session_opened(pane, session.clone(), records);
        self.opens.reply(pane, Ok(Reply::Opened { session }));
        Ok(())
    }

    /// Asks the worker to fork `parent` into `pane` at the parent's last persisted record.
    pub(super) async fn request_fork(&mut self, pane: PaneId, parent: PaneId) -> Result<()> {
        let journal = self.panes.runtime(parent)?.journal_mut()?;
        journal.flush().await?;
        let parent_sequence = journal.last_sequence();
        self.worker.send(WorkerCommand::OpenFork {
            pane,
            parent,
            parent_sequence,
        })
    }

    /// Opens the runtime of a fork the worker created, unless the fork or its parent closed
    /// meanwhile.
    pub(super) async fn on_fork_opened(
        &mut self,
        pane: PaneId,
        parent: PaneId,
        parent_sequence: u64,
        events: AgentEvents,
    ) -> Result<()> {
        let session_id = events.request_id().to_owned();
        let Some(parent_runtime) = self
            .panes
            .get(parent)
            .filter(|_| self.app.root(pane).is_some())
        else {
            self.worker.send(WorkerCommand::ClosePane(pane))?;
            return self
                .on_fork_failed(
                    pane,
                    "the session closed while it was being forked".to_owned(),
                )
                .await;
        };
        let effort = self.app.root(pane).map_or_else(
            || self.config.agent().thinking(),
            |root| root.composer().effort(),
        );
        let parent_session_id = parent_runtime.session_id.clone();
        let agent = PaneAgent {
            settings: PaneSettings {
                effort,
                ..parent_runtime.settings
            },
            instructions: Arc::clone(&parent_runtime.instructions),
            skills_catalog_present: parent_runtime.skills_catalog_present,
            subagent_control: parent_runtime.subagent_control.clone(),
        };
        let config = self.config.with_workspace(
            self.app
                .root(pane)
                .expect("fork pane exists")
                .workspace()
                .to_owned(),
        );
        let lock = SessionLock::acquire(self.config.path(), &session_id)?;
        let started = self
            .panes
            .open(
                pane,
                0,
                PaneSession::fork(&session_id, &parent_session_id, parent_sequence),
                agent,
                &config,
                lock,
            )?
            .journal_mut()?
            .persist_start()
            .await?
            .expect("a new fork must have a deferred session start");
        self.panes.forward_agent_events(pane, 0, events);
        self.app
            .session_opened(pane, session_id.clone(), Vec::new());
        self.apply(AppEvent::Transcript {
            pane,
            record: started,
        })
        .await?;
        self.apply(AppEvent::ForkReady { pane }).await?;
        self.opens.reply(
            pane,
            Ok(Reply::Opened {
                session: session_id,
            }),
        );
        Ok(())
    }

    pub(super) async fn on_fork_failed(&mut self, pane: PaneId, error: String) -> Result<()> {
        self.opens
            .reply(pane, Err(CommandError::Failed(error.clone())));
        self.apply(AppEvent::ForkFailed { pane, error }).await
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenStart, OpenedSession, PendingOpens, open_workspace};
    use crate::{
        app::config::{Config, ConfigOverrides, ReasoningEffort},
        core::protocol::{CommandError, OpenSpec},
        tui::components::{AppNode, RootNode},
    };
    use std::{fs, path::Path};
    use tempfile::tempdir;

    fn workspace_config(directory: &Path) -> Config {
        let path = directory.join("config.toml");
        fs::write(&path, "[auth]\nmode = 'api-key'\n[openai]\napi_key = 'workspace-fixture'\n[agent]\nweb_search = false\nimage_generation = false\n[skills]\nenabled = false\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        Config::load(ConfigOverrides {
            path: Some(path),
            auth_file: Some(directory.join("unused-auth.json")),
            workspace: Some(directory.to_owned()),
            ..ConfigOverrides::default()
        })
        .unwrap()
    }

    fn app(config: &Config, workspace: &Path) -> AppNode {
        AppNode::new(
            config.theme().clone(),
            workspace.to_owned(),
            RootNode::new(workspace, ReasoningEffort::Low),
        )
    }

    #[tokio::test]
    async fn web_open_builds_the_agent_and_pane_in_the_selected_workspace() {
        let directory = tempdir().unwrap();
        let selected = tempdir().unwrap();
        let selected = selected.path().canonicalize().unwrap();
        let config = workspace_config(directory.path());
        let mut app = app(&config, directory.path());
        let mut opens = PendingOpens::default();

        let started = opens.start(
            OpenSpec::New {
                model: Some("sol".to_owned()),
                workspace: Some(selected.to_string_lossy().into_owned()),
            },
            &mut app,
            &config,
        );

        let Ok(OpenStart::Pending(pane)) = started else {
            panic!("expected pending session")
        };
        let (opened_pane, opened) = opens.join_next().await.unwrap().unwrap();
        assert_eq!(opened_pane, pane);
        let OpenedSession::Fresh { configured, .. } = opened.unwrap() else {
            panic!("expected fresh session")
        };
        assert_eq!(configured.workspace, selected);
        app.set_pane_workspace(pane, configured.workspace.clone());
        assert_eq!(app.root(pane).unwrap().workspace(), selected);
        assert_eq!(config.agent().workspace(), directory.path());
        assert_eq!(
            config.with_workspace(selected).memory_workspace(),
            directory.path()
        );
        configured.agent.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn web_open_rejects_a_workspace_that_is_not_an_absolute_directory() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("file");
        fs::write(&file, "text").unwrap();
        let config = workspace_config(directory.path());
        let mut app = app(&config, directory.path());
        let mut opens = PendingOpens::default();

        let started = opens.start(
            OpenSpec::New {
                model: None,
                workspace: Some(file.to_string_lossy().into_owned()),
            },
            &mut app,
            &config,
        );

        assert!(matches!(started, Err(CommandError::Invalid(_))));
        assert!(opens.join_next().await.is_none());
        assert!(matches!(
            open_workspace("relative"),
            Err(CommandError::Invalid(_))
        ));
        assert_eq!(
            open_workspace(&directory.path().to_string_lossy()),
            Ok(directory.path().canonicalize().unwrap())
        );
    }
}
