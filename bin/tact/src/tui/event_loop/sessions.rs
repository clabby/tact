//! Replacing and resuming a pane's session.
//!
//! A pane's agent is configured off the loop, on a background task, and installed into the pane
//! when the task completes. A completion for a pane that closed meanwhile is dropped. Terminal
//! input stays detached while the task runs.

use super::{
    EventLoop,
    background::{TaskKind, TaskOutput},
    panes::{InstalledAgent, InstalledSession, PaneSettings},
};
use crate::{
    app::{
        config::{Config, ReasoningEffort, ReasoningMode, Speed},
        error::{Result, RuntimeError},
    },
    core::{
        ConfiguredAgent,
        pane::PaneId,
        session::{SessionLock, SessionStore, SessionSummary},
        supported_reasoning_mode,
        transcript::TranscriptRecord,
    },
    tui::components::{AppEvent, DraftReset, RestoredSessionProjection, RootNode, SessionListKind},
};
use crossterm::event::EventStream;
use nanocodex::HarnessModel as Model;
use std::{path::Path, sync::Arc, time::Instant};

/// A stored session loaded with an agent configured to continue it.
pub(super) struct RestoredSession {
    pub(super) configured: ConfiguredAgent,
    pub(super) lock: SessionLock,
    pub(super) records: Vec<Arc<TranscriptRecord>>,
    pub(super) projection: RestoredSessionProjection,
    pub(super) reasoning_mode: ReasoningMode,
    pub(super) model: Model,
    pub(super) next_sequence: u64,
}

/// A fresh agent configured to replace a pane's session.
pub(super) struct NewSession {
    pane: PaneId,
    settings: PaneSettings,
    draft_reset: DraftReset,
    configured: Result<ConfiguredAgent>,
}

/// A stored session restored for a pane, with the settings it resumes with.
pub(super) struct ResumedSession {
    pane: PaneId,
    effort: ReasoningEffort,
    preferred_reasoning_mode: ReasoningMode,
    speed: Speed,
    restored: Result<RestoredSession>,
}

/// The settings a fresh session starts with: the configured defaults for `model`.
pub(super) fn fresh_settings(config: &Config, model: Model) -> PaneSettings {
    let agent = config.agent();
    PaneSettings::new(
        agent.thinking(),
        supported_reasoning_mode(model, agent.reasoning_mode()),
        agent.speed(),
        model,
    )
}

/// Loads a persisted session and configures an agent that continues it.
pub(super) async fn restore_session(
    config: Config,
    session_id: String,
    effort: ReasoningEffort,
    lock: SessionLock,
) -> Result<RestoredSession> {
    let store = SessionStore::new(config.path());
    let checkpoint_session_id = session_id.clone();
    let checkpoint = store.run_blocking(move |store| store.load_checkpoint(&checkpoint_session_id));
    let transcript_session_id = session_id.clone();
    let transcript = store.run_blocking(move |store| store.load_transcript(&transcript_session_id));
    let (snapshot, transcript) = tokio::join!(checkpoint, transcript);
    let snapshot = snapshot?;
    let transcript = transcript?;
    let config = config.with_workspace(transcript.workspace()?);
    tokio::task::spawn_blocking(move || -> Result<_> {
        let reasoning_mode = transcript.reasoning_mode();
        let model = transcript.model()?;
        let next_sequence = transcript.next_sequence();
        let records = transcript.into_records();
        let projection = RootNode::project_session(effort, records.clone());
        let configured = ConfiguredAgent::from_config_with_session(
            &config,
            effort,
            reasoning_mode,
            model,
            Some(&session_id),
            Some(snapshot),
        )?;
        Ok(RestoredSession {
            configured,
            lock,
            records,
            projection,
            reasoning_mode,
            model,
            next_sequence,
        })
    })
    .await
    .map_err(RuntimeError::SessionTask)?
}

impl EventLoop {
    /// Installs `configured` as `pane`'s agent in the agent's workspace.
    pub(super) fn install_agent(
        &mut self,
        pane: PaneId,
        configured: ConfiguredAgent,
        history: InstalledSession,
        settings: PaneSettings,
    ) -> Result<InstalledAgent> {
        self.app
            .set_pane_workspace(pane, configured.workspace.clone());
        self.panes.install(
            pane,
            configured,
            history,
            settings,
            &self.config,
            &self.worker,
        )
    }

    /// Replaces a pane's session with a fresh one using the configured defaults for `model`.
    pub(super) fn new_session(&mut self, pane: PaneId, model: Model, workspace: &Path) {
        let settings = fresh_settings(&self.config, model);
        let config = self.config.with_workspace(workspace.to_path_buf());
        self.spawn_new_session(pane, settings, DraftReset::Clear, move |settings| {
            ConfiguredAgent::from_config_with_session(
                &config,
                settings.effort,
                settings.reasoning_mode,
                settings.model,
                None,
                None,
            )
        });
    }

