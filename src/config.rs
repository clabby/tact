//! Configuration loading, precedence, and effective runtime settings.

use crate::{
    error::{ConfigError, Result},
    tui::theme::{Theme, ThemeMode},
};
use clap::ValueEnum;
use nanocodex::Thinking;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fmt, fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, value};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Authentication method used by `tact`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AuthMode {
    /// Prefer a stored ChatGPT session, then fall back to an API key.
    #[default]
    Auto,
    /// Require a stored ChatGPT session.
    #[serde(rename = "chatgpt")]
    #[value(name = "chatgpt")]
    ChatGpt,
    /// Require `OPENAI_API_KEY`.
    ApiKey,
}

/// Reasoning effort used by the model.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningEffort {
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
}

/// Effective application configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Config {
    #[serde(skip)]
    path: PathBuf,
    auth: AuthConfig,
    agent: AgentConfig,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    mcp_servers: BTreeMap<String, McpServerConfig>,
    skills: SkillsConfig,
    theme: Theme,
    #[serde(skip)]
    reload: ReloadSource,
}

/// Configuration for a local MCP server using the stdio transport.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpServerConfig {
    command: String,
    args: Vec<String>,
    #[serde(serialize_with = "serialize_mcp_environment")]
    env: Arc<McpEnvironment>,
    cwd: Option<PathBuf>,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct McpSecretString(String);

/// Application-owned MCP environment values.
///
/// Cloned configurations share this owner so secret bytes are not duplicated. The TOML parser's
/// input buffer is separately zeroized after loading; allocations internal to the TOML parser are
/// outside this crate's ownership.
pub(crate) struct McpEnvironment(BTreeMap<String, McpSecretString>);

/// Effective authentication configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AuthConfig {
    mode: AuthMode,
    file: PathBuf,
}

/// Effective Nanocodex configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AgentConfig {
    workspace: PathBuf,
    thinking: ReasoningEffort,
    instructions: Option<String>,
    web_search: bool,
    image_generation: bool,
    websocket_url: Option<String>,
    api_base_url: Option<String>,
}

/// Filesystem locations from which local model skills may be discovered.
///
/// Skills are disabled by default because each `SKILL.md` contains model instructions that may
/// direct shell or tool execution and adds persistent context to every model session. Missing
/// roots are ignored so standard locations do not need to exist.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct SkillsConfig {
    enabled: bool,
    roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigOverrides {
    pub(crate) path: Option<PathBuf>,
    pub(crate) auth_mode: Option<AuthMode>,
    pub(crate) auth_file: Option<PathBuf>,
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) thinking: Option<ReasoningEffort>,
    pub(crate) instructions: Option<String>,
    pub(crate) web_search: Option<bool>,
    pub(crate) image_generation: Option<bool>,
    pub(crate) websocket_url: Option<String>,
    pub(crate) api_base_url: Option<String>,
}

