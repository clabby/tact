//! Construction of individual agents for a session tree.
//!
//! An [`AgentRecipe`] holds everything that stays fixed while a session tree runs: the
//! configuration snapshot, canonical workspace, base tools, memory store, and the subagent
//! registry. The root agent and every subagent are built from the same recipe so they share
//! tool wiring and provider credentials, while each receives its own [`AgentSpec`].
//!
//! Construction failures are typed as [`AgentBuildError`]. They become Nanocodex request errors
//! only where Nanocodex's agent-factory signature requires its own error type.

use super::{
    claude,
    extensions::{
        CurrentSessionTool,
        sessions::{FindSessionsTool, ReadSessionTool},
    },
    session::AgentSnapshot,
    supported_reasoning_mode,
};
use crate::app::{
    config::{Config, ReasoningMode, Speed},
    error::{AuthError, ConfigError, SecretError},
    secret::SecretString,
};
use nanocodex::{
    AgentEvents, HarnessModel as Model, Model as CodexModel, Nanocodex, NanocodexError, OpenAi,
    Tools,
    agent::session::SessionId,
    claude::ClaudeClient,
    oai::{OpenAiError, session::SessionIdError},
    tools::ToolsBuildError,
};
use reqwest::header::{HeaderMap, HeaderValue, InvalidHeaderValue};
use std::{num::NonZeroU32, path::PathBuf, sync::Arc};
use tact_memory::{MemoryTool, MutationAuthorizer, SelectedMemoryStore};
use tact_subagents::{AgentContext, RootAgentAuthority, WeakSubagents};
use thiserror::Error;

const RESPONSE_MAX_ATTEMPTS: NonZeroU32 = NonZeroU32::new(2_000).unwrap();

