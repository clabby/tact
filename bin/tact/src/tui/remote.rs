//! Loop-side answers to web queries and to the web commands that act on the whole process. Each
//! answer calls the module the terminal uses for the same feature; this module only gathers inputs
//! from loop state and moves slow work off the event loop.

use super::{components::AppNode, merge_recent_prompts, session};
use crate::{
    app::{config::Config, error::ConfigEditError, model::ModelCatalog},
    core::extensions::SkillMatches,
    search::FileMatches,
    tui::{
        components::RootNode,
        session::{HistoryPage, RecentPrompt, RecentPrompts},
    },
    web::bridge::{CommandError, ListedMemory, Query, QueryReply, Reply},
};
use std::{fmt::Display, future::Future, path::Path, pin::Pin};
use tact_memory::{MemoryError, MemoryKey, MemoryStore, SelectedMemoryStore};
use tokio::sync::oneshot;

type Work<T> = Pin<Box<dyn Future<Output = Result<T, CommandError>> + Send>>;

/// An answer available now, or work that must finish off the event loop.
pub(super) enum Answer<T> {
    Ready(Result<T, CommandError>),
    Pending(Work<T>),
}

impl<T: Send + 'static> Answer<T> {
    fn pending(work: impl Future<Output = Result<T, CommandError>> + Send + 'static) -> Self {
        Self::Pending(Box::pin(work))
    }

    /// Sends the answer through `reply`, spawning pending work so the loop never waits on it.
    pub(super) fn deliver(self, reply: oneshot::Sender<Result<T, CommandError>>) {
        match self {
            Self::Ready(result) => drop(reply.send(result)),
            Self::Pending(work) => {
                tokio::spawn(async move { drop(reply.send(work.await)) });
            }
        }
    }
}

/// The loop state queries read.
pub(super) struct QueryState<'a> {
    pub(super) app: &'a AppNode,
    pub(super) config: &'a Config,
    pub(super) workspace: &'a Path,
    pub(super) memory_store: Option<&'a SelectedMemoryStore>,
    /// Persisted recent prompts, once the loop has loaded them.
    pub(super) recent_prompts: Option<&'a [RecentPrompt]>,
}

pub(super) fn answer(query: Query, state: &QueryState<'_>) -> Answer<QueryReply> {
    prepare(query, state).unwrap_or_else(|error| Answer::Ready(Err(error)))
}

fn prepare(query: Query, state: &QueryState<'_>) -> Result<Answer<QueryReply>, CommandError> {
    Ok(match query {
        Query::Models => Answer::Ready(Ok(QueryReply::Models(ModelCatalog::new(
            state.config.claude().enabled(),
        )))),
        Query::History { query, cursor } => {
            let config_path = state.config.path().to_path_buf();
            let workspace = state.workspace.to_path_buf();
            Answer::pending(async move {
                let sessions = session::list_async(config_path, workspace, true)
                    .await
                    .map_err(failed)?;
                HistoryPage::new(sessions, &query, cursor.as_deref())
                    .map(QueryReply::History)
                    .map_err(CommandError::Invalid)
            })
        }
        Query::Files { query } => {
            let workspace = state.workspace.to_path_buf();
            Answer::pending(async move {
                tokio::task::spawn_blocking(move || FileMatches::search(&workspace, &query))
                    .await
                    .map(QueryReply::Files)
                    .map_err(failed)
            })
        }
        Query::Skills { query } => {
            let root = state
                .app
                .root(state.app.active_pane())
                .ok_or(CommandError::UnknownSession)?;
            Answer::Ready(Ok(QueryReply::Skills(SkillMatches::new(
                root.skills(),
                &query,
            ))))
        }
        Query::RecentPrompts {
            session,
            scope,
            query,
        } => {
            let current = session_root(state.app, &session)?.recent_prompts().to_vec();
            let workspace = session_root(state.app, &session)?.workspace().to_owned();
            let rank = move |persisted| {
                let prompts = merge_recent_prompts(persisted, current, &session, &workspace);
                QueryReply::RecentPrompts(RecentPrompts::new(prompts, &session, scope, &query))
            };
            match state.recent_prompts {
                Some(persisted) => Answer::Ready(Ok(rank(persisted.to_vec()))),
                None => {
                    let config_path = state.config.path().to_path_buf();
                    Answer::pending(async move {
                        let persisted = session::load_recent_prompts_async(config_path)
                            .await
                            .map_err(failed)?;
                        Ok(rank(persisted))
                    })
                }
            }
        }
        Query::ContextDiagnostics { session } => Answer::Ready(Ok(QueryReply::ContextDiagnostics(
            session_root(state.app, &session)?
                .context_diagnostics()
                .clone(),
        ))),
        Query::Memories => {
            let store = memory_store(state.memory_store)?;
            Answer::pending(async move {
                let access = store.access().await.map_err(failed)?;
                let records = store.list().await.map_err(failed)?;
                let records = records
                    .into_iter()
                    .map(|record| ListedMemory {
                        deletable: access.can_delete(&record.key),
                        record,
                    })
                    .collect();
                Ok(QueryReply::Memories { access, records })
            })
        }
        Query::Config => Answer::Ready(
            state
                .config
                .document()
                .map(QueryReply::Config)
                .map_err(config_edit_error),
        ),
    })
}