#[derive(Clone, Debug)]
struct ReloadSource {
    overrides: ConfigOverrides,
    environment: Environment,
    current_dir: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ConfigReload {
    config: Config,
    workspace_changed: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    auth: AuthConfigFile,
    agent: AgentConfigFile,
    mcp_servers: BTreeMap<String, McpServerConfigFile>,
    skills: SkillsConfigFile,
    theme: Theme,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SkillsConfigFile {
    enabled: bool,
    roots: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpServerConfigFile {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AuthConfigFile {
    mode: Option<AuthMode>,
    file: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AgentConfigFile {
    workspace: Option<PathBuf>,
    thinking: Option<ReasoningEffort>,
    instructions: Option<String>,
    web_search: Option<bool>,
    image_generation: Option<bool>,
    websocket_url: Option<String>,
    api_base_url: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct Environment {
    tact_home: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl Config {
    pub(crate) fn load(overrides: ConfigOverrides) -> Result<Self> {
        let current_dir = env::current_dir().map_err(ConfigError::CurrentDirectory)?;
        Self::load_with(overrides, Environment::read(), &current_dir)
    }

    fn load_with(
        overrides: ConfigOverrides,
        environment: Environment,
        current_dir: &Path,
    ) -> Result<Self> {
        let reload = ReloadSource {
            overrides: overrides.clone(),
            environment: environment.clone(),
            current_dir: current_dir.to_path_buf(),
        };
        let explicit_path = overrides.path.is_some();
        let path = Self::config_path(overrides.path, &environment, current_dir)?;
        let file = ConfigFile::read(&path, explicit_path)?;
        let auth_file = Self::auth_file_path(
            overrides.auth_file,
            file.auth.file,
            &path,
            &environment,
            current_dir,
        )?;
        let workspace = Self::configured_path(
            overrides.workspace,
            file.agent.workspace,
            &path,
            current_dir,
        )
        .unwrap_or_else(|| current_dir.to_path_buf());
        let config_dir = path.parent().unwrap_or(Path::new("."));
        let mcp_servers = file
            .mcp_servers
            .into_iter()
            .map(|(name, server)| (name, McpServerConfig::new(server, config_dir)))
            .collect();
        let skills = SkillsConfig::new(file.skills, config_dir, &environment);

        Ok(Self {
            path,
            auth: AuthConfig::new(
                overrides.auth_mode.or(file.auth.mode).unwrap_or_default(),
                auth_file,
            ),
            agent: AgentConfig {
                workspace,
                thinking: overrides
                    .thinking
                    .or(file.agent.thinking)
                    .unwrap_or_default(),
                instructions: overrides.instructions.or(file.agent.instructions),
                web_search: overrides
                    .web_search
                    .or(file.agent.web_search)
                    .unwrap_or(true),
                image_generation: overrides
                    .image_generation
                    .or(file.agent.image_generation)
                    .unwrap_or(true),
                websocket_url: overrides.websocket_url.or(file.agent.websocket_url),
                api_base_url: overrides.api_base_url.or(file.agent.api_base_url),
            },
            mcp_servers,
            skills,
            theme: file.theme,
            reload,
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
        Ok(ConfigReload {
            config,
            workspace_changed,
        })
    }

    pub(crate) fn set_thinking(&mut self, effort: ReasoningEffort) {
        self.agent.thinking = effort;
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn auth(&self) -> &AuthConfig {
        &self.auth
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

    pub(crate) const fn theme(&self) -> &Theme {
        &self.theme
    }

    pub(crate) fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(ConfigError::Serialize)
            .map_err(Into::into)
    }

    pub(crate) fn persist_thinking(&self, effort: ReasoningEffort) -> Result<()> {
        Self::persist_thinking_at(&self.path, effort)
    }

    pub(crate) fn persist_theme_mode(&self, mode: ThemeMode) -> Result<()> {
        Self::persist_setting(&self.path, "theme", "mode", mode.as_str())
    }

    fn persist_thinking_at(path: &Path, effort: ReasoningEffort) -> Result<()> {
        Self::persist_setting(path, "agent", "thinking", effort.as_str())
    }

    fn persist_setting(path: &Path, section: &str, key: &str, setting: &str) -> Result<()> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                }
                .into());
            }
        };
        let mut document =
            contents
                .parse::<DocumentMut>()
                .map_err(|source| ConfigError::UpdateParse {
                    path: path.to_path_buf(),
                    source,
                })?;
        if !document.contains_key(section) {
            document[section] = Item::Table(Table::new());
        }
        document[section][key] = value(setting);
        let parent = path.parent().ok_or_else(|| ConfigError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::other("configuration path has no parent directory"),
        })?;
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;

        let mut temporary = NamedTempFile::new_in(parent).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        temporary
            .write_all(document.to_string().as_bytes())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        temporary
            .persist(path)
            .map_err(|error| ConfigError::Write {
                path: path.to_path_buf(),
                source: error.error,
            })?;
        Ok(())
    }
}

impl McpServerConfig {
    fn new(file: McpServerConfigFile, config_dir: &Path) -> Self {
        Self {
            command: file.command,
            args: file.args,
            env: Arc::new(McpEnvironment(
                file.env
                    .into_iter()
                    .map(|(name, value)| (name, McpSecretString(value)))
                    .collect(),
            )),
            cwd: file.cwd.map(|path| Config::resolve_path(path, config_dir)),
        }
    }

    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    pub(crate) fn env(&self) -> &McpEnvironment {
        &self.env
    }

    pub(crate) fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }
}

impl McpEnvironment {
    /// Explicitly exposes environment values for the narrow scope of starting the server.
    pub(crate) fn expose(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.expose()))
    }
}

impl McpSecretString {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for McpSecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Zeroize for McpEnvironment {
    fn zeroize(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

impl Drop for McpEnvironment {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for McpEnvironment {}

impl fmt::Debug for McpEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_map()
            .entries(self.0.keys().map(|name| (name, "[REDACTED]")))
            .finish()
    }
}

fn serialize_mcp_environment<S>(
    environment: &Arc<McpEnvironment>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;

    let mut map = serializer.serialize_map(Some(environment.0.len()))?;
    for name in environment.0.keys() {
        map.serialize_entry(name, "[REDACTED]")?;
    }
    map.end()
}

impl ConfigReload {
    pub(crate) fn into_parts(self) -> (Config, bool) {
        (self.config, self.workspace_changed)
    }
}

impl AuthConfig {
    pub(crate) const fn new(mode: AuthMode, file: PathBuf) -> Self {
        Self { mode, file }
    }

