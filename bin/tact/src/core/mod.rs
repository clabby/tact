//! The front-end-neutral session runtime.
//!
//! This module constructs agents, schedules their turns on the worker, persists sessions and
//! their transcripts, and projects transcripts into structured entries. The terminal (`tui`) and
//! browser (`web`) front-ends depend on it; it depends on neither of them.

pub(crate) mod agent_events;
mod claude;
mod claude_context;
pub(crate) mod context;
pub(crate) mod extensions;
mod headless;
mod instructions;
#[cfg(test)]
mod mixed_provider_tests;
#[cfg(test)]
mod openai_tests;
#[cfg(feature = "harbor-evals")]
mod orchestration;
pub(crate) mod pane;
pub(crate) mod prompt;
pub(crate) mod protocol;
mod recipe;
pub(crate) mod session;
pub(crate) mod shell;
pub(crate) mod storage;
pub(crate) mod subagent_updates;
pub(crate) mod transcript;
pub(crate) mod worker;

use crate::{
    app::{
        config::{Config, ReasoningEffort, ReasoningMode, Speed},
        error::{ConfigError, Result, RuntimeError},
    },
    core::{
        extensions::{Skill, mcp_provider},
        instructions::{AgentInstructions, RestoredInstructions},
        recipe::{AgentRecipe, AgentSpec},
        session::ResumeState,
    },
};
pub(crate) use instructions::{IMAGE_RENDERING_INSTRUCTIONS, MEMORY_REVIEW_CHECKPOINT};
use nanocodex::{AgentEvents, HarnessModel as Model, Nanocodex, NanocodexError, Tools};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tact_memory::{RemoteMemoryClient, RemoteToken, SelectedMemoryStore};
use tact_subagents::{AgentContext, ScopedAgentUpdate, Subagents};
use tokio::sync::mpsc;

/// A root agent built from the configuration, with everything a front-end needs to drive it.
///
/// The front-end owns the agent and its event stream, persists the session using
/// `instructions` (the exact system prompt to restore later), offers `skills` for completion,
/// and forwards `subagent_updates` from the session tree that `subagent_control` manages.
pub(crate) struct ConfiguredAgent {
    pub(crate) workspace: PathBuf,
    pub(crate) agent: Nanocodex,
    pub(crate) context: AgentContext,
    pub(crate) events: AgentEvents,
    pub(crate) instructions: Arc<str>,
    pub(crate) skills: Arc<[Skill]>,
    pub(crate) memory_enabled: bool,
    pub(crate) subagent_updates: mpsc::UnboundedReceiver<ScopedAgentUpdate>,
    pub(crate) subagent_control: Subagents,
}

pub(crate) fn supported_reasoning_mode(model: Model, preferred: ReasoningMode) -> ReasoningMode {
    if matches!(model, Model::Codex(model) if model.supports_reasoning_mode(preferred.into())) {
        preferred
    } else {
        ReasoningMode::Standard
    }
}

impl ConfiguredAgent {
    pub(crate) fn from_config_with_model(
        config: &Config,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        model: Model,
    ) -> Result<Self> {
        Self::from_config_with_session(config, thinking, reasoning_mode, model, None, None)
    }