/// Deletes a memory under the same rule as the terminal's memory browser.
pub(super) fn delete_memory(store: Option<&SelectedMemoryStore>, key: MemoryKey) -> Answer<Reply> {
    let store = match memory_store(store) {
        Ok(store) => store,
        Err(error) => return Answer::Ready(Err(error)),
    };
    Answer::pending(async move {
        let access = store.access().await.map_err(failed)?;
        if !access.can_delete(&key) {
            return Err(CommandError::NotAvailableRemotely);
        }
        match store.delete(key).await {
            Ok(()) => Ok(Reply::Done),
            Err(MemoryError::Conflict) => Err(CommandError::Stale),
            Err(error) => Err(failed(error)),
        }
    })
}

pub(super) fn config_edit_error(error: ConfigEditError) -> CommandError {
    match error {
        ConfigEditError::Stale => CommandError::Stale,
        ConfigEditError::HoldsCredentials => CommandError::NotAvailableRemotely,
        ConfigEditError::Invalid(error) => CommandError::Invalid(error.to_string()),
        ConfigEditError::Io(error) => failed(error),
    }
}

fn session_root<'a>(app: &'a AppNode, session: &str) -> Result<&'a RootNode, CommandError> {
    app.pane_for_session(session)
        .and_then(|pane| app.root(pane))
        .ok_or(CommandError::UnknownSession)
}

fn memory_store(store: Option<&SelectedMemoryStore>) -> Result<SelectedMemoryStore, CommandError> {
    store.cloned().ok_or_else(|| {
        CommandError::Disabled(
            "Memory is disabled. Enable it with memory.enabled = true.".to_owned(),
        )
    })
}

fn failed(error: impl Display) -> CommandError {
    CommandError::Failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Answer, QueryState, answer, delete_memory};
    use crate::{
        app::{
            config::{Config, ConfigOverrides, ReasoningEffort},
            model::ModelCatalog,
        },
        tui::{
            components::{AppNode, RootNode},
            pane::PaneId,
            session::{RecentPrompt, RecentPromptScope},
            theme::Theme,
        },
        web::bridge::{CommandError, Query, QueryReply},
    };
    use std::{fs, path::Path};
    use tact_memory::MemoryKey;
    use tempfile::tempdir;

    async fn resolve<T>(answer: Answer<T>) -> Result<T, CommandError> {
        match answer {
            Answer::Ready(result) => result,
            Answer::Pending(work) => work.await,
        }
    }

    fn config(directory: &Path) -> Config {
        let path = directory.join("config.toml");
        fs::write(&path, "").unwrap();
        Config::load(ConfigOverrides {
            path: Some(path),
            workspace: Some(directory.to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap()
    }

    fn app(workspace: &Path) -> AppNode {
        let root = RootNode::new(workspace, ReasoningEffort::Low);
        let mut app = AppNode::new(Theme::default(), workspace.to_path_buf(), root);
        app.session_opened(PaneId::Main, "main".to_owned(), Vec::new());
        app
    }

    #[tokio::test]
    async fn queries_are_answered_by_the_shared_modules() {
        let directory = tempdir().unwrap();
        fs::create_dir_all(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/lib.rs"), "").unwrap();
        let config = config(directory.path());
        let app = app(directory.path());
        let recent = [RecentPrompt {
            text: "fix the parser".to_owned(),
            recorded_at_unix_ms: 1,
            session_id: "earlier".to_owned(),
            workspace: directory.path().to_path_buf(),
        }];
        let state = QueryState {
            app: &app,
            config: &config,
            workspace: directory.path(),
            memory_store: None,
            recent_prompts: Some(&recent),
        };
        let query = |query| resolve(answer(query, &state));

        let Ok(QueryReply::Models(catalog)) = query(Query::Models).await else {
            panic!("models");
        };
        assert_eq!(catalog, ModelCatalog::new(config.claude().enabled()));

        let Ok(QueryReply::Files(files)) = query(Query::Files {
            query: "lib".to_owned(),
        })
        .await
        else {
            panic!("files");
        };
        assert_eq!(files.paths, ["src/lib.rs"]);

        let Ok(QueryReply::RecentPrompts(prompts)) = query(Query::RecentPrompts {
            session: "main".to_owned(),
            scope: RecentPromptScope::Global,
            query: "parser".to_owned(),
        })
        .await
        else {
            panic!("recent prompts");
        };
        assert_eq!(prompts.prompts, recent);

        let Ok(QueryReply::History(history)) = query(Query::History {
            query: String::new(),
            cursor: None,
        })
        .await
        else {
            panic!("history");
        };
        assert!(history.sessions.is_empty());

        let Ok(QueryReply::Config(document)) = query(Query::Config).await else {
            panic!("config");
        };
        assert_eq!(document.text, "");

        assert!(matches!(
            query(Query::ContextDiagnostics {
                session: "main".to_owned(),
            })
            .await,
            Ok(QueryReply::ContextDiagnostics(_))
        ));
        assert!(matches!(
            query(Query::Skills {
                query: String::new(),
            })
            .await,
            Ok(QueryReply::Skills(_))
        ));
    }

    #[tokio::test]
    async fn queries_refuse_unknown_sessions_and_disabled_memory() {
        let directory = tempdir().unwrap();
        let config = config(directory.path());
        let app = app(directory.path());
        let state = QueryState {
            app: &app,
            config: &config,
            workspace: directory.path(),
            memory_store: None,
            recent_prompts: None,
        };

        assert!(matches!(
            resolve(answer(
                Query::ContextDiagnostics {
                    session: "gone".to_owned(),
                },
                &state,
            ))
            .await,
            Err(CommandError::UnknownSession)
        ));
        assert!(matches!(
            resolve(answer(Query::Memories, &state)).await,
            Err(CommandError::Disabled(_))
        ));
        assert!(matches!(
            resolve(delete_memory(None, MemoryKey::local(1, 1))).await,
            Err(CommandError::Disabled(_))
        ));
    }
}