    pub(crate) const fn mode(&self) -> AuthMode {
        self.mode
    }

    pub(crate) fn file(&self) -> &Path {
        &self.file
    }
}

impl AgentConfig {
    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(crate) const fn thinking(&self) -> ReasoningEffort {
        self.thinking
    }

    pub(crate) fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    pub(crate) const fn web_search(&self) -> bool {
        self.web_search
    }

    pub(crate) const fn image_generation(&self) -> bool {
        self.image_generation
    }

    pub(crate) fn websocket_url(&self) -> Option<&str> {
        self.websocket_url.as_deref()
    }

    pub(crate) fn api_base_url(&self) -> Option<&str> {
        self.api_base_url.as_deref()
    }
}

impl SkillsConfig {
    fn new(file: SkillsConfigFile, config_dir: &Path, environment: &Environment) -> Self {
        let mut roots = Vec::new();
        if file.enabled {
            if let Some(codex_home) = &environment.codex_home {
                roots.push(codex_home.join("skills"));
            } else if let Some(home) = &environment.home {
                roots.push(home.join(".codex/skills"));
            }
            if let Some(home) = &environment.home {
                roots.push(home.join(".agents/skills"));
            }
        }
        roots.extend(
            file.roots
                .into_iter()
                .map(|root| Config::resolve_path(root, config_dir)),
        );
        roots.sort();
        roots.dedup();

        Self {
            enabled: file.enabled,
            roots,
        }
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    #[cfg(test)]
    pub(crate) fn from_roots(enabled: bool, roots: Vec<PathBuf>) -> Self {
        Self { enabled, roots }
    }
}

impl ReasoningEffort {
    pub(crate) const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Xhigh => 3,
            Self::Max => 4,
        }
    }
}

impl From<ReasoningEffort> for Thinking {
    fn from(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::Xhigh => Self::Xhigh,
            ReasoningEffort::Max => Self::Max,
        }
    }
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
}

impl Config {
    fn config_path(
        explicit: Option<PathBuf>,
        environment: &Environment,
        current_dir: &Path,
    ) -> Result<PathBuf> {
        if let Some(path) = explicit {
            return Ok(Self::resolve_path(path, current_dir));
        }
        if let Some(home) = &environment.tact_home {
            return Ok(home.join("config.toml"));
        }

        environment
            .home
            .as_ref()
            .map(|home| home.join(".tact/config.toml"))
            .ok_or(ConfigError::ConfigHomeUnavailable.into())
    }
}

impl ConfigFile {
    fn read(path: &Path, explicit: bool) -> Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => Zeroizing::new(contents),
            Err(source) if source.kind() == ErrorKind::NotFound && !explicit => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                }
                .into());
            }
        };

        toml::from_str(&contents).map_err(|mut source| {
            // TOML errors retain the entire input in a non-zeroizing allocation and render the
            // failing line. Remove it before the error can outlive this zeroizing buffer.
            source.set_input(None);
            ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }
            .into()
        })
    }
}

impl Config {
    fn auth_file_path(
        cli: Option<PathBuf>,
        configured: Option<PathBuf>,
        config_path: &Path,
        environment: &Environment,
        current_dir: &Path,
    ) -> Result<PathBuf> {
        if let Some(path) = Self::configured_path(cli, configured, config_path, current_dir) {
            return Ok(path);
        }
        if let Some(home) = &environment.codex_home {
            return Ok(home.join("auth.json"));
        }

        environment
            .home
            .as_ref()
            .map(|home| home.join(".codex/auth.json"))
            .ok_or(ConfigError::AuthHomeUnavailable.into())
    }

    fn configured_path(
        cli: Option<PathBuf>,
        configured: Option<PathBuf>,
        config_path: &Path,
        current_dir: &Path,
    ) -> Option<PathBuf> {
        if let Some(path) = cli {
            return Some(Self::resolve_path(path, current_dir));
        }

        configured.map(|path| {
            let config_dir = config_path.parent().unwrap_or(Path::new("."));
            Self::resolve_path(path, config_dir)
        })
    }