    /// Replaces a pane's session with a fresh one on `model`, keeping the pane's effort and draft.
    pub(super) fn set_model(&mut self, pane: PaneId, model: Model, workspace: &Path) {
        let root = self.app.root(pane).expect("model pane must exist");
        let settings = PaneSettings::new(
            root.composer().effort(),
            supported_reasoning_mode(model, root.preferred_reasoning_mode()),
            self.config.agent().speed(),
            model,
        );
        let config = self.config.with_workspace(workspace.to_path_buf());
        self.spawn_new_session(pane, settings, DraftReset::Preserve, move |settings| {
            ConfiguredAgent::from_config_with_model(
                &config,
                settings.effort,
                settings.reasoning_mode,
                settings.model,
            )
        });
    }

    fn spawn_new_session(
        &mut self,
        pane: PaneId,
        settings: PaneSettings,
        draft_reset: DraftReset,
        configure: impl FnOnce(PaneSettings) -> Result<ConfiguredAgent> + Send + 'static,
    ) {
        self.input = None;
        self.tasks.spawn_blocking(TaskKind::NewSession, move || {
            TaskOutput::NewSession(NewSession {
                pane,
                settings,
                draft_reset,
                configured: configure(settings),
            })
        });
    }

    pub(super) fn on_new_session_configured(&mut self, session: NewSession) -> Result<()> {
        let NewSession {
            pane,
            settings,
            draft_reset,
            configured,
        } = session;
        self.input.get_or_insert_with(EventStream::new);
        match configured {
            Ok(_) if self.app.root(pane).is_none() => {}
            Ok(configured) => {
                let InstalledAgent { session_id, skills } =
                    self.install_agent(pane, configured, InstalledSession::Fresh, settings)?;
                self.app.session_opened(pane, session_id, Vec::new());
                self.show(AppEvent::NewSessionReady {
                    pane,
                    effort: settings.effort,
                    reasoning_mode: settings.reasoning_mode,
                    speed: settings.speed,
                    model: settings.model,
                    draft_reset,
                    skills,
                });
            }
            Err(error) => self.show(AppEvent::NewSessionFailed {
                pane,
                error: error.to_string(),
            }),
        }
        self.scheduler.request_immediate(Instant::now());
        Ok(())
    }

    /// Lists stored sessions in `workspace` for a picker, leaving out the pane's own session.
    pub(super) fn load_sessions(&mut self, pane: PaneId, kind: SessionListKind, workspace: &Path) {
        self.input = None;
        let store = SessionStore::new(self.config.path());
        let workspace = workspace.to_path_buf();
        let active_session_id = self
            .panes
            .session_id(pane)
            .expect("session-list pane must exist")
            .to_owned();
        self.tasks.spawn(TaskKind::SessionList, async move {
            let resumable_only = matches!(kind, SessionListKind::Resume);
            let sessions = store
                .list_workspace_family(&workspace, resumable_only)
                .await
                .map(|mut sessions| {
                    sessions.retain(|session| session.session_id != active_session_id);
                    sessions
                });
            TaskOutput::SessionList {
                pane,
                sessions: sessions.map_err(Into::into),
            }
        });
    }

    pub(super) fn on_sessions_listed(
        &mut self,
        pane: PaneId,
        sessions: Result<Vec<SessionSummary>>,
    ) {
        self.input.get_or_insert_with(EventStream::new);
        self.show(match sessions {
            Ok(sessions) => AppEvent::SessionsLoaded { pane, sessions },
            Err(error) => AppEvent::SessionLoadFailed {
                pane,
                error: format!("Could not load sessions: {error}"),
            },
        });
        self.scheduler.request_immediate(Instant::now());
    }

    /// Resumes a stored session in `pane`, unless it is already live in another pane.
    pub(super) fn resume_session(&mut self, pane: PaneId, session_id: String) {
        if let Some(live) = self.app.pane_for_session(&session_id) {
            self.show(AppEvent::SessionLoadFailed {
                pane,
                error: "That session is already open here.".to_owned(),
            });
            self.app.activate(live);
            return;
        }
        let lock = match SessionLock::acquire(self.config.path(), &session_id) {
            Ok(lock) => lock,
            Err(error) => {
                self.show(AppEvent::SessionLoadFailed {
                    pane,
                    error: format!("Could not resume session: {error}"),
                });
                return;
            }
        };
        self.input = None;
        let agent = self.config.agent();
        let effort = agent.thinking();
        let preferred_reasoning_mode = agent.reasoning_mode();
        let speed = agent.speed();
        let config = self.config.clone();
        self.tasks.spawn(TaskKind::ResumeSession, async move {
            let restored = restore_session(config, session_id, effort, lock).await;
            TaskOutput::ResumeSession(Box::new(ResumedSession {
                pane,
                effort,
                preferred_reasoning_mode,
                speed,
                restored,
            }))
        });
    }

