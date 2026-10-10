//! Configuration loading, precedence, and effective runtime settings.
//!
//! Each setting resolves from a CLI override, then the configuration file, then its default.
//! Relative paths given on the CLI are relative to the current directory; relative paths in the
//! file are relative to the file's directory. The file is selected by `--config`, then
//! `$TACT_HOME/config.toml`, then `~/.tact/config.toml`; only an explicitly selected file must
//! exist.
//!
//! The effective configuration renders as TOML with every user-facing key present and every
//! secret redacted, and that rendering loads back with the same meaning. A loaded configuration
//! remembers its overrides and environment so it can be reloaded from the same source.

mod agent;
mod decisions;
mod edit;
mod file;
mod mcp;
mod memory;
mod providers;
mod render;
mod settings;
mod skills;
mod subagents;
#[cfg(test)]
mod test_support;
mod tui;
mod web;

use crate::app::{
    error::{ConfigError, Result},
    theme::Theme,
};
pub(crate) use agent::AgentConfig;
pub(crate) use decisions::DecisionsConfig;
pub(crate) use edit::{ConfigDocument, Setting};
use file::ConfigFile;
pub(crate) use mcp::McpServerConfig;
pub(crate) use memory::{MemoryConfig, RemoteMemoryConfig};
use nanocodex::HarnessModel as Model;
use providers::OpenAiConfig;
pub(crate) use providers::{AuthConfig, AuthMode, ClaudeConfig};
use serde::Serialize;
pub(crate) use settings::{
    DEFAULT_MAX_SUBAGENTS, ReasoningEffort, ReasoningMode, Speed, Transport,
};
pub(crate) use skills::SkillsConfig;
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};
pub(crate) use subagents::SubagentsConfig;
pub(crate) use tui::TuiConfig;
pub(crate) use web::WebConfig;

/// Effective application configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Config {
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    codex_home: Option<PathBuf>,
    /// The process workspace selects the shared memory backend.
    #[serde(skip)]
    memory_workspace: PathBuf,
    auth: AuthConfig,
    openai: OpenAiConfig,
    claude: ClaudeConfig,
    agent: AgentConfig,
    mcp_servers: BTreeMap<String, McpServerConfig>,
    skills: SkillsConfig,
    memory: MemoryConfig,
    subagents: SubagentsConfig,
    decisions: DecisionsConfig,
    web: WebConfig,
    tui: TuiConfig,
    theme: Theme,
    #[serde(skip)]
    reload: ReloadSource,
}

/// Settings given on the command line, which take precedence over the file.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigOverrides {
    pub(crate) path: Option<PathBuf>,
    pub(crate) auth_mode: Option<AuthMode>,
    pub(crate) auth_file: Option<PathBuf>,
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) model: Option<Model>,
    pub(crate) thinking: Option<ReasoningEffort>,
    pub(crate) reasoning_mode: Option<ReasoningMode>,
    pub(crate) max_subagents: Option<usize>,
    pub(crate) web: Option<bool>,
    pub(crate) instructions: Option<String>,
    pub(crate) append_instructions: Option<String>,
    pub(crate) web_search: Option<bool>,
    pub(crate) image_generation: Option<bool>,
    pub(crate) websocket_url: Option<String>,
    pub(crate) api_base_url: Option<String>,
    pub(crate) transport: Option<Transport>,
}

/// The inputs a configuration was loaded from, kept so it can be reloaded identically.
#[derive(Clone, Debug)]
struct ReloadSource {
    overrides: ConfigOverrides,
    environment: Environment,
    current_dir: PathBuf,
}

/// Whether an explicitly selected configuration file must already exist. The default file
/// location may always be absent, in which case defaults apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MissingFile {
    /// A `--config` path that does not exist is an error.
    Explicit,
    /// The caller creates the file, so any selected path may be absent.
    Allowed,
}

/// A reloaded configuration and whether the file asked for a different workspace, which takes
/// effect only in new processes.
#[derive(Debug)]
pub(crate) struct ConfigReload {
    config: Config,
    workspace_changed: bool,
}