/// Why an agent could not be constructed.
#[derive(Debug, Error)]
pub(super) enum AgentBuildError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    OpenAi(#[from] OpenAiError),
    #[error(transparent)]
    SessionId(#[from] SessionIdError),
    #[error("claude.workspace_id is not a valid HTTP header value")]
    ClaudeWorkspaceId(#[source] InvalidHeaderValue),
    #[error(transparent)]
    HttpClient(#[from] reqwest::Error),
}

impl From<AgentBuildError> for NanocodexError {
    fn from(error: AgentBuildError) -> Self {
        Self::InvalidRequest(error.to_string())
    }
}

/// The per-agent half of construction: which model runs, under which prompt, and whether it
/// resumes an existing session.
pub(super) struct AgentSpec<'a> {
    pub(super) context: AgentContext,
    pub(super) reasoning_mode: ReasoningMode,
    pub(super) speed: Speed,
    pub(super) instructions: Arc<str>,
    pub(super) session_id: Option<&'a str>,
    pub(super) snapshot: Option<AgentSnapshot>,
}

impl AgentSpec<'static> {
    /// A brand-new agent, as spawned for subagents. Pro reasoning is kept only on models that
    /// support it.
    pub(super) fn clean(
        context: AgentContext,
        reasoning_mode: ReasoningMode,
        speed: Speed,
        instructions: Arc<str>,
    ) -> Self {
        Self {
            context,
            reasoning_mode: supported_reasoning_mode(context.model, reasoning_mode),
            speed,
            instructions,
            session_id: None,
            snapshot: None,
        }
    }
}

pub(super) struct AgentRecipe {
    pub(super) config: Config,
    pub(super) workspace: PathBuf,
    pub(super) tools: Tools,
    pub(super) memory: Option<SelectedMemoryStore>,
    pub(super) subagents: WeakSubagents,
}

impl AgentRecipe {
    pub(super) fn build(
        self: &Arc<Self>,
        spec: AgentSpec<'_>,
    ) -> nanocodex::agent::Result<(Nanocodex, AgentEvents)> {
        if let Some(snapshot) = &spec.snapshot {
            snapshot.validate_identity(spec.context.model, spec.session_id)?;
        }
        match spec.context.model {
            Model::Codex(model) => self.build_codex(model, spec),
            Model::Claude(_) => self.build_claude(spec),
        }
    }

    fn build_codex(
        &self,
        model: CodexModel,
        spec: AgentSpec<'_>,
    ) -> nanocodex::agent::Result<(Nanocodex, AgentEvents)> {
        let agent = self.config.agent();
        let auth = self.config.auth().load().map_err(AgentBuildError::from)?;
        let mut openai = OpenAi::builder(auth)
            .max_attempts(RESPONSE_MAX_ATTEMPTS)
            .transport(agent.transport().into());
        if let Some(url) = agent.websocket_url() {
            openai = openai.websocket_url(url);
        }
        if let Some(url) = agent.api_base_url() {
            openai = openai.api_base_url(url);
        }
        let openai = openai.build().map_err(AgentBuildError::from)?;
        let tools = self.tools_factory();
        let mut builder = Nanocodex::builder(openai)
            .model(model)
            .workspace(&self.workspace)
            .thinking(spec.context.thinking)
            .reasoning_mode(spec.reasoning_mode.into())
            .instructions(spec.instructions)
            .tools_factory(move |_| tools());
        if let Some(home) = self.config.codex_home() {
            builder = builder.codex_home(home);
        }
        if let Some(id) = spec.session_id {
            builder = builder.session_id(id.parse::<SessionId>().map_err(AgentBuildError::from)?);
        }
        if let Some(snapshot) = spec.snapshot {
            builder = builder.resume(snapshot.into_codex()?);
        }
        builder
            .service_tier(spec.speed.for_model(spec.context.model).into())
            .build()
    }

    fn build_claude(
        self: &Arc<Self>,
        spec: AgentSpec<'_>,
    ) -> nanocodex::agent::Result<(Nanocodex, AgentEvents)> {
        let client = self.claude_client(|| SecretString::from_environment("ANTHROPIC_API_KEY"))?;
        let runtime =
            claude::tool_runtime(&self.config, &self.workspace, &self.tools_factory()()?)?;
        let recipe = Arc::clone(self);
        let reasoning_mode = spec.reasoning_mode;
        let clean_instructions = Arc::clone(&spec.instructions);
        let spawn: claude::CleanAgentFactory = Arc::new(move |context, fast_mode| {
            let speed = if fast_mode {
                Speed::Fast
            } else {
                Speed::Standard
            };
            recipe.build(AgentSpec::clean(
                context,
                reasoning_mode,
                speed,
                Arc::clone(&clean_instructions),
            ))
        });
        claude::build_client(
            client,
            spec.context,
            &self.workspace,
            spec.instructions,
            claude::ClaudeSession {
                session_id: spec.session_id,
                snapshot: spec.snapshot,
                fast_mode: spec.speed.for_model(spec.context.model) != Speed::Standard,
            },
            runtime,
            Some(spawn),
        )
    }

    /// Builds each agent's tool set: the base tools plus session, memory, and (when enabled)
    /// subagent tools bound to this session tree.
    pub(super) fn tools_factory(
        &self,
    ) -> impl Fn() -> Result<Tools, ToolsBuildError> + Send + Sync + 'static {
        let tools = self.tools.clone();
        let subagents = self.subagents.clone();
        let memory = self.memory.clone();
        let subagents_enabled = self.config.subagents().enabled();
        let config_path = self.config.path().to_path_buf();
        move || {
            let mut builder = tools
                .clone()
                .into_builder()
                .tool(CurrentSessionTool)
                .tool(FindSessionsTool::new(config_path.clone()))
                .tool(ReadSessionTool::new(config_path.clone()));
            if let Some(store) = memory.clone() {
                builder = builder.tool(MemoryTool::new(
                    store,
                    RootMemoryAuthorizer(subagents.root_agent_authority()),
                ));
            }
            if subagents_enabled {
                builder = subagents.install_tools(builder);
            }
            builder.build()
        }
    }

    pub(super) fn claude_client(
        &self,
        read_key: impl FnOnce() -> Result<Option<SecretString>, SecretError>,
    ) -> Result<ClaudeClient, AgentBuildError> {
        let config = self.config.claude();
        if !config.enabled() {
            return Err(ConfigError::ClaudeDisabled.into());
        }
        let endpoint = config
            .api_base_url()
            .map(|base| format!("{}/messages", base.trim_end_matches('/')));
        let key = config
            .resolve_api_key(read_key)?
            .ok_or(AuthError::ClaudeApiKeyUnavailable)?;
        let mut headers = HeaderMap::new();
        if let Some(workspace_id) = config.workspace_id() {
            let value =
                HeaderValue::from_str(workspace_id).map_err(AgentBuildError::ClaudeWorkspaceId)?;
            headers.insert("anthropic-workspace-id", value);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()?;
        // The native client owns a non-zeroizing copy after this boundary.
        Ok(match endpoint {
            Some(endpoint) => ClaudeClient::new(http, endpoint, key.key().expose_secret()),
            None => ClaudeClient::official(http, key.key().expose_secret()),
        })
    }
}

/// Restricts memory mutations to the root agent of the session tree.
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

#[cfg(test)]
mod tests {
    use super::{AgentBuildError, AgentRecipe};
    use crate::app::{
        config::{Config, ConfigOverrides},
        error::{AuthError, ConfigError},
        secret::SecretString,
    };
    use nanocodex::{HarnessModel as Model, Model as CodexModel, Tools};
    use std::{cell::Cell, fs, path::Path};
    use tact_subagents::Subagents;
    use tempfile::tempdir;

    fn recipe(config_path: &Path, contents: &str) -> AgentRecipe {
        fs::write(config_path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(config_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = Config::load(ConfigOverrides {
            path: Some(config_path.to_path_buf()),
            model: Some(Model::Codex(CodexModel::Sol)),
            ..Default::default()
        })
        .unwrap();
        let (subagents, _) = Subagents::new(1);
        AgentRecipe {
            workspace: config_path.parent().unwrap().to_path_buf(),
            config,
            tools: Tools::builder()
                .web_search(false)
                .image_generation(false)
                .build()
                .unwrap(),
            memory: None,
            subagents: subagents.downgrade(),
        }
    }

    #[test]
    fn claude_requires_opt_in_and_an_api_key() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");

        let disabled = recipe(&config_path, "[claude]\nenabled = false\n");
        let reads = Cell::new(0);
        let result = disabled.claude_client(|| {
            reads.set(reads.get() + 1);
            Ok(None)
        });
        assert_eq!(reads.get(), 0);
        assert!(matches!(
            result,
            Err(AgentBuildError::Config(ConfigError::ClaudeDisabled))
        ));

        let enabled = recipe(&config_path, "[claude]\nenabled = true\n");
        assert!(matches!(
            enabled.claude_client(|| Ok(None)),
            Err(AgentBuildError::Auth(AuthError::ClaudeApiKeyUnavailable))
        ));
        let token = "sk-ant-oat01-subscription-sentinel";
        let rejected = enabled
            .claude_client(|| Ok(Some(SecretString::new(token.into()))))
            .err()
            .expect("subscription credentials must be rejected before constructing a client");
        assert!(matches!(
            rejected,
            AgentBuildError::Auth(AuthError::InvalidClaudeApiKey)
        ));
        assert!(!rejected.to_string().contains(token));
        assert!(
            enabled
                .claude_client(|| Ok(Some(SecretString::new(
                    "sk-ant-api03-synthetic-api-key".into()
                ))))
                .is_ok()
        );

        #[cfg(unix)]
        {
            let configured = recipe(
                &config_path,
                "[claude]\nenabled = true\napi_key = 'sk-ant-api03-configured-sentinel'\n",
            );
            assert!(configured.claude_client(|| Ok(None)).is_ok());
        }
    }
}