    pub(super) fn on_session_restored(&mut self, session: ResumedSession) -> Result<()> {
        let ResumedSession {
            pane,
            effort,
            preferred_reasoning_mode,
            speed,
            restored,
        } = session;
        self.input.get_or_insert_with(EventStream::new);
        match restored {
            Ok(_) if self.app.root(pane).is_none() => {}
            Ok(RestoredSession {
                configured,
                lock,
                records,
                projection,
                reasoning_mode,
                model,
                next_sequence,
            }) => {
                let InstalledAgent { session_id, skills } = self.install_agent(
                    pane,
                    configured,
                    InstalledSession::Restored {
                        next_sequence,
                        lock,
                    },
                    PaneSettings::new(effort, reasoning_mode, speed, model),
                )?;
                self.app.session_opened(pane, session_id, records);
                self.show(AppEvent::SessionRestored {
                    pane,
                    projection: Box::new(projection),
                    effort,
                    reasoning_mode,
                    preferred_reasoning_mode,
                    speed,
                    model,
                    skills,
                });
            }
            Err(error) => self.show(AppEvent::SessionLoadFailed {
                pane,
                error: format!("Could not resume session: {error}"),
            }),
        }
        self.scheduler.request_immediate(Instant::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{fresh_settings, restore_session};
    use crate::{
        app::{
            config::{Config, ConfigOverrides, ReasoningEffort, ReasoningMode, Speed},
            error::{Error, RuntimeError},
        },
        core::{
            session::{SessionLock, SessionStore},
            storage::SessionStorage,
            transcript::{LocalEvent, SessionStarted, TranscriptRecord},
        },
    };
    use nanocodex::{HarnessModel as Model, Model as CodexModel, agent::session::SessionSnapshot};
    use serde_json::json;
    use std::{fs, sync::Arc};

    #[test]
    fn fresh_openai_sessions_keep_the_pro_reasoning_preference() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[agent]\nreasoning_mode = \"pro\"\n").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_owned()),
            ..ConfigOverrides::default()
        })
        .unwrap();

        for model in [CodexModel::Astra, CodexModel::Sol, CodexModel::Luna] {
            let settings = fresh_settings(&config, Model::Codex(model));
            assert_eq!(settings.reasoning_mode, ReasoningMode::Pro);
            assert_eq!(settings.effort, config.agent().thinking());
        }
    }

    #[tokio::test]
    async fn resume_rejects_the_recorded_workspace_when_it_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "").unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path.clone()),
            workspace: Some(directory.path().to_owned()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let missing = directory.path().join("deleted-checkout");
        let start = SessionStarted {
            session_id: "session".to_owned(),
            parent_session_id: None,
            parent_sequence: None,
            model: Model::Codex(CodexModel::Luna).to_string(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            workspace: missing.clone(),
            application_version: "test".to_owned(),
        };
        let record = Arc::new(
            TranscriptRecord::from_local(1, 1, LocalEvent::SessionStarted(start)).unwrap(),
        );
        SessionStorage::open(&config_path)
            .unwrap()
            .append_records("session", &[record])
            .unwrap();
        let snapshot: SessionSnapshot = serde_json::from_value(json!({
            "version": 1,
            "model": nanocodex::oai::MODEL,
            "lineage_id": "resume",
            "prompt_cache_key": "cache-resume",
            "workspace": "/work",
            "request_prefix": [
                {"type": "additional_tools", "role": "developer", "tools": []},
                {"type": "message", "role": "developer", "content": []}
            ],
            "canonical_context": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "canonical"}]
            },
            "history": []
        }))
        .unwrap();
        SessionStore::new(&config_path)
            .save_checkpoint("session", &snapshot, "instructions", false)
            .unwrap();
        let lock = SessionLock::acquire(&config_path, "session").unwrap();

        let result =
            restore_session(config, "session".to_owned(), ReasoningEffort::Low, lock).await;

        let Err(error) = result else {
            panic!("missing workspace must prevent resume");
        };
        assert!(matches!(
            error,
            Error::Runtime(RuntimeError::ResolveWorkspace { path, .. }) if path == missing
        ));
    }
}
