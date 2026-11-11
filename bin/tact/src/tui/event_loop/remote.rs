//! Loop-side answers to web queries and to the web commands that act on the whole process. Each
//! answer calls the module the terminal uses for the same feature; this module only gathers inputs
//! from loop state and moves slow work off the event loop.

use super::recent_prompts::merge_recent_prompts;
use crate::{
    app::{
        config::Config,
        error::ConfigEditError,
        model::{ModelCatalog, available},
    },
    core::{
        extensions::SkillMatches,
        protocol::{CommandError, ListedMemory, Query, QueryReply, Reply},
        session::{HistoryPage, RecentPrompt, RecentPrompts, SessionListScope, SessionStore},
    },
    search::FileMatches,
    tui::components::{AppNode, RootNode},
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
        Query::Models => Answer::Ready(Ok(QueryReply::Models(ModelCatalog::new(&available(
            state.config.claude(),
        ))))),
        Query::History {
            query,
            cursor,
            scope,
        } => {
            let store = SessionStore::new(state.config.path());
            let workspace = state.workspace.to_path_buf();
            let live_sessions = state.app.live_session_ids();
            Answer::pending(async move {
                let sessions = match scope {
                    SessionListScope::Repository => {
                        store.list_workspace_family(&workspace, true).await
                    }
                    SessionListScope::Workspace => {
                        store
                            .run_blocking(move |store| store.list(&[workspace], true))
                            .await
                    }
                }
                .map_err(failed)?;
                let search = query.clone();
                let conversation = store
                    .run_blocking(move |store| store.search_conversations(&search))
                    .await
                    .map_err(failed)?;
                HistoryPage::new(
                    sessions,
                    &query,
                    cursor.as_deref(),
                    conversation,
                    &live_sessions,
                )
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
                    let store = SessionStore::new(state.config.path());
                    Answer::pending(async move {
                        let persisted = store
                            .run_blocking(SessionStore::recent_prompts)
                            .await
                            .map_err(failed)?;
                        Ok(rank(persisted))
                    })
                }
            }
        }
        Query::ContextDiagnostics { session } => {
            Answer::Ready(Ok(QueryReply::ContextDiagnostics(Box::new(
                session_root(state.app, &session)?
                    .context_diagnostics()
                    .snapshot(),
            ))))
        }
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
        Query::Workspaces { .. } => Answer::Ready(Err(CommandError::Invalid(
            "answered by the web server".to_owned(),
        ))),
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
            config::{Config, ConfigOverrides, ReasoningEffort, ReasoningMode, Speed},
            model::{ModelCatalog, available},
            theme::Theme,
        },
        core::{
            pane::PaneId,
            protocol::{CommandError, Query, QueryReply},
            session::{RecentPrompt, RecentPromptScope, SessionListScope},
            storage::SessionStorage,
            transcript::{LocalEvent, SessionStarted, TranscriptRecord, TurnId, UserSubmitted},
        },
        tui::components::{AppNode, RootNode},
    };
    use nanocode::agent::events::{AgentEvent, AgentEventKind};
    use serde_json::{json, value::to_raw_value};
    use std::{fs, path::Path, process::Command, sync::Arc};
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
        assert_eq!(catalog, ModelCatalog::new(&available(config.claude())));

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
            scope: SessionListScope::Repository,
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
    async fn history_searches_conversations_and_reports_matching_live_sessions() {
        let directory = tempdir().unwrap();
        let config = config(directory.path());
        let app = app(directory.path());
        let mut storage = SessionStorage::open(config.path()).unwrap();
        for (session_id, prompt, reply) in [
            (
                "saved",
                "same first prompt",
                "the lexer now handles unicode",
            ),
            ("other", "same first prompt", "nothing relevant"),
            ("main", "live work", "a lexer for the live session"),
        ] {
            let start = SessionStarted {
                session_id: session_id.to_owned(),
                parent_session_id: None,
                parent_sequence: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                speed: Speed::Standard,
                workspace: directory.path().to_path_buf(),
                application_version: "test".to_owned(),
            };
            let prompt = UserSubmitted {
                id: TurnId::new(1),
                text: prompt.to_owned(),
            };
            let reply = AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("request"),
                seq: 1,
                kind: AgentEventKind::AssistantMessage,
                payload: to_raw_value(
                    &json!({ "model_call_index": 0, "phase": null, "text": reply }),
                )
                .unwrap()
                .into(),
            };
            let records = [
                TranscriptRecord::from_local(1, 1, LocalEvent::SessionStarted(start)).unwrap(),
                TranscriptRecord::from_local(2, 2, LocalEvent::UserSubmitted(prompt)).unwrap(),
                TranscriptRecord::from_agent(3, 3, reply),
            ];
            storage
                .append_records(session_id, &records.map(Arc::new))
                .unwrap();
            if session_id != "main" {
                storage.save_resume_state(session_id, b"{}").unwrap();
            }
        }
        let state = QueryState {
            app: &app,
            config: &config,
            workspace: directory.path(),
            memory_store: None,
            recent_prompts: None,
        };

        let Ok(QueryReply::History(history)) = resolve(answer(
            Query::History {
                query: "LEXER".to_owned(),
                cursor: None,
                scope: SessionListScope::Repository,
            },
            &state,
        ))
        .await
        else {
            panic!("history");
        };
        let found = history
            .sessions
            .iter()
            .map(|session| (session.session_id.as_str(), session.snippet.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(found, [("saved", Some("the lexer now handles unicode"))]);
        assert_eq!(
            history.live_matches.get("main").map(String::as_str),
            Some("a lexer for the live session")
        );
    }

    #[tokio::test]
    async fn history_scope_selects_the_repository_or_only_the_workspace() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let checkout = directory.path().join("checkout");
        fs::create_dir(&repository).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.test",
                ])
                .current_dir(&repository)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "fixture"]);
        git(&[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            checkout.to_str().unwrap(),
        ]);
        let repository = repository.canonicalize().unwrap();
        let checkout = checkout.canonicalize().unwrap();
        let config = config(directory.path());
        let app = app(&checkout);
        let mut storage = SessionStorage::open(config.path()).unwrap();
        for (index, (session_id, workspace)) in
            [("in-repository", &repository), ("in-checkout", &checkout)]
                .into_iter()
                .enumerate()
        {
            let start = SessionStarted {
                session_id: session_id.to_owned(),
                parent_session_id: None,
                parent_sequence: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                speed: Speed::Standard,
                workspace: workspace.clone(),
                application_version: "test".to_owned(),
            };
            let started = TranscriptRecord::from_local(
                1,
                index as u64 + 1,
                LocalEvent::SessionStarted(start),
            )
            .unwrap();
            storage
                .append_records(session_id, &[Arc::new(started)])
                .unwrap();
            storage.save_resume_state(session_id, b"{}").unwrap();
        }
        drop(storage);
        let state = QueryState {
            app: &app,
            config: &config,
            workspace: &checkout,
            memory_store: None,
            recent_prompts: None,
        };
        let listed = async |scope| {
            let Ok(QueryReply::History(history)) = resolve(answer(
                Query::History {
                    query: String::new(),
                    cursor: None,
                    scope,
                },
                &state,
            ))
            .await
            else {
                panic!("history");
            };
            history
                .sessions
                .into_iter()
                .map(|session| session.session_id)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            listed(SessionListScope::Repository).await,
            ["in-checkout", "in-repository"]
        );
        assert_eq!(listed(SessionListScope::Workspace).await, ["in-checkout"]);
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