    fn resolve_path(path: PathBuf, base: &Path) -> PathBuf {
        if path.is_absolute() {
            return path;
        }

        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuthMode, Config, ConfigOverrides, Environment, McpEnvironment, McpSecretString,
        ReasoningEffort, ThemeMode,
    };
    use crate::error::{ConfigError, Error};
    use ratatui::style::Color;
    use std::{collections::BTreeMap, fs, path::Path, sync::Arc};
    use tempfile::tempdir;
    use zeroize::Zeroize;

    #[test]
    fn missing_default_file_materializes_all_defaults() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let config = Config::load_with(
            ConfigOverrides::default(),
            Environment {
                home: Some(home.clone()),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert_eq!(config.path(), home.join(".tact/config.toml"));
        assert_eq!(config.auth.mode, AuthMode::Auto);
        assert_eq!(config.auth.file, home.join(".codex/auth.json"));
        assert_eq!(config.agent.workspace, directory.path());
        assert_eq!(config.agent.thinking, ReasoningEffort::Medium);
        assert!(config.agent.web_search);
        assert!(config.agent.image_generation);
        assert_eq!(config.theme.border(), Color::DarkGray);

        let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
        assert_eq!(rendered["auth"]["mode"].as_str(), Some("auto"));
        assert_eq!(
            rendered["auth"]["file"].as_str(),
            home.join(".codex/auth.json").to_str()
        );
        assert_eq!(
            rendered["agent"]["workspace"].as_str(),
            directory.path().to_str()
        );
        assert_eq!(rendered["agent"]["thinking"].as_str(), Some("medium"));
        assert_eq!(rendered["theme"]["mode"].as_str(), Some("auto"));
        assert_eq!(rendered["theme"]["dark"]["accent"].as_str(), Some("blue"));
    }

    #[test]
    fn explicit_missing_file_is_an_error() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("missing.toml");
        let error = Config::load_with(
            ConfigOverrides {
                path: Some(path.clone()),
                ..ConfigOverrides::default()
            },
            Environment::default(),
            directory.path(),
        )
        .unwrap_err();

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

        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment::default(),
            directory.path(),
        )
        .unwrap();