impl ConfigReload {
    pub(crate) fn into_parts(self) -> (Config, bool) {
        (self.config, self.workspace_changed)
    }
}

/// The process environment variables that locate configuration and credentials. Empty variables
/// are treated as unset.
#[derive(Clone, Debug, Default)]
struct Environment {
    tact_home: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl Environment {
    fn read() -> Self {
        Self {
            tact_home: Self::non_empty_var("TACT_HOME").map(PathBuf::from),
            codex_home: Self::non_empty_var("CODEX_HOME").map(PathBuf::from),
            home: Self::non_empty_var("HOME")
                .or_else(|| Self::non_empty_var("USERPROFILE"))
                .map(PathBuf::from),
        }
    }

    fn non_empty_var(name: &str) -> Option<OsString> {
        env::var_os(name).filter(|value| !value.is_empty())
    }

    fn config_path(&self, explicit: Option<PathBuf>, current_dir: &Path) -> Result<PathBuf> {
        if let Some(path) = explicit {
            return Ok(resolve_path(path, current_dir));
        }
        if let Some(home) = &self.tact_home {
            return Ok(home.join("config.toml"));
        }
        self.home
            .as_ref()
            .map(|home| home.join(".tact/config.toml"))
            .ok_or_else(|| ConfigError::ConfigHomeUnavailable.into())
    }

    fn codex_home(&self) -> Option<PathBuf> {
        self.codex_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".codex")))
    }

    fn default_auth_file(&self) -> Result<PathBuf> {
        self.codex_home()
            .map(|home| home.join("auth.json"))
            .ok_or_else(|| ConfigError::AuthHomeUnavailable.into())
    }
}

impl Config {
    pub(crate) fn load(overrides: ConfigOverrides) -> Result<Self> {
        let current_dir = env::current_dir().map_err(ConfigError::CurrentDirectory)?;
        Self::load_with(overrides, Environment::read(), &current_dir)
    }

    /// Loads configuration for a command that writes it, such as adding an MCP server. An
    /// explicitly selected file may be absent because the command creates it.
    pub(crate) fn load_for_edit(overrides: ConfigOverrides) -> Result<Self> {
        let current_dir = env::current_dir().map_err(ConfigError::CurrentDirectory)?;
        Self::load_with_options(
            overrides,
            Environment::read(),
            &current_dir,
            MissingFile::Allowed,
        )
    }

    fn load_with(
        overrides: ConfigOverrides,
        environment: Environment,
        current_dir: &Path,
    ) -> Result<Self> {
        Self::load_with_options(overrides, environment, current_dir, MissingFile::Explicit)
    }

    fn load_with_options(
        overrides: ConfigOverrides,
        environment: Environment,
        current_dir: &Path,
        missing: MissingFile,
    ) -> Result<Self> {
        let required = overrides.path.is_some() && missing == MissingFile::Explicit;
        let path = environment.config_path(overrides.path.clone(), current_dir)?;
        let config_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut file = ConfigFile::read(&path, required)?;
        file.validate_secret_permissions(&path)?;

        let auth_file = match configured_path(
            overrides.auth_file.clone(),
            file.auth.file,
            &config_dir,
            current_dir,
        ) {
            Some(path) => path,
            None => environment.default_auth_file()?,
        };
        let workspace = configured_path(
            overrides.workspace.clone(),
            file.agent.workspace.take(),
            &config_dir,
            current_dir,
        )
        .unwrap_or_else(|| current_dir.to_path_buf());
        let mcp_servers = file
            .mcp_servers
            .into_iter()
            .map(|(name, server)| {
                McpServerConfig::new(&name, server, &config_dir).map(|server| (name, server))
            })
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
        let skills = SkillsConfig::new(file.skills, &config_dir, &environment);
        let memory = MemoryConfig::new(file.memory, &config_dir).map_err(ConfigError::from)?;
        let agent = AgentConfig::resolve(&overrides, file.agent, workspace.clone());
        file.claude.ensure_model_enabled(agent.model())?;
        let decisions = DecisionsConfig::new(
            file.decisions.enabled.unwrap_or(false),
            file.openai.decisions_api_key.as_ref().map(Arc::clone),
        );

        Ok(Self {
            path,
            codex_home: environment.codex_home(),
            memory_workspace: workspace,
            auth: AuthConfig::new(
                overrides.auth_mode.or(file.auth.mode).unwrap_or_default(),
                auth_file,
                file.openai.api_key.as_ref().map(Arc::clone),
            ),
            openai: file.openai,
            claude: file.claude,
            agent,
            mcp_servers,
            skills,
            memory,
            subagents: SubagentsConfig::new(file.subagents),
            decisions,
            web: WebConfig::new(file.web, overrides.web)?,
            tui: TuiConfig::new(file.tui),
            theme: file.theme,
            reload: ReloadSource {
                overrides,
                environment,
                current_dir: current_dir.to_path_buf(),
            },
        })
    }