    /// Builds the root agent and its subagent registry. A `session_id` without `resume` starts
    /// a new session under that identifier; `resume` restores the persisted conversation and
    /// system prompt.
    pub(crate) fn from_config_with_session(
        config: &Config,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        model: Model,
        session_id: Option<&str>,
        resume: Option<ResumeState>,
    ) -> Result<Self> {
        crate::app::model::parse(model.as_str()).map_err(NanocodexError::InvalidRequest)?;
        config.claude().ensure_model_enabled(model)?;
        let agent_config = config.agent();
        let workspace = Self::resolve_workspace(agent_config.workspace())?;
        let mut tools = Tools::builder()
            .web_search(agent_config.web_search())
            .image_generation(agent_config.image_generation());
        if let Some(mcp) = mcp_provider(config)? {
            tools = tools.provider(mcp);
        }
        let tools = tools.build().map_err(NanocodexError::from)?;
        let memory = configured_memory_store(config, config.memory_workspace())?;
        let memory_enabled = memory.is_some();
        let (subagent_control, subagent_updates) = Subagents::new(agent_config.max_subagents());
        subagent_control.set_claude_enabled(config.claude().enabled());
        let recipe = Arc::new(AgentRecipe {
            config: config.clone(),
            workspace: workspace.clone(),
            tools,
            memory,
            subagents: subagent_control.downgrade(),
        });
        let (snapshot, restored_instructions) = resume.map(ResumeState::into_parts).map_or(
            (None, None),
            |(snapshot, instructions, has_skill_catalog)| {
                (
                    Some(snapshot),
                    Some(RestoredInstructions::new(instructions, has_skill_catalog)),
                )
            },
        );
        let prompts =
            AgentInstructions::from_config(config, model, restored_instructions, memory_enabled)?;
        let instructions = Arc::clone(&prompts.session.text);
        let skills = Arc::clone(&prompts.session.skills);
        let context = AgentContext::new(model, thinking.into(), reasoning_mode.into());
        let (agent, events) = recipe.build(AgentSpec {
            context,
            speed: agent_config.speed(),
            instructions: Arc::clone(&instructions),
            session_id,
            snapshot,
        })?;
        subagent_control.set_agent_factory(
            context.thinking,
            context.reasoning_mode,
            agent_config.speed(),
            move |context, speed| {
                recipe.build(AgentSpec::clean(
                    context,
                    speed,
                    prompts.for_model(context.model),
                ))
            },
        )?;
        Ok(Self {
            workspace,
            agent,
            context,
            events,
            instructions,
            skills,
            memory_enabled,
            subagent_updates,
            subagent_control,
        })
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

pub(crate) async fn set_speed(
    agent: &Nanocodex,
    model: Model,
    speed: Speed,
) -> nanocodex::agent::Result<()> {
    let speed = speed.for_model(model);
    match model {
        Model::Codex(_) => agent.set_service_tier(speed.into()).await,
        Model::Claude(_) => agent.set_fast_mode(speed != Speed::Standard).await,
    }
}

pub(crate) fn configured_memory_store(
    config: &Config,
    workspace: &Path,
) -> Result<Option<SelectedMemoryStore>> {
    if !config.memory().enabled() {
        return Ok(None);
    }
    let store = SelectedMemoryStore::local(config.memory_path(), config.memory().local().limits());
    let Some(remote) = config.memory().remote() else {
        return Ok(Some(store));
    };
    let canonical_workspace =
        workspace
            .canonicalize()
            .map_err(|source| RuntimeError::ResolveWorkspace {
                path: workspace.to_path_buf(),
                source,
            })?;
    if !remote
        .matches_workspace(&canonical_workspace)
        .map_err(ConfigError::from)?
    {
        return Ok(Some(store));
    }
    let token =
        RemoteToken::new(remote.bearer_token().to_owned()).map_err(RuntimeError::RemoteMemory)?;
    let client = RemoteMemoryClient::new(remote.endpoint(), remote.namespace().to_owned(), token)
        .map_err(RuntimeError::RemoteMemory)?;
    Ok(Some(SelectedMemoryStore::remote(client)))
}

#[cfg(test)]
mod tests {
    use super::{ConfiguredAgent, configured_memory_store};
    use crate::app::{
        config::{Config, ConfigOverrides},
        error::{Error, RuntimeError},
    };
    use nanocodex::{
        HarnessModel as Model, Model as CodexModel, Nanocodex, OpenAi, ReasoningMode,
        oai::{
            ResponseError,
            tower::{ResponsesAttempt, ResponsesServiceConfig, ResponsesServiceResponse},
        },
    };
    use std::{
        fs,
        future::{Pending, pending},
        result::Result as StdResult,
        sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };
    use tact_memory::{MemoryError, MemorySource, MemoryStore};
    use tempfile::tempdir;
    use tokio::{sync::Notify, time::timeout};
    use tokio_util::sync::CancellationToken;
    use tower::Service;

    #[derive(Clone)]
    struct PendingService {
        called: Arc<Notify>,
    }

    #[test]
    fn configured_memory_store_selects_one_backend_without_environment_lookup() {
        let directory = tempdir().unwrap();
        let allowed = directory.path().join("allowed");
        let outside = directory.path().join("outside");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&outside).unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[memory]\nenabled = true\n[memory.remote]\nendpoint = \"http://127.0.0.1:1/\"\nnamespace = \"personal\"\nbearer_token = \"direct-runtime-token\"\nworkspace_roots = [\"{}\"]\n",
                allowed.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            auth_file: Some(directory.path().join("auth.json")),
            workspace: Some(outside.clone()),
            ..ConfigOverrides::default()
        })
        .unwrap();

        let local = configured_memory_store(&config, &outside).unwrap().unwrap();
        assert_eq!(local.source(), MemorySource::Local);
        let remote = configured_memory_store(&config, &allowed).unwrap().unwrap();
        assert_eq!(remote.source(), MemorySource::Remote);
    }

    #[tokio::test]
    async fn configured_memory_store_applies_local_limits() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[memory]\nenabled = true\n\n[memory.local]\nmax_records = 3\nmax_record_bytes = 4\nmax_total_bytes = 10\n",
        )
        .unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            auth_file: Some(directory.path().join("auth.json")),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let store = configured_memory_store(&config, directory.path())
            .unwrap()
            .unwrap();

        assert!(matches!(
            store.put("abcde", None).await,
            Err(MemoryError::ContentTooLarge { maximum_bytes: 4 })
        ));
        for content in ["aaaa", "bbbb"] {
            store.put(content, None).await.unwrap();
        }
        assert!(matches!(
            store.put("ccc", None).await,
            Err(MemoryError::ContentCapacity { maximum_bytes: 10 })
        ));
        store.put("cc", None).await.unwrap();
        assert!(matches!(
            store.put("d", None).await,
            Err(MemoryError::RecordCapacity { maximum: 3 })
        ));
    }

    impl Service<ResponsesAttempt> for PendingService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
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

    #[tokio::test]
    async fn cancellation_stops_the_turn_and_waits_for_the_driver() {
        let called = Arc::new(Notify::new());
        let service_called = Arc::clone(&called);
        let openai = OpenAi::builder("test-key")
            .service(move || PendingService {
                called: Arc::clone(&service_called),
            })
            .build()
            .unwrap();
        let (agent, events) = Nanocodex::builder(openai).build().unwrap();
        let (subagent_control, subagent_updates) = tact_subagents::Subagents::new(32);
        let configured = ConfiguredAgent {
            workspace: std::env::current_dir().unwrap(),
            agent,
            context: tact_subagents::AgentContext {
                model: Model::Codex(CodexModel::Astra),
                thinking: nanocodex::Thinking::Low,
                reasoning_mode: ReasoningMode::Standard,
            },
            events,
            instructions: ResponsesServiceConfig::default().system_prompt().into(),
            skills: Arc::from([]),
            memory_enabled: false,
            subagent_updates,
            subagent_control,
        };
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            configured
                .run(
                    "keep running".to_owned(),
                    task_shutdown,
                    Vec::new(),
                    #[cfg(feature = "harbor-evals")]
                    None,
                )
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