        assert_eq!(config.auth.mode, AuthMode::ApiKey);
        assert_eq!(config.auth.file, config_dir.join("credentials/auth.json"));
        assert_eq!(config.agent.workspace, config_dir.join("workspace"));
    }

    #[test]
    fn skills_are_disabled_by_default_without_discovery_roots() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let config = Config::load_with(
            ConfigOverrides::default(),
            Environment {
                codex_home: Some(directory.path().join("codex")),
                home: Some(home),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert!(!config.skills.enabled());
        assert!(config.skills.roots().is_empty());
        let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
        assert_eq!(rendered["skills"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn enabled_skills_include_default_and_config_relative_roots() {
        let directory = tempdir().unwrap();
        let config_dir = directory.path().join("settings");
        let config_path = config_dir.join("config.toml");
        let codex_home = directory.path().join("codex");
        let home = directory.path().join("home");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            &config_path,
            "[skills]\nenabled = true\nroots = [\"project-skills\"]\n",
        )
        .unwrap();

        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(codex_home.clone()),
                home: Some(home.clone()),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert!(config.skills.enabled());
        assert_eq!(
            config.skills.roots(),
            [
                codex_home.join("skills"),
                home.join(".agents/skills"),
                config_dir.join("project-skills"),
            ]
        );

        let rendered: toml::Value = toml::from_str(&config.to_toml().unwrap()).unwrap();
        assert_eq!(rendered["skills"]["enabled"].as_bool(), Some(true));
        assert_eq!(rendered["skills"]["roots"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn reload_rebuilds_skills_configuration() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[skills]\nenabled = false\n").unwrap();
        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path.clone()),
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
            "[skills]\nenabled = true\nroots = [\"extra\"]\n",
        )
        .unwrap();
        let (reloaded, _) = config.reload().unwrap().into_parts();

        assert!(reloaded.skills.enabled());
        assert_eq!(
            reloaded.skills.roots(),
            [
                directory.path().join("codex/skills"),
                directory.path().join("extra")
            ]
        );
    }

    #[test]
    fn cli_overrides_take_precedence_and_use_the_working_directory() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[auth]\nmode = \"auto\"\nfile = \"stored.json\"\n\
             \n[agent]\nworkspace = \"configured\"\nthinking = \"low\"\nweb_search = true\n",
        )
        .unwrap();

        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                auth_mode: Some(AuthMode::ChatGpt),
                auth_file: Some("cli-auth.json".into()),
                workspace: Some("cli-workspace".into()),
                thinking: Some(ReasoningEffort::High),
                web_search: Some(false),
                ..ConfigOverrides::default()
            },
            Environment::default(),
            directory.path(),
        )
        .unwrap();

        assert_eq!(config.auth.mode, AuthMode::ChatGpt);
        assert_eq!(config.auth.file, directory.path().join("cli-auth.json"));
        assert_eq!(
            config.agent.workspace,
            directory.path().join("cli-workspace")
        );
        assert_eq!(config.agent.thinking, ReasoningEffort::High);
        assert!(!config.agent.web_search);
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
        assert_eq!(reloaded.agent.workspace, directory.path().join("first"));
        assert_eq!(reloaded.agent.thinking, ReasoningEffort::High);
        assert!(!reloaded.agent.web_search);
        assert_eq!(reloaded.theme.mode(), ThemeMode::Dark);
    }

    #[test]
    fn invalid_reload_reports_the_selected_path_without_changing_the_config() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[agent]\nthinking = \"low\"\n").unwrap();
        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path.clone()),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        fs::write(&config_path, "[agent\n").unwrap();
        let error = config.reload().unwrap_err();

        assert!(
            error
                .to_string()
                .contains(&config_path.display().to_string())
        );
        assert_eq!(config.agent.thinking, ReasoningEffort::Low);
    }

    #[test]
    fn agent_configuration_is_loaded() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[agent]\nworkspace = \"workspace\"\nthinking = \"xhigh\"\n\
             instructions = \"Be concise.\"\nweb_search = false\nimage_generation = false\n\
             websocket_url = \"wss://example.com/responses\"\n\
             api_base_url = \"https://example.com/v1\"\n",
        )
        .unwrap();

        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert_eq!(config.agent.workspace, directory.path().join("workspace"));
        assert_eq!(config.agent.thinking, ReasoningEffort::Xhigh);
        assert_eq!(config.agent.instructions.as_deref(), Some("Be concise."));
        assert!(!config.agent.web_search);
        assert!(!config.agent.image_generation);
        assert_eq!(
            config.agent.websocket_url.as_deref(),
            Some("wss://example.com/responses")
        );
        assert_eq!(
            config.agent.api_base_url.as_deref(),
            Some("https://example.com/v1")
        );
    }

    #[test]
    fn named_stdio_mcp_servers_are_loaded() {
        let directory = tempdir().unwrap();
        let config_dir = directory.path().join("settings");
        let config_path = config_dir.join("config.toml");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            &config_path,
            "[mcp_servers.files]\ncommand = \"node\"\nargs = [\"server.js\", \"--stdio\"]\n\
             cwd = \"servers/files\"\n\n[mcp_servers.files.env]\nTOKEN = \"secret-sentinel\"\n\
             \n[mcp_servers.search]\ncommand = \"search-server\"\n",
        )
        .unwrap();

        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        let files = &config.mcp_servers()["files"];
        assert_eq!(files.command(), "node");
        assert_eq!(files.args(), ["server.js", "--stdio"]);
        assert_eq!(
            files.cwd(),
            Some(config_dir.join("servers/files").as_path())
        );
        assert!(
            files
                .env()
                .expose()
                .any(|(name, value)| name == "TOKEN" && value == "secret-sentinel")
        );

        let search = &config.mcp_servers()["search"];
        assert!(search.args().is_empty());
        assert!(search.env().expose().next().is_none());
        assert_eq!(search.cwd(), None);
    }

    #[test]
    fn cloned_configs_share_mcp_secret_ownership() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[mcp_servers.files]\ncommand = \"files-server\"\n\
             \n[mcp_servers.files.env]\nTOKEN = \"secret-sentinel\"\n",
        )
        .unwrap();
        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        let cloned = config.clone();
        assert!(Arc::ptr_eq(
            &config.mcp_servers["files"].env,
            &cloned.mcp_servers["files"].env,
        ));
    }

    #[test]
    fn mcp_environment_is_redacted_from_config_and_debug_output() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[mcp_servers.files]\ncommand = \"files-server\"\n\
             \n[mcp_servers.files.env]\nTOKEN = \"secret-sentinel\"\n",
        )
        .unwrap();
        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        let rendered = config.to_toml().unwrap();
        let debug = format!("{config:?}");
        assert!(!rendered.contains("secret-sentinel"));
        assert!(!debug.contains("secret-sentinel"));
        assert!(rendered.contains("TOKEN = \"[REDACTED]\""));
        assert!(debug.contains("TOKEN"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn config_parse_errors_do_not_retain_mcp_environment_values() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[mcp_servers.files]\ncommand = \"files-server\"\n\
             \n[mcp_servers.files.env]\nTOKEN = { value = \"secret-sentinel\" }\n",
        )
        .unwrap();

        let error = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap_err();

        assert!(!error.to_string().contains("secret-sentinel"));
    }

    #[test]
    fn mcp_environment_values_can_be_explicitly_zeroized() {
        let mut environment = McpEnvironment(BTreeMap::from([(
            "TOKEN".into(),
            McpSecretString("secret-sentinel".into()),
        )]));

        environment.zeroize();

        assert!(environment.expose().all(|(_, value)| value.is_empty()));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "unknown = true\n").unwrap();

        let error = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment::default(),
            directory.path(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
    }

    #[test]
    fn theme_overrides_are_loaded_and_serialized() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[theme]\ntext = \"#AABBCC\"\nborder = 239\ncode_text = \"white\"\ncode_background = \"#101010\"\nthinking_high = \"green\"\n",
        )
        .unwrap();

        let config = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert_eq!(config.theme.text(), Color::Rgb(0xAA, 0xBB, 0xCC));
        assert_eq!(config.theme.border(), Color::Indexed(239));
        assert_eq!(config.theme.code_text(), Color::White);
        assert_eq!(config.theme.code_background(), Color::Rgb(0x10, 0x10, 0x10));
        assert_eq!(config.theme.thinking_high(), Color::Green);
        assert_eq!(config.theme.accent(), Color::Blue);

        let rendered = config.to_toml().unwrap();
        assert!(rendered.contains("text = \"#AABBCC\""));
        assert!(rendered.contains("border = \"239\""));
    }

    #[test]
    fn invalid_theme_color_is_a_config_parse_error() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[theme]\naccent = \"not-a-color\"\n").unwrap();

        let error = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment::default(),
            directory.path(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
    }

    #[test]
    fn persisting_thinking_preserves_the_rest_of_the_config() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "# Keep this comment.\n[agent]\nthinking = \"low\"\nweb_search = false\n\n\
             [theme]\naccent = \"#AABBCC\"\n",
        )
        .unwrap();

        Config::persist_thinking_at(&path, ReasoningEffort::Xhigh).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let document = toml::from_str::<toml::Value>(&contents).unwrap();
        assert!(contents.contains("# Keep this comment."));
        assert_eq!(document["agent"]["thinking"].as_str(), Some("xhigh"));
        assert_eq!(document["agent"]["web_search"].as_bool(), Some(false));
        assert_eq!(document["theme"]["accent"].as_str(), Some("#AABBCC"));
    }

    #[test]
    fn persisting_theme_mode_preserves_palette_overrides() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "# Keep this comment.\n[theme]\nmode = \"auto\"\naccent = \"#AABBCC\"\n",
        )
        .unwrap();
        let config = Config::load_with(
            ConfigOverrides {
                path: Some(path.clone()),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        config.persist_theme_mode(ThemeMode::Light).unwrap();

        let contents = fs::read_to_string(path).unwrap();
        let document = toml::from_str::<toml::Value>(&contents).unwrap();
        assert!(contents.contains("# Keep this comment."));
        assert_eq!(document["theme"]["mode"].as_str(), Some("light"));
        assert_eq!(document["theme"]["accent"].as_str(), Some("#AABBCC"));
    }

    #[test]
    fn persisting_thinking_creates_a_missing_config_and_parent_directory() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/config.toml");

        Config::persist_thinking_at(&path, ReasoningEffort::Max).unwrap();

        let contents = fs::read_to_string(path).unwrap();
        let document = toml::from_str::<toml::Value>(&contents).unwrap();
        assert!(contents.starts_with("[agent]\n"));
        assert_eq!(document["agent"]["thinking"].as_str(), Some("max"));
    }

    #[test]
    fn tact_home_selects_the_config_path() {
        let directory = tempdir().unwrap();
        let tact_home = directory.path().join("tact-home");
        let config = Config::load_with(
            ConfigOverrides::default(),
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

        let error = Config::load_with(
            ConfigOverrides {
                path: Some(config_path),
                ..ConfigOverrides::default()
            },
            Environment::default(),
            directory.path(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            Error::Config(ConfigError::AuthHomeUnavailable)
        ));
    }
}