    /// Reloads the original source while preserving settings that cannot change safely in-process.
    pub(crate) fn reload(&self) -> Result<ConfigReload> {
        let mut config = Self::load_with(
            self.reload.overrides.clone(),
            self.reload.environment.clone(),
            &self.reload.current_dir,
        )?;
        let workspace_changed = config.agent.workspace != self.agent.workspace;
        config.agent.workspace.clone_from(&self.agent.workspace);
        config.memory_workspace.clone_from(&self.memory_workspace);
        Ok(ConfigReload {
            config,
            workspace_changed,
        })
    }

    pub(crate) fn set_thinking(&mut self, effort: ReasoningEffort) {
        self.agent.thinking = effort;
    }

    pub(crate) fn set_reasoning_mode(&mut self, mode: ReasoningMode) {
        self.agent.reasoning_mode = mode;
    }

    pub(crate) fn set_speed(&mut self, speed: Speed) {
        self.agent.speed = speed;
    }

    pub(crate) fn set_max_subagents(&mut self, limit: usize) {
        self.agent.max_subagents = limit;
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn auth(&self) -> &AuthConfig {
        &self.auth
    }

    pub(crate) fn codex_home(&self) -> Option<&Path> {
        self.codex_home.as_deref()
    }

    pub(crate) const fn claude(&self) -> &ClaudeConfig {
        &self.claude
    }

    pub(crate) fn with_workspace(&self, workspace: PathBuf) -> Self {
        let mut config = self.clone();
        config.agent.workspace = workspace;
        config
    }

    pub(crate) fn memory_workspace(&self) -> &Path {
        &self.memory_workspace
    }

    pub(crate) fn agent(&self) -> &AgentConfig {
        &self.agent
    }

    pub(crate) fn mcp_servers(&self) -> &BTreeMap<String, McpServerConfig> {
        &self.mcp_servers
    }

    pub(crate) const fn skills(&self) -> &SkillsConfig {
        &self.skills
    }

    pub(crate) const fn memory(&self) -> &MemoryConfig {
        &self.memory
    }

    pub(crate) const fn subagents(&self) -> &SubagentsConfig {
        &self.subagents
    }

    pub(crate) const fn decisions(&self) -> &DecisionsConfig {
        &self.decisions
    }

    pub(crate) const fn web(&self) -> &WebConfig {
        &self.web
    }

    pub(crate) const fn tui(&self) -> &TuiConfig {
        &self.tui
    }

    /// The local memory store, shared by every workspace that uses this configuration file.
    pub(crate) fn memory_path(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join("memory/v1.sqlite3")
    }

    pub(crate) const fn theme(&self) -> &Theme {
        &self.theme
    }

    pub(crate) fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(ConfigError::Serialize)
            .map_err(Into::into)
    }
}

