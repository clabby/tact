//! Persisted display titles are read and written off the session event loop.

use super::EventLoop;
use crate::{
    app::config::Config,
    core::{
        protocol::{CommandError, Reply},
        storage::{SessionStorage, StorageError},
        titles::{self, Title},
    },
    tui::components::{AppEvent, RenderRequest},
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

type RenameReply = oneshot::Sender<Result<Reply, CommandError>>;

pub(super) struct Rename {
    session: String,
    title: String,
    reply: Option<RenameReply>,
}

pub(super) enum TitleUpdate {
    Snapshot {
        titles: HashMap<String, Title>,
        /// Whether sessions without a stored title yet can expect a generated one.
        generating: bool,
    },
    Renamed {
        request: Rename,
        result: Result<(), CommandError>,
    },
}

pub(super) fn spawn(
    config: Config,
    shutdown: CancellationToken,
) -> (
    mpsc::UnboundedSender<Rename>,
    mpsc::UnboundedReceiver<TitleUpdate>,
) {
    let (requests, mut renames) = mpsc::unbounded_channel::<Rename>();
    let (updates, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                Some(request) = renames.recv() => {
                    let path = config.path().to_owned();
                    let session = request.session.clone();
                    let title = request.title.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        SessionStorage::open(&path)?.rename_session(&session, &title)
                    }).await.map_err(|error| CommandError::Failed(error.to_string()))
                        .and_then(|result| result.map_err(|error| CommandError::Failed(error.to_string())));
                    if updates.send(TitleUpdate::Renamed { request, result }).is_err() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    let config = config.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let titles = SessionStorage::open_read_only(config.path())?
                            .map(|storage| storage.titles()).transpose()
                            .map(Option::unwrap_or_default)?;
                        Ok::<_, StorageError>((titles, titles::available(&config)))
                    }).await;
                    if let Ok(Ok((titles, generating))) = result
                        && updates.send(TitleUpdate::Snapshot { titles, generating }).is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    (requests, receiver)
}

impl EventLoop {
    pub(super) fn rename_session(
        &mut self,
        session: String,
        title: String,
        reply: Option<RenameReply>,
    ) {
        if self.app.pane_for_session(&session).is_none() {
            self.on_title_update(TitleUpdate::Renamed {
                request: Rename {
                    session,
                    title,
                    reply,
                },
                result: Err(CommandError::UnknownSession),
            });
            return;
        }
        let has_control = title.chars().any(char::is_control);
        let title = title.trim().to_owned();
        if title.is_empty() || has_control || title.chars().count() > 200 {
            self.on_title_update(TitleUpdate::Renamed {
                request: Rename {
                    session,
                    title,
                    reply,
                },
                result: Err(CommandError::Invalid(
                    "Use a title of 1–200 characters on one line.".to_owned(),
                )),
            });
            return;
        }
        if let Err(error) = self.titles.send(Rename {
            session,
            title,
            reply,
        }) {
            self.on_title_update(TitleUpdate::Renamed {
                request: error.0,
                result: Err(CommandError::Failed("title storage stopped".to_owned())),
            });
        }
    }

    pub(super) fn on_title_update(&mut self, update: TitleUpdate) {
        let mut changed = false;
        match update {
            TitleUpdate::Snapshot { titles, generating } => {
                changed = self.app.set_title_generation(generating);
                for (session, title) in titles {
                    changed |= self.app.set_session_title(&session, title);
                }
            }
            TitleUpdate::Renamed { request, result } => {
                if result.is_ok() {
                    changed = self.app.set_session_title(
                        &request.session,
                        Title {
                            text: request.title,
                            pending: false,
                        },
                    );
                }
                if let Some(reply) = request.reply {
                    drop(reply.send(result.map(|()| Reply::Done)));
                } else if let Err(error) = result
                    && let Some(pane) = self.app.pane_for_session(&request.session)
                {
                    self.show(AppEvent::NotifyError {
                        pane,
                        error: error.to_string(),
                    });
                }
            }
        }
        if changed {
            self.scheduler
                .request(RenderRequest::Immediate, Instant::now());
        }
    }
}
