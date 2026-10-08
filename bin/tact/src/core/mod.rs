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
pub(crate) mod session;
pub(crate) mod shell;
pub(crate) mod storage;
pub(crate) mod subagent_roster;
pub(crate) mod subagent_updates;
pub(crate) mod transcript;
pub(crate) mod worker;

pub(crate) use instructions::{IMAGE_RENDERING_INSTRUCTIONS, MEMORY_REVIEW_CHECKPOINT};

use crate::{
    app::{
        config::{Config, ReasoningEffort, ReasoningMode, Speed},
        error::{AuthError, ConfigError, Result, RuntimeError, SecretError},
        hook,
        secret::SecretString,
    },
    core::{
        extensions::{
            CurrentSessionTool, Skill, mcp_provider,
            sessions::{FindSessionsTool, ReadSessionTool},
        },
        instructions::{AgentInstructions, RestoredInstructions},
        session::{AgentSnapshot, ResumeState},
    },
};
use nanocodex::{
    AgentEvents, HarnessModel as Model, Nanocodex, NanocodexError, OpenAi, Tools, TurnControl,
    agent::session::SessionId, claude::ClaudeClient,
};
#[cfg(feature = "harbor-evals")]
use orchestration::{OrchestrationRecorder, RunOutcome};
use reqwest::header::{HeaderMap, HeaderValue};
use std::{
    io,
    io::Write,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
};
use tact_memory::{
    MemoryTool, MutationAuthorizer, RemoteMemoryClient, RemoteToken, SelectedMemoryStore,
};
use tact_subagents::{
    AgentContext, RootAgentAuthority, ScopedAgentUpdate, Subagents, WeakSubagents,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const RESPONSE_MAX_ATTEMPTS: NonZeroU32 = NonZeroU32::new(2_000).unwrap();

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

enum Cancellation {
    NotRequested,
    Requested,
    Failed(NanocodexError),
}

pub(crate) fn supported_reasoning_mode(model: Model, preferred: ReasoningMode) -> ReasoningMode {
    if matches!(model, Model::Codex(model) if model.supports_reasoning_mode(preferred.into())) {
        preferred
    } else {
        ReasoningMode::Standard
    }
}

impl ConfiguredAgent {
    pub(crate) async fn run_from_config(
        config: &Config,
        model: Model,
        prompt: String,
        shutdown: CancellationToken,
        #[cfg(feature = "harbor-evals")] orchestration_log: Option<PathBuf>,
    ) -> Result<()> {
        let reasoning_mode = supported_reasoning_mode(model, config.agent().reasoning_mode());
        let result =
            Self::from_config_with_model(config, config.agent().thinking(), reasoning_mode, model)?
                .run(
                    prompt,
                    shutdown,
                    io::stdout(),
                    #[cfg(feature = "harbor-evals")]
                    orchestration_log,
                )
                .await;
        if let Some(command) = config.agent().completion_hook() {
            drop(hook::execute(command, config.agent().workspace()).await);
        }
        result
    }

    pub(crate) fn from_config_with_model(
        config: &Config,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        model: Model,
    ) -> Result<Self> {
        Self::from_config_with_session_and_model(
            config,
            thinking,
            reasoning_mode,
            model,
            None,
            None,
        )
    }

    pub(crate) fn from_config_with_session(
        config: &Config,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        model: Model,
        session_id: Option<&str>,
        resume: Option<ResumeState>,
    ) -> Result<Self> {
        Self::from_config_with_session_and_model(
            config,
            thinking,
            reasoning_mode,
            model,
            session_id,
            resume,
        )
    }

    fn from_config_with_session_and_model(
        config: &Config,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        model: Model,
        session_id: Option<&str>,
        resume: Option<ResumeState>,
    ) -> Result<Self> {
        crate::app::model::parse(model.as_str()).map_err(NanocodexError::InvalidRequest)?;
        let agent_config = config.agent();
        let workspace = Self::resolve_workspace(agent_config.workspace())?;
        if matches!(model, Model::Claude(_)) && !config.claude().enabled() {
            return Err(NanocodexError::InvalidRequest(
                "Claude requires [claude] enabled = true".into(),
            )
            .into());
        }
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
        let (agent, events) = recipe.build(
            AgentContext {
                model,
                thinking: thinking.into(),
            },
            reasoning_mode,
            agent_config.speed(),
            Arc::clone(&instructions),
            session_id,
            snapshot,
        )?;
        subagent_control.set_agent_factory(
            thinking.into(),
            agent_config.speed(),
            move |model, thinking, speed| {
                recipe.build(
                    AgentContext { model, thinking },
                    supported_reasoning_mode(model, reasoning_mode),
                    speed,
                    prompts.for_model(model),
                    None,
                    None,
                )
            },
        )?;
        Ok(Self {
            workspace,
            agent,
            context: AgentContext {
                model,
                thinking: thinking.into(),
            },
            events,
            instructions,
            skills,
            memory_enabled,
            subagent_updates,
            subagent_control,
        })
    }

    async fn run(
        mut self,
        prompt: String,
        shutdown: CancellationToken,
        mut output: impl Write,
        #[cfg(feature = "harbor-evals")] orchestration_log: Option<PathBuf>,
    ) -> Result<()> {
        let (_unused_sender, empty_updates) = mpsc::unbounded_channel();
        let subagent_updates = std::mem::replace(&mut self.subagent_updates, empty_updates);
        #[cfg(feature = "harbor-evals")]
        let recorder = OrchestrationRecorder::start(subagent_updates, orchestration_log)?;
        #[cfg(not(feature = "harbor-evals"))]
        let mut subagent_updates = subagent_updates;
        #[cfg(not(feature = "harbor-evals"))]
        let subagent_drain =
            tokio::spawn(async move { while subagent_updates.recv().await.is_some() {} });
        let root_session_id = self.agent.session_id().to_string();
        if shutdown.is_cancelled() {
            let shutdown_result = self.shutdown().await;
            #[cfg(feature = "harbor-evals")]
            recorder
                .finish(&root_session_id, RunOutcome::Cancelled)
                .await?;
            #[cfg(not(feature = "harbor-evals"))]
            subagent_drain.abort();
            shutdown_result?;
            return Ok(());
        }

        let turn = match self.agent.prompt(self.context.prompt(prompt)).await {
            Ok(turn) => turn,
            Err(error) => {
                let shutdown_result = self.shutdown().await;
                #[cfg(feature = "harbor-evals")]
                recorder
                    .finish(&root_session_id, RunOutcome::Failed)
                    .await?;
                #[cfg(not(feature = "harbor-evals"))]
                subagent_drain.abort();
                shutdown_result?;
                return Err(error.into());
            }
        };
        let control = turn.control();
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

        let turn_result = turn.await;
        let was_cancelled = matches!(cancellation, Cancellation::Requested);
        drop(control);
        self.subagent_control.close_all(&root_session_id).await;
        let shutdown_result = self.shutdown().await;
        #[cfg(feature = "harbor-evals")]
        {
            let outcome = if was_cancelled {
                RunOutcome::Cancelled
            } else if event_result.is_err() || turn_result.is_err() {
                RunOutcome::Failed
            } else {
                RunOutcome::Completed
            };
            recorder.finish(&root_session_id, outcome).await?;
        }
        #[cfg(not(feature = "harbor-evals"))]
        subagent_drain.abort();

        event_result?;
        if let Cancellation::Failed(error) = cancellation {
            return Err(error.into());
        }
        match turn_result {
            Err(NanocodexError::TurnCancelled) if was_cancelled => {}
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        shutdown_result?;
        Ok(())
    }

    async fn shutdown(mut self) -> nanocodex::agent::Result<()> {
        let result = self.agent.shutdown().await;
        drop(self.agent);
        while self.events.recv().await.is_some() {}
        result
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

struct AgentRecipe {
    config: Config,
    workspace: PathBuf,
    tools: Tools,
    memory: Option<SelectedMemoryStore>,
    subagents: WeakSubagents,
}

impl AgentRecipe {
    fn build(
        self: &Arc<Self>,
        context: AgentContext,
        reasoning_mode: ReasoningMode,
        speed: Speed,
        instructions: Arc<str>,
        session_id: Option<&str>,
        snapshot: Option<AgentSnapshot>,
    ) -> nanocodex::agent::Result<(Nanocodex, AgentEvents)> {
        let AgentContext { model, thinking } = context;
        let speed = speed.for_model(model);
        if let Some(snapshot) = &snapshot {
            snapshot.validate_identity(model, session_id)?;
        }
        let config = &self.config;
        let agent = config.agent();
        let tools = self.tools.clone();
        let subagents = self.subagents.clone();
        let memory = self.memory.clone();
        let subagents_enabled = config.subagents().enabled();
        let session_config_path = config.path().to_path_buf();
        let tool_factory = move || {
            install_agent_tools(
                tools.clone(),
                &subagents,
                memory.clone(),
                subagents_enabled,
                session_config_path.clone(),
            )
        };
        if let Model::Codex(codex_model) = model {
            let auth = config
                .auth()
                .load()
                .map_err(|error| NanocodexError::InvalidRequest(error.to_string()))?;
            let mut openai = OpenAi::builder(auth)
                .max_attempts(RESPONSE_MAX_ATTEMPTS)
                .transport(agent.transport().into());
            if let Some(url) = agent.websocket_url() {
                openai = openai.websocket_url(url);
            }
            if let Some(url) = agent.api_base_url() {
                openai = openai.api_base_url(url);
            }
            let openai = openai
                .build()
                .map_err(|error| NanocodexError::InvalidRequest(error.to_string()))?;
            let mut builder = Nanocodex::builder(openai)
                .model(codex_model)
                .workspace(&self.workspace)
                .thinking(thinking)
                .reasoning_mode(reasoning_mode.into())
                .instructions(instructions)
                .tools_factory(move |_| tool_factory());
            if let Some(home) = config.codex_home() {
                builder = builder.codex_home(home);
            }
            if let Some(id) = session_id {
                builder = builder.session_id(
                    id.parse::<SessionId>()
                        .map_err(|error| NanocodexError::InvalidRequest(error.to_string()))?,
                );
            }
            if let Some(snapshot) = snapshot {
                builder = builder.resume(snapshot.into_codex()?);
            }
            builder.service_tier(speed.into()).build()
        } else {
            if !config.claude().enabled() {
                return Err(NanocodexError::InvalidRequest(
                    "Claude requires [claude] enabled = true".into(),
                ));
            }
            let client =
                self.claude_client(|| SecretString::from_environment("ANTHROPIC_API_KEY"))?;
            let runtime = claude::tool_runtime(config, &self.workspace, &tool_factory()?)?;
            let recipe = Arc::clone(self);
            let clean_instructions = Arc::clone(&instructions);
            let spawn: claude::CleanAgentFactory = Arc::new(move |context, fast_mode| {
                recipe.build(
                    context,
                    supported_reasoning_mode(context.model, reasoning_mode),
                    if fast_mode {
                        Speed::Fast
                    } else {
                        Speed::Standard
                    },
                    Arc::clone(&clean_instructions),
                    None,
                    None,
                )
            });
            claude::build_client(
                client,
                context,
                &self.workspace,
                instructions,
                claude::ClaudeSession {
                    session_id,
                    snapshot,
                    fast_mode: speed != Speed::Standard,
                },
                runtime,
                Some(spawn),
            )
        }
    }

    fn claude_client(
        &self,
        read_key: impl FnOnce() -> std::result::Result<Option<SecretString>, SecretError>,
    ) -> nanocodex::agent::Result<ClaudeClient> {
        let config = self.config.claude();
        config
            .ensure_enabled()
            .map_err(|error| NanocodexError::InvalidRequest(error.to_string()))?;
        let endpoint = config
            .api_base_url()
            .map(|base| format!("{}/messages", base.trim_end_matches('/')));
        let key = config
            .resolve_api_key(read_key)
            .map_err(|error| NanocodexError::InvalidRequest(error.to_string()))?
            .ok_or_else(|| {
                NanocodexError::InvalidRequest(AuthError::ClaudeApiKeyUnavailable.to_string())
            })?;
        let mut headers = HeaderMap::new();
        if let Some(workspace_id) = config.workspace_id() {
            let value = HeaderValue::from_str(workspace_id).map_err(|_| {
                NanocodexError::InvalidRequest(
                    "claude.workspace_id is not a valid HTTP header value".into(),
                )
            })?;
            headers.insert("anthropic-workspace-id", value);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|error| NanocodexError::InvalidRequest(error.to_string()))?;
        // The native client owns a non-zeroizing copy after this boundary.
        Ok(match endpoint {
            Some(endpoint) => ClaudeClient::new(http, endpoint, key.key().expose_secret()),
            None => ClaudeClient::official(http, key.key().expose_secret()),
        })
    }
}

#[derive(Clone)]
struct RootMemoryAuthorizer(RootAgentAuthority);

#[nanocodex::tools::contract::async_trait]
impl MutationAuthorizer for RootMemoryAuthorizer {
    async fn authorize_memory_mutation(&self, session_id: &str) -> std::io::Result<()> {
        self.0
            .require_root(session_id)
            .await
            .map_err(std::io::Error::other)
    }
}

fn install_agent_tools(
    tools: Tools,
    subagents: &WeakSubagents,
    memory: Option<SelectedMemoryStore>,
    subagents_enabled: bool,
    session_config_path: PathBuf,
) -> std::result::Result<Tools, nanocodex::tools::ToolsBuildError> {
    let mut tools = tools
        .into_builder()
        .tool(CurrentSessionTool)
        .tool(FindSessionsTool::new(session_config_path.clone()))
        .tool(ReadSessionTool::new(session_config_path));
    if let Some(store) = memory {
        tools = tools.tool(MemoryTool::new(
            store,
            RootMemoryAuthorizer(subagents.root_agent_authority()),
        ));
    }
    let tools = if subagents_enabled {
        subagents.install_tools(tools)
    } else {
        tools
    };
    tools.build()
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
    use super::{ConfiguredAgent, configured_memory_store};
    use crate::app::{
        config::{Config, ConfigOverrides},
        error::{Error, RuntimeError},
    };
    use nanocodex::{
        HarnessModel as Model, Model as CodexModel, Nanocodex, OpenAi,
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
    fn claude_requires_opt_in_and_api_key() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        use super::AgentRecipe;
        use crate::app::secret::SecretString;
        use nanocodex::Tools;
        use std::cell::Cell;
        use tact_subagents::Subagents;

        for enabled in [false, true] {
            let directory = tempdir().unwrap();
            let config_path = directory.path().join("config.toml");
            fs::write(&config_path, format!("[claude]\nenabled = {enabled}\n")).unwrap();
            let config = Config::load(ConfigOverrides {
                path: Some(config_path),
                model: Some(Model::Codex(CodexModel::Sol)),
                ..Default::default()
            })
            .unwrap();
            let (subagents, _) = Subagents::new(1);
            let recipe = AgentRecipe {
                config,
                workspace: directory.path().to_path_buf(),
                tools: Tools::builder()
                    .web_search(false)
                    .image_generation(false)
                    .build()
                    .unwrap(),
                memory: None,
                subagents: subagents.downgrade(),
            };
            let reads = Cell::new(0);
            let result = recipe.claude_client(|| {
                reads.set(reads.get() + 1);
                Ok(None)
            });
            assert_eq!(reads.get(), usize::from(enabled));
            assert!(result.is_err());
            if enabled {
                let token = "sk-ant-oat01-subscription-sentinel";
                let rejected = recipe
                    .claude_client(|| Ok(Some(SecretString::new(token.into()))))
                    .err()
                    .expect(
                        "subscription credentials must be rejected before constructing a client",
                    );
                assert!(!rejected.to_string().contains(token));
                assert!(
                    result
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("ANTHROPIC_API_KEY")
                );
                assert!(
                    recipe
                        .claude_client(|| Ok(Some(SecretString::new(
                            "sk-ant-api03-synthetic-api-key".into()
                        ))))
                        .is_ok()
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;

                    let path = recipe.config.path();
                    fs::write(
                        path,
                        "[claude]\nenabled = true\napi_key = 'sk-ant-api03-configured-sentinel'\n",
                    )
                    .unwrap();
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
                    let config = Config::load(ConfigOverrides {
                        path: Some(path.to_path_buf()),
                        ..Default::default()
                    })
                    .unwrap();
                    let recipe = AgentRecipe { config, ..recipe };
                    assert!(recipe.claude_client(|| Ok(None)).is_ok());
                }
            }
        }
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