/// Selects a path from the CLI, resolved against the current directory, or else from the file,
/// resolved against the configuration directory.
fn configured_path(
    cli: Option<PathBuf>,
    file: Option<PathBuf>,
    config_dir: &Path,
    current_dir: &Path,
) -> Option<PathBuf> {
    match (cli, file) {
        (Some(path), _) => Some(resolve_path(path, current_dir)),
        (None, Some(path)) => Some(resolve_path(path, config_dir)),
        (None, None) => None,
    }
}

fn resolve_path(path: PathBuf, base: &Path) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    base.join(path)
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::{
            AuthMode, Config, ConfigOverrides, Environment, ReasoningEffort, ReasoningMode, Speed,
            Transport,
            test_support::{
                assert_table_fields, load_config, load_config_at, load_default,
                load_without_environment, toml_string,
            },
        },
        error::{ConfigError, Error},
        theme::ThemeMode,
    };
    use nanocodex::{HarnessModel as Model, Model as CodexModel};
    use ratatui::style::Color;
    use std::{fs, path::Path};
    use tact_memory::MemoryLimits;
    use tempfile::tempdir;

    #[test]
    fn missing_default_file_materializes_all_defaults() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let config = load_default(
            Environment {
                home: Some(home.clone()),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert_eq!(config.path(), home.join(".tact/config.toml"));
        assert_eq!(config.auth().mode(), AuthMode::Auto);
        assert_eq!(config.auth().file(), home.join(".codex/auth.json"));
        assert_eq!(config.agent().workspace(), directory.path());
        assert_eq!(config.agent().model(), Model::Codex(CodexModel::Sol));
        assert_eq!(config.agent().thinking(), ReasoningEffort::Low);
        assert_eq!(config.agent().reasoning_mode(), ReasoningMode::Standard);
        assert_eq!(config.agent().speed(), Speed::Standard);
        assert_eq!(config.agent().max_subagents(), 32);
        assert!(config.agent().web_search());
        assert!(config.agent().image_generation());
        assert_eq!(config.tui().mouse_scroll_lines.get(), 3);
        assert_eq!(config.theme().border(), Color::DarkGray);

        let rendered_toml = config.to_toml().unwrap();
        let rendered: toml::Value = toml::from_str(&rendered_toml).unwrap();
        assert!(rendered["memory"]["remote"].is_table());
        assert_table_fields(
            &rendered,
            &[
                "auth",
                "openai",
                "claude",
                "agent",
                "mcp_servers",
                "skills",
                "memory",
                "subagents",
                "decisions",
                "web",
                "tui",
                "theme",
            ],
        );
        assert_table_fields(&rendered["auth"], &["mode", "file"]);
        assert_table_fields(&rendered["openai"], &["api_key", "decisions_api_key"]);
        assert_eq!(rendered["openai"]["api_key"].as_str(), Some(""));
        assert_eq!(rendered["openai"]["decisions_api_key"].as_str(), Some(""));
        assert_table_fields(
            &rendered["claude"],
            &["enabled", "api_key", "api_base_url", "workspace_id"],
        );
        assert_eq!(rendered["claude"]["enabled"].as_bool(), Some(false));
        assert_eq!(rendered["claude"]["api_key"].as_str(), Some(""));
        assert!(config.claude().api_key().is_none());
        assert_table_fields(
            &rendered["agent"],
            &[
                "workspace",
                "model",
                "thinking",
                "reasoning_mode",
                "speed",
                "max_subagents",
                "instructions",
                "append_instructions",
                "web_search",
                "image_generation",
                "websocket_url",
                "api_base_url",
                "transport",
                "completion_hook",
            ],
        );
        assert_table_fields(&rendered["mcp_servers"], &[]);
        assert_table_fields(&rendered["skills"], &["enabled", "roots"]);
        assert_table_fields(&rendered["memory"], &["enabled", "local", "remote"]);
        assert_table_fields(
            &rendered["memory"]["local"],
            &["max_records", "max_record_bytes", "max_total_bytes"],
        );
        assert_table_fields(
            &rendered["memory"]["remote"],
            &["endpoint", "namespace", "bearer_token", "workspace_roots"],
        );
        assert_table_fields(&rendered["subagents"], &["enabled"]);
        assert_table_fields(&rendered["decisions"], &["enabled"]);
        assert_table_fields(
            &rendered["web"],
            &[
                "enabled",
                "bind",
                "port",
                "public_url",
                "tailscale",
                "max_live_sessions",
            ],
        );
        assert_table_fields(&rendered["tui"], &["mouse_scroll_lines"]);
        assert_table_fields(&rendered["theme"], &["mode", "light", "dark"]);
        let palette_fields = [
            "text",
            "border",
            "muted",
            "accent",
            "code_text",
            "code_background",
            "thinking_low",
            "thinking_medium",
            "thinking_high",
            "thinking_xhigh",
            "thinking_max",
            "model_luna",
            "model_sol",
            "model_astra",
            "model_haiku",
            "model_sonnet",
            "model_opus",
            "model_fable",
        ];
        assert_table_fields(&rendered["theme"]["light"], &palette_fields);
        assert_table_fields(&rendered["theme"]["dark"], &palette_fields);
        assert_eq!(rendered["auth"]["mode"].as_str(), Some("auto"));
        assert_eq!(
            rendered["auth"]["file"].as_str(),
            home.join(".codex/auth.json").to_str()
        );
        assert_eq!(
            rendered["agent"]["model"].as_str(),
            Some(Model::Codex(CodexModel::Sol).as_str())
        );
        assert_eq!(rendered["agent"]["transport"].as_str(), Some("websocket"));
        assert_eq!(
            rendered["agent"]["workspace"].as_str(),
            directory.path().to_str()
        );
        assert_eq!(rendered["agent"]["thinking"].as_str(), Some("low"));
        assert_eq!(rendered["agent"]["speed"].as_str(), Some("standard"));
        assert_eq!(rendered["agent"]["max_subagents"].as_integer(), Some(32));
        for field in [
            "instructions",
            "append_instructions",
            "websocket_url",
            "api_base_url",
            "completion_hook",
        ] {
            assert_eq!(rendered["agent"][field].as_str(), Some(""), "{field}");
        }
        assert_eq!(
            rendered["mcp_servers"].as_table().map(|table| table.len()),
            Some(0)
        );
        assert_eq!(rendered["theme"]["mode"].as_str(), Some("auto"));
        assert_eq!(rendered["theme"]["dark"]["accent"].as_str(), Some("blue"));
        assert_eq!(
            rendered["memory"]["local"]["max_records"].as_integer(),
            Some(MemoryLimits::PRODUCTION.records as i64)
        );
        assert_eq!(
            rendered["memory"]["local"]["max_record_bytes"].as_integer(),
            Some(MemoryLimits::PRODUCTION.content_bytes as i64)
        );
        assert_eq!(
            rendered["memory"]["local"]["max_total_bytes"].as_integer(),
            Some(MemoryLimits::PRODUCTION.total_content_bytes as i64)
        );
        assert_eq!(rendered["memory"]["remote"]["endpoint"].as_str(), Some(""));
        assert_eq!(rendered["memory"]["remote"]["namespace"].as_str(), Some(""));
        assert_eq!(
            rendered["memory"]["remote"]["bearer_token"].as_str(),
            Some("")
        );
        assert_eq!(
            rendered["memory"]["remote"]["workspace_roots"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(rendered["tui"]["mouse_scroll_lines"].as_integer(), Some(3));

        let reloaded = load_config(&rendered_toml).unwrap();
        assert!(reloaded.agent().completion_hook().is_none());
        assert!(reloaded.agent().instructions().is_none());
        assert!(reloaded.agent().append_instructions().is_none());
        assert!(reloaded.agent().websocket_url().is_none());
        assert!(reloaded.agent().api_base_url().is_none());
        assert_eq!(reloaded.agent().transport(), Transport::Websocket);
        assert!(reloaded.memory().remote().is_none());
    }

    #[test]
    fn memory_path_is_global_across_workspace_changes_and_reload() {
        let directory = tempdir().unwrap();
        let config_dir = directory.path().join("settings");
        let config_path = config_dir.join("config.toml");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            &config_path,
            "[agent]\nworkspace = \"first-workspace\"\n\n[memory]\nenabled = false\n",
        )
        .unwrap();
        let config = load_config_at(config_path.clone(), directory.path()).unwrap();
        let expected_path = config_dir.join("memory/v1.sqlite3");

        assert_eq!(config.memory_path(), expected_path);
        assert_ne!(
            config.memory_path(),
            config.agent().workspace().join("memory/v1.sqlite3")
        );

        fs::write(
            &config_path,
            "[agent]\nworkspace = \"second-workspace\"\n\n[memory]\nenabled = true\n",
        )
        .unwrap();
        let (reloaded, workspace_changed) = config.reload().unwrap().into_parts();

        assert!(workspace_changed);
        assert_eq!(reloaded.agent().workspace(), config.agent().workspace());
        assert!(reloaded.memory().enabled());
        assert_eq!(reloaded.memory_path(), expected_path);
    }

    #[test]
    fn explicit_missing_file_is_an_error() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("missing.toml");
        let error = load_without_environment(path.clone(), directory.path()).unwrap_err();

        assert!(matches!(
            error,
            Error::Config(ConfigError::Read { path: error_path, .. })
                if error_path == path
        ));
    }

    #[test]
    fn config_paths_are_relative_to_the_config_file() {
        let directory = tempdir().unwrap();
        let config_dir = directory.path().join("settings");
        let config_path = config_dir.join("config.toml");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            &config_path,
            "[auth]\nmode = \"api-key\"\nfile = \"credentials/auth.json\"\n\
             \n[agent]\nworkspace = \"workspace\"\n",
        )
        .unwrap();

        let config = load_without_environment(config_path, directory.path()).unwrap();

        assert_eq!(config.auth().mode(), AuthMode::ApiKey);
        assert_eq!(
            config.auth().file(),
            config_dir.join("credentials/auth.json")
        );
        assert_eq!(config.agent().workspace(), config_dir.join("workspace"));
    }

    #[test]
    fn codex_home_uses_environment_override_or_home_default() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let default_codex_home = home.join(".codex");
        let configured_codex_home = directory.path().join("configured-codex");
        let overridden = load_default(
            Environment {
                codex_home: Some(configured_codex_home.clone()),
                home: Some(home.clone()),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();
        let defaulted = load_default(
            Environment {
                home: Some(home.clone()),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert_eq!(
            overridden.codex_home(),
            Some(configured_codex_home.as_path())
        );
        assert_eq!(defaulted.codex_home(), Some(default_codex_home.as_path()));
    }

    #[test]
    fn cli_overrides_take_precedence_and_use_the_working_directory() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[auth]\nmode = \"auto\"\nfile = \"stored.json\"\n\
             \n[agent]\nworkspace = \"configured\"\nmodel = \"astra\"\nthinking = \"low\"\nweb_search = true\n",
        )
        .unwrap();

        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                auth_mode: Some(AuthMode::ChatGpt),
                auth_file: Some("cli-auth.json".into()),
                workspace: Some("cli-workspace".into()),
                model: Some(Model::Codex(CodexModel::Luna)),
                thinking: Some(ReasoningEffort::High),
                web_search: Some(false),
                ..ConfigOverrides::default()
            },
            Environment::default(),
            directory.path(),
        )
        .unwrap();

        assert_eq!(config.auth().mode(), AuthMode::ChatGpt);
        assert_eq!(config.auth().file(), directory.path().join("cli-auth.json"));
        assert_eq!(
            config.agent().workspace(),
            directory.path().join("cli-workspace")
        );
        assert_eq!(config.agent().model(), Model::Codex(CodexModel::Luna));
        assert_eq!(config.agent().thinking(), ReasoningEffort::High);
        assert!(!config.agent().web_search());
    }

    #[test]
    fn reload_preserves_overrides_and_defers_workspace_changes() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[agent]\nworkspace = \"first\"\nthinking = \"low\"\nweb_search = true\n\
             \n[theme]\nmode = \"light\"\n",
        )
        .unwrap();
        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path.clone()),
                web_search: Some(false),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        fs::write(
            &config_path,
            "[agent]\nworkspace = \"second\"\nthinking = \"high\"\nweb_search = true\n\
             \n[theme]\nmode = \"dark\"\n",
        )
        .unwrap();
        let (reloaded, workspace_changed) = config.reload().unwrap().into_parts();

        assert!(workspace_changed);
        assert_eq!(reloaded.agent().workspace(), directory.path().join("first"));
        assert_eq!(reloaded.agent().thinking(), ReasoningEffort::High);
        assert!(!reloaded.agent().web_search());
        assert_eq!(reloaded.theme().mode(), ThemeMode::Dark);
    }

    #[test]
    fn invalid_reload_reports_the_selected_path_without_changing_the_config() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[agent]\nthinking = \"low\"\n").unwrap();
        let config = load_config_at(config_path.clone(), directory.path()).unwrap();

        fs::write(&config_path, "[agent\n").unwrap();
        let error = config.reload().unwrap_err();

        assert!(matches!(
            error,
            Error::Config(ConfigError::Parse { path, .. }) if path == config_path
        ));
        assert_eq!(config.agent().thinking(), ReasoningEffort::Low);
    }

    #[test]
    fn theme_overrides_are_loaded_and_serialized() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[theme]\ntext = \"#AABBCC\"\nborder = 239\ncode_text = \"white\"\ncode_background = \"#101010\"\nthinking_high = \"green\"\nmodel_sol = \"#123456\"\n",
        )
        .unwrap();

        let config = load_config_at(config_path, directory.path()).unwrap();

        assert_eq!(config.theme().text(), Color::Rgb(0xAA, 0xBB, 0xCC));
        assert_eq!(config.theme().border(), Color::Indexed(239));
        assert_eq!(config.theme().code_text(), Color::White);
        assert_eq!(
            config.theme().code_background(),
            Color::Rgb(0x10, 0x10, 0x10)
        );
        assert_eq!(config.theme().thinking_high(), Color::Green);
        assert_eq!(config.theme().accent(), Color::Blue);
        assert_eq!(
            config.theme().model(Model::Codex(CodexModel::Sol)),
            Color::Rgb(0x12, 0x34, 0x56)
        );

        let rendered = config.to_toml().unwrap();
        for (key, expected) in [
            ("text", "#AABBCC"),
            ("border", "239"),
            ("model_sol", "#123456"),
        ] {
            for palette in ["light", "dark"] {
                assert_eq!(toml_string(&rendered, &["theme", palette, key]), expected);
            }
        }
    }

    #[test]
    fn invalid_theme_color_is_a_config_parse_error() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[theme]\naccent = \"not-a-color\"\n").unwrap();

        let error = load_without_environment(config_path, directory.path()).unwrap_err();

        assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
    }

    #[test]
    fn tact_home_selects_the_config_path() {
        let directory = tempdir().unwrap();
        let tact_home = directory.path().join("tact-home");
        let config = load_default(
            Environment {
                tact_home: Some(tact_home.clone()),
                codex_home: Some(directory.path().join("codex-home")),
                home: None,
            },
            Path::new("/unused"),
        )
        .unwrap();

        assert_eq!(config.path(), tact_home.join("config.toml"));
    }

    #[test]
    fn missing_auth_home_has_an_actionable_error() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "").unwrap();

        let error = load_without_environment(config_path, directory.path()).unwrap_err();

        assert!(matches!(
            error,
            Error::Config(ConfigError::AuthHomeUnavailable)
        ));
    }
}
