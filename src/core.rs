//! Nanocodex construction, turn execution, and graceful shutdown.

use crate::{
    config::{Config, ReasoningEffort, ReasoningMode, SkillsConfig},
    error::{Result, RuntimeError},
    mcp,
    skills::SkillCatalog,
    subagents::{self, ScopedAgentUpdate, SubagentControl},
    tui::session::ResumeState,
};
use nanocodex::{AgentEvents, Nanocodex, NanocodexError, Responses, Tools, TurnControl};
use nanocodex_core::ModelConfig;
use std::{
    io,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct ConfiguredAgent {
    pub(crate) agent: Nanocodex,
    pub(crate) events: AgentEvents,
    pub(crate) instructions: Arc<str>,
    pub(crate) subagent_updates: mpsc::UnboundedReceiver<ScopedAgentUpdate>,
    pub(crate) subagent_control: SubagentControl,
}

enum Cancellation {
    NotRequested,
    Requested,
    Failed(NanocodexError),
}

impl ConfiguredAgent {
    pub(crate) async fn run_from_config(
        config: &Config,
        prompt: String,
        shutdown: CancellationToken,
    ) -> Result<()> {
        Self::from_config(config)?
            .run(prompt, shutdown, io::stdout())
            .await
    }

    pub(crate) fn from_config(config: &Config) -> Result<Self> {
        Self::from_config_with_session(
            config,
            config.agent().thinking(),
            config.agent().reasoning_mode(),
            None,
            None,
        )
    }

    pub(crate) fn from_config_with_session(
        config: &Config,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        session_id: Option<&str>,
        resume: Option<ResumeState>,
    ) -> Result<Self> {
        let agent_config = config.agent();
        let workspace = Self::resolve_workspace(agent_config.workspace())?;
        let mcp = mcp::provider(config)?;
        let auth = config.auth().load()?;

        let mut responses = Responses::builder();
        if let Some(url) = agent_config.websocket_url() {
            responses = responses.websocket_url(url);
        }
        if let Some(url) = agent_config.api_base_url() {
            responses = responses.api_base_url(url);
        }

        let mut tools = Tools::builder()
            .web_search(agent_config.web_search())
            .image_generation(agent_config.image_generation());
        if let Some(mcp) = mcp {
            tools = tools.provider(mcp);
        }
        let tools = tools.build().map_err(NanocodexError::from)?;
        let (subagents, subagent_control, subagent_updates) =
            subagents::channel(agent_config.max_subagents());
        let mut builder = Nanocodex::builder(auth)
            .workspace(workspace)
            .thinking(thinking.into())
            .reasoning_mode(reasoning_mode.into())
            .fast_mode(agent_config.fast_mode())
            .responses(responses.build())
            .tools_factory(move |agent| {
                subagents::root_tools(tools.clone(), agent, Arc::clone(&subagents))
            });
        if let Some(codex_home) = config.codex_home() {
            builder = builder.codex_home(codex_home);
        }
        let (snapshot, restored_instructions) = resume
            .map(ResumeState::into_parts)
            .map_or((None, None), |(snapshot, instructions)| {
                (Some(snapshot), Some(instructions))
            });
        let instructions = session_instructions(
            agent_config.instructions(),
            agent_config.append_instructions(),
            config.skills(),
            restored_instructions,
        );
        builder = builder.instructions(Arc::clone(&instructions));
        if let Some(session_id) = session_id {
            builder = builder.session_id(session_id);
        }
        if let Some(snapshot) = snapshot {
            builder = builder.resume(snapshot);
        }

        let (agent, events) = builder.build()?;
        Ok(Self {
            agent,
            events,
            instructions,
            subagent_updates,
            subagent_control,
        })
    }

    async fn run(
        mut self,
        prompt: String,
        shutdown: CancellationToken,
        mut output: impl Write,
    ) -> Result<()> {
        let (_unused_sender, empty_updates) = mpsc::unbounded_channel();
        let mut subagent_updates = std::mem::replace(&mut self.subagent_updates, empty_updates);
        let subagent_drain =
            tokio::spawn(async move { while subagent_updates.recv().await.is_some() {} });
        if shutdown.is_cancelled() {
            self.shutdown().await;
            subagent_drain.abort();
            return Ok(());
        }

        let turn = match self.agent.prompt(prompt).await {
            Ok(turn) => turn,
            Err(error) => {
                self.shutdown().await;
                subagent_drain.abort();
                return Err(error.into());
            }
        };
        let control = turn.control();
        let root_session_id = self.events.request_id().to_owned();
        let mut cancellation = Cancellation::NotRequested;
        let event_result = tokio::select! {
            biased;
            result = self.events.write_turn_jsonl(&mut output) => result,
            () = shutdown.cancelled() => {
                cancellation = Cancellation::request(&control).await;
                self.subagent_control
                    .cancel_all(&root_session_id)
                    .await;
                self.events.write_turn_jsonl(&mut output).await
            }
        };

        if event_result.is_err() && matches!(cancellation, Cancellation::NotRequested) {
            cancellation = Cancellation::request(&control).await;
            self.subagent_control.cancel_all(&root_session_id).await;
        }

        let turn_result = turn.result().await;
        let was_cancelled = matches!(cancellation, Cancellation::Requested);
        drop(control);
        self.subagent_control.close_all(&root_session_id).await;
        self.shutdown().await;
        subagent_drain.abort();

        event_result?;
        if let Cancellation::Failed(error) = cancellation {
            return Err(error.into());
        }
        match turn_result {
            Err(NanocodexError::TurnCancelled) if was_cancelled => Ok(()),
            result => result.map(|_| ()).map_err(Into::into),
        }
    }

    async fn shutdown(mut self) {
        drop(self.agent);
        while self.events.recv().await.is_some() {}
    }

    fn resolve_workspace(path: &Path) -> Result<PathBuf> {
        let workspace = path
            .canonicalize()
            .map_err(|source| RuntimeError::ResolveWorkspace {
                path: path.to_path_buf(),
                source,
            })?;
        if !workspace.is_dir() {
            return Err(RuntimeError::WorkspaceNotDirectory(workspace).into());
        }

        Ok(workspace)
    }
}

fn session_instructions(
    custom: Option<&str>,
    appended: Option<&str>,
    skills: &SkillsConfig,
    restored: Option<String>,
) -> Arc<str> {
    restored.map_or_else(
        || Arc::from(fresh_instructions(custom, appended, skills)),
        Arc::from,
    )
}

fn fresh_instructions(
    custom: Option<&str>,
    appended: Option<&str>,
    skills: &SkillsConfig,
) -> String {
    let catalog = SkillCatalog::load(skills);
    let mut instructions = custom
        .map(str::to_owned)
        .unwrap_or_else(|| ModelConfig::default().system_prompt.to_string());
    if let Some(appended) = appended {
        instructions.push_str("\n\n");
        instructions.push_str(appended);
    }
    catalog
        .rendered_instructions()
        .map_or(instructions.clone(), |skill_instructions| {
            format!("{instructions}\n\n{skill_instructions}")
        })
}

impl Cancellation {
    async fn request(control: &TurnControl) -> Self {
        match control.cancel().await {
            Ok(()) => Self::Requested,
            Err(NanocodexError::TurnNotCancellable) => Self::NotRequested,
            Err(error) => Self::Failed(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfiguredAgent, fresh_instructions, session_instructions};
    use crate::{
        config::SkillsConfig,
        error::{Error, RuntimeError},
    };
    use nanocodex::{
        Nanocodex, NanocodexError, Responses, ResponsesAttempt, ResponsesServiceResponse,
    };
    use nanocodex_core::{
        ModelConfig, OpenAiAuth, OpenAiAuthError, OpenAiAuthFuture, OpenAiAuthMode,
        OpenAiAuthSnapshot, OpenAiAuthSource, Thinking,
        responses::{RequestProfile, ResponseCreate},
    };
    use std::{
        fs,
        future::{Pending, pending},
        result::Result as StdResult,
        sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };
    use tempfile::tempdir;
    use tokio::{sync::Notify, time::timeout};
    use tokio_util::sync::CancellationToken;
    use tower::Service;

    struct TestChatGptAuth;

    impl OpenAiAuthSource for TestChatGptAuth {
        fn validate(&self) -> StdResult<(), OpenAiAuthError> {
            Ok(())
        }

        fn snapshot(&self) -> OpenAiAuthFuture<'_, StdResult<OpenAiAuthSnapshot, OpenAiAuthError>> {
            Box::pin(async {
                Ok(OpenAiAuthSnapshot::new(
                    OpenAiAuthMode::ChatGpt,
                    "test-token",
                    Some("test-account"),
                    false,
                    1,
                ))
            })
        }

        fn recover_unauthorized(
            &self,
            _rejected: &OpenAiAuthSnapshot,
        ) -> OpenAiAuthFuture<'_, StdResult<(), OpenAiAuthError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Clone)]
    struct PendingService {
        called: Arc<Notify>,
    }

    impl Service<ResponsesAttempt> for PendingService {
        type Response = ResponsesServiceResponse;
        type Error = NanocodexError;
        type Future = Pending<StdResult<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
            self.called.notify_one();
            pending()
        }
    }

    #[test]
    fn workspace_must_be_a_directory() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("file");
        fs::write(&file, "contents").unwrap();

        let error = ConfiguredAgent::resolve_workspace(&file).unwrap_err();
        let file = file.canonicalize().unwrap();

        assert!(matches!(
            error,
            Error::Runtime(RuntimeError::WorkspaceNotDirectory(path)) if path == file
        ));
    }

    #[test]
    fn fresh_disabled_skills_do_not_change_instructions() {
        let disabled = SkillsConfig::from_roots(false, Vec::new());

        assert_eq!(
            fresh_instructions(None, None, &disabled),
            ModelConfig::default().system_prompt.as_ref()
        );
        assert_eq!(
            fresh_instructions(Some("Custom instructions."), None, &disabled),
            "Custom instructions."
        );
    }

    #[test]
    fn appended_instructions_extend_the_default_or_replacement() {
        let disabled = SkillsConfig::from_roots(false, Vec::new());
        let default = ModelConfig::default().system_prompt;

        let instructions = fresh_instructions(None, Some("Project instructions."), &disabled);
        assert_eq!(instructions, format!("{default}\n\nProject instructions."));
        assert_eq!(
            fresh_instructions(
                Some("Replacement."),
                Some("Project instructions."),
                &disabled
            ),
            "Replacement.\n\nProject instructions."
        );
    }

    #[test]
    fn enabled_skills_extend_the_current_default_with_metadata_only() {
        let directory = tempdir().unwrap();
        let skill_directory = directory.path().join("review");
        fs::create_dir(&skill_directory).unwrap();
        let skill_path = skill_directory.join("SKILL.md");
        fs::write(
            &skill_path,
            "---\nname: review\ndescription: Review code carefully.\n---\nBODY-SENTINEL\n",
        )
        .unwrap();
        let enabled = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        let instructions = fresh_instructions(None, None, &enabled);
        let default = ModelConfig::default().system_prompt;

        assert!(instructions.starts_with(default.as_ref()));
        assert!(instructions.contains("Review code carefully."));
        assert!(
            instructions.contains(&fs::canonicalize(skill_path).unwrap().display().to_string())
        );
        assert!(!instructions.contains("BODY-SENTINEL"));
    }

    #[test]
    fn enabled_skills_preserve_then_extend_custom_instructions() {
        let directory = tempdir().unwrap();
        let skill_directory = directory.path().join("test");
        fs::create_dir(&skill_directory).unwrap();
        fs::write(
            skill_directory.join("SKILL.md"),
            "---\nname: test\ndescription: Run focused tests.\n---\nSECRET-BODY\n",
        )
        .unwrap();
        let enabled = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        let instructions = fresh_instructions(Some("Keep this first."), None, &enabled);

        assert!(instructions.starts_with("Keep this first.\n\n## Available local skills"));
        assert!(instructions.contains("Run focused tests."));
        assert!(!instructions.contains("SECRET-BODY"));
    }

    #[test]
    fn malformed_skills_do_not_hide_healthy_skills() {
        let directory = tempdir().unwrap();
        let malformed = directory.path().join("broken");
        let healthy = directory.path().join("healthy");
        fs::create_dir(&malformed).unwrap();
        fs::create_dir(&healthy).unwrap();
        fs::write(malformed.join("SKILL.md"), "invalid").unwrap();
        fs::write(
            healthy.join("SKILL.md"),
            "---\nname: healthy\ndescription: Still available.\n---\n",
        )
        .unwrap();
        let enabled = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        let instructions = fresh_instructions(None, None, &enabled);

        assert!(instructions.contains("Still available."));
    }

    #[test]
    fn restored_catalog_is_reused_after_skills_are_disabled_or_changed() {
        let stored = "Original instructions.\n\n<!-- tact:skills-catalog:start -->\nold catalog\n<!-- tact:skills-catalog:end -->";
        let disabled = SkillsConfig::from_roots(false, Vec::new());

        let directory = tempdir().unwrap();
        let changed = directory.path().join("changed");
        fs::create_dir(&changed).unwrap();
        fs::write(
            changed.join("SKILL.md"),
            "---\nname: changed\ndescription: A changed catalog.\n---\n",
        )
        .unwrap();
        let enabled = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        assert_eq!(
            session_instructions(
                Some("Changed instructions."),
                Some("Changed appendix."),
                &disabled,
                Some(stored.to_owned())
            )
            .as_ref(),
            stored
        );
        assert_eq!(
            session_instructions(None, None, &enabled, Some(stored.to_owned())).as_ref(),
            stored
        );
    }

    #[test]
    fn restored_session_reuses_exact_instructions() {
        let directory = tempdir().unwrap();
        let skill = directory.path().join("new");
        fs::create_dir(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: new\ndescription: Must not be injected.\n---\n",
        )
        .unwrap();
        let enabled = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        assert_eq!(
            session_instructions(None, None, &enabled, Some("Old default.".to_owned())).as_ref(),
            "Old default."
        );
        assert_eq!(
            session_instructions(
                Some("Current custom."),
                Some("Current appendix."),
                &enabled,
                Some("Old custom.".to_owned())
            )
            .as_ref(),
            "Old custom."
        );
    }

    #[test]
    fn chatgpt_requests_disable_response_storage() {
        let auth = OpenAiAuth::managed_chatgpt(Arc::new(TestChatGptAuth));
        let config = ModelConfig {
            auth,
            store_responses: false,
            ..ModelConfig::default()
        };
        let profile = RequestProfile::new("session", "lineage", Arc::from([]));

        let request = serde_json::to_value(ResponseCreate::warmup(
            &config,
            Thinking::Medium,
            false,
            &profile,
            None,
        ))
        .unwrap();

        assert_eq!(request["store"], false);
    }

    #[tokio::test]
    async fn cancellation_stops_the_turn_and_waits_for_the_driver() {
        let called = Arc::new(Notify::new());
        let service_called = Arc::clone(&called);
        let responses = Responses::builder()
            .service(move || PendingService {
                called: Arc::clone(&service_called),
            })
            .build();
        let (agent, events) = Nanocodex::builder("test-key")
            .responses(responses)
            .build()
            .unwrap();
        let (_registry, subagent_control, subagent_updates) = crate::subagents::channel(32);
        let configured = ConfiguredAgent {
            agent,
            events,
            instructions: ModelConfig::default().system_prompt,
            subagent_updates,
            subagent_control,
        };
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            configured
                .run("keep running".to_owned(), task_shutdown, Vec::new())
                .await
        });

        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the model request should start");
        shutdown.cancel();

        timeout(Duration::from_secs(5), task)
            .await
            .expect("graceful shutdown should finish")
            .expect("the core task should not panic")
            .expect("cancellation should be a successful shutdown");
    }
}
