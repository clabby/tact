//! Configuration loading, precedence, and effective runtime settings.

use crate::{
    error::{ConfigError, McpUrlError, Result},
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
use toml_edit::{Array, DocumentMut, Item, Table, value};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub(crate) const DEFAULT_MAX_SUBAGENTS: usize = 32;

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

/// Reasoning execution mode used by the model.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningMode {
    #[default]
    Standard,
    Pro,
}

impl ReasoningMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Pro => "pro",
        }
    }
}

impl From<ReasoningMode> for nanocodex::ReasoningMode {
    fn from(mode: ReasoningMode) -> Self {
        match mode {
            ReasoningMode::Standard => Self::Standard,
            ReasoningMode::Pro => Self::Pro,
        }
    }
}

/// Effective application configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Config {
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    codex_home: Option<PathBuf>,
    auth: AuthConfig,
    agent: AgentConfig,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    mcp_servers: BTreeMap<String, McpServerConfig>,
    skills: SkillsConfig,
    theme: Theme,
    #[serde(skip)]
    reload: ReloadSource,
}

/// Configuration for one MCP server transport.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum McpServerConfig {
    Stdio(McpStdioConfig),
    Http(McpHttpConfig),
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpStdioConfig {
    command: String,
    args: Vec<String>,
    #[serde(serialize_with = "serialize_mcp_environment")]
    env: Arc<McpEnvironment>,
    cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct McpHttpConfig {
    url: String,
    bearer_token_env_var: Option<String>,
    header_env: BTreeMap<String, String>,
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
    reasoning_mode: ReasoningMode,
    fast_mode: bool,
    max_subagents: usize,
    instructions: Option<String>,
    append_instructions: Option<String>,
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
    pub(crate) reasoning_mode: Option<ReasoningMode>,
    pub(crate) max_subagents: Option<usize>,
    pub(crate) instructions: Option<String>,
    pub(crate) append_instructions: Option<String>,
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
#[serde(untagged)]
enum McpServerConfigFile {
    Stdio(McpStdioConfigFile),
    Http(McpHttpConfigFile),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpStdioConfigFile {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpHttpConfigFile {
    url: String,
    bearer_token_env_var: Option<String>,
    #[serde(default)]
    header_env: BTreeMap<String, String>,
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
    reasoning_mode: Option<ReasoningMode>,
    fast_mode: Option<bool>,
    max_subagents: Option<usize>,
    instructions: Option<String>,
    append_instructions: Option<String>,
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

    /// Loads configuration for a command that may create the selected file.
    pub(crate) fn load_for_update(overrides: ConfigOverrides) -> Result<Self> {
        let current_dir = env::current_dir().map_err(ConfigError::CurrentDirectory)?;
        Self::load_with_options(overrides, Environment::read(), &current_dir, true)
    }

    fn load_with(
        overrides: ConfigOverrides,
        environment: Environment,
        current_dir: &Path,
    ) -> Result<Self> {
        Self::load_with_options(overrides, environment, current_dir, false)
    }

    fn load_with_options(
        overrides: ConfigOverrides,
        environment: Environment,
        current_dir: &Path,
        allow_missing: bool,
    ) -> Result<Self> {
        let reload = ReloadSource {
            overrides: overrides.clone(),
            environment: environment.clone(),
            current_dir: current_dir.to_path_buf(),
        };
        let explicit_path = overrides.path.is_some();
        let path = Self::config_path(overrides.path, &environment, current_dir)?;
        let file = ConfigFile::read(&path, explicit_path && !allow_missing)?;
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
            .map(|(name, server)| {
                McpServerConfig::new(&name, server, config_dir).map(|server| (name, server))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let skills = SkillsConfig::new(file.skills, config_dir, &environment);
        let codex_home = environment
            .codex_home
            .clone()
            .or_else(|| environment.home.as_ref().map(|home| home.join(".codex")));

        Ok(Self {
            path,
            codex_home,
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
                reasoning_mode: overrides
                    .reasoning_mode
                    .or(file.agent.reasoning_mode)
                    .unwrap_or_default(),
                fast_mode: file.agent.fast_mode.unwrap_or(false),
                max_subagents: overrides
                    .max_subagents
                    .or(file.agent.max_subagents)
                    .unwrap_or(DEFAULT_MAX_SUBAGENTS),
                instructions: overrides.instructions.or(file.agent.instructions),
                append_instructions: overrides
                    .append_instructions
                    .or(file.agent.append_instructions),
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

    pub(crate) fn set_reasoning_mode(&mut self, mode: ReasoningMode) {
        self.agent.reasoning_mode = mode;
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        self.agent.fast_mode = enabled;
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

    pub(crate) fn persist_reasoning_mode(&self, mode: ReasoningMode) -> Result<()> {
        Self::persist_reasoning_mode_at(&self.path, mode)
    }

    pub(crate) fn persist_fast_mode(&self, enabled: bool) -> Result<()> {
        Self::persist_fast_mode_at(&self.path, enabled)
    }

    pub(crate) fn persist_max_subagents(&self, limit: usize) -> Result<()> {
        Self::persist_max_subagents_at(&self.path, limit)
    }

    pub(crate) fn persist_theme_mode(&self, mode: ThemeMode) -> Result<()> {
        Self::persist_setting(&self.path, "theme", "mode", mode.as_str())
    }

    pub(crate) fn add_mcp_server<'a>(
        &self,
        name: &str,
        command: &str,
        arguments: &[String],
        environment: impl Iterator<Item = (&'a str, &'a str)>,
        cwd: Option<&Path>,
    ) -> Result<()> {
        let mut server = Table::new();
        server["command"] = value(command);
        if !arguments.is_empty() {
            let mut values = Array::new();
            values.extend(arguments.iter().map(String::as_str));
            server["args"] = value(values);
        }
        if let Some(cwd) = cwd {
            let cwd = Self::resolve_path(cwd.to_path_buf(), &self.reload.current_dir);
            let cwd = cwd
                .to_str()
                .ok_or_else(|| ConfigError::McpWorkingDirectoryNotUnicode(cwd.to_path_buf()))?;
            server["cwd"] = value(cwd);
        }

        let mut environment_table = Table::new();
        for (name, secret) in environment {
            environment_table[name] = value(secret);
        }
        if !environment_table.is_empty() {
            server["env"] = Item::Table(environment_table);
        }

        self.add_mcp_server_table(name, server)
    }

    pub(crate) fn add_http_mcp_server<'a>(
        &self,
        name: &str,
        url: &str,
        bearer_token_env_var: Option<&str>,
        header_env: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> Result<()> {
        validate_mcp_url(url).map_err(|source| ConfigError::McpUrl {
            name: name.to_owned(),
            source,
        })?;
        let mut server = Table::new();
        server["url"] = value(url);
        if let Some(variable) = bearer_token_env_var {
            server["bearer_token_env_var"] = value(variable);
        }

        let mut headers = Table::new();
        for (header, variable) in header_env {
            headers[header] = value(variable);
        }
        if !headers.is_empty() {
            server["header_env"] = Item::Table(headers);
        }

        self.add_mcp_server_table(name, server)
    }

    fn add_mcp_server_table(&self, name: &str, server: Table) -> Result<()> {
        let mut document = Self::read_document(&self.path)?;
        let document_contains_server = document
            .get("mcp_servers")
            .and_then(Item::as_table_like)
            .is_some_and(|servers| servers.contains_key(name));
        if self.mcp_servers.contains_key(name) || document_contains_server {
            return Err(ConfigError::McpServerExists {
                name: name.to_owned(),
            }
            .into());
        }

        if !document.contains_key("mcp_servers") {
            document["mcp_servers"] = Item::Table(Table::new());
        }
        document["mcp_servers"][name] = Item::Table(server);
        Self::write_document(&self.path, document)
    }

    fn persist_thinking_at(path: &Path, effort: ReasoningEffort) -> Result<()> {
        Self::persist_setting(path, "agent", "thinking", effort.as_str())
    }

    fn persist_reasoning_mode_at(path: &Path, mode: ReasoningMode) -> Result<()> {
        Self::persist_setting(path, "agent", "reasoning_mode", mode.as_str())
    }

    fn persist_fast_mode_at(path: &Path, enabled: bool) -> Result<()> {
        let mut document = Self::read_document(path)?;
        if !document.contains_key("agent") {
            document["agent"] = Item::Table(Table::new());
        }
        document["agent"]["fast_mode"] = value(enabled);
        Self::write_document(path, document)
    }

    fn persist_max_subagents_at(path: &Path, limit: usize) -> Result<()> {
        let mut document = Self::read_document(path)?;
        if !document.contains_key("agent") {
            document["agent"] = Item::Table(Table::new());
        }
        document["agent"]["max_subagents"] = value(i64::try_from(limit).unwrap_or(i64::MAX));
        Self::write_document(path, document)
    }

    fn persist_setting(path: &Path, section: &str, key: &str, setting: &str) -> Result<()> {
        let mut document = Self::read_document(path)?;
        if !document.contains_key(section) {
            document[section] = Item::Table(Table::new());
        }
        document[section][key] = value(setting);
        Self::write_document(path, document)
    }

    fn read_document(path: &Path) -> Result<DocumentMut> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => Zeroizing::new(contents),
            Err(source) if source.kind() == ErrorKind::NotFound => Zeroizing::new(String::new()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                }
                .into());
            }
        };
        contents
            .parse::<DocumentMut>()
            .map_err(|source| ConfigError::UpdateParse {
                path: path.to_path_buf(),
                source,
            })
            .map_err(Into::into)
    }

    fn write_document(path: &Path, document: DocumentMut) -> Result<()> {
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
        // toml_edit retains non-zeroizing copies of values while the document is alive. Secret
        // guarantees therefore cover the parsed CLI values owned by tact, not toml_edit's
        // transient representation used to update the configuration file.
        let rendered = Zeroizing::new(document.to_string());
        temporary
            .write_all(rendered.as_bytes())
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
    fn new(name: &str, file: McpServerConfigFile, config_dir: &Path) -> Result<Self> {
        let config = match file {
            McpServerConfigFile::Stdio(file) => Self::Stdio(McpStdioConfig {
                command: file.command,
                args: file.args,
                env: Arc::new(McpEnvironment(
                    file.env
                        .into_iter()
                        .map(|(name, value)| (name, McpSecretString(value)))
                        .collect(),
                )),
                cwd: file.cwd.map(|path| Config::resolve_path(path, config_dir)),
            }),
            McpServerConfigFile::Http(mut file) => {
                if let Err(source) = validate_mcp_url(&file.url) {
                    file.url.zeroize();
                    return Err(ConfigError::McpUrl {
                        name: name.to_owned(),
                        source,
                    }
                    .into());
                }
                Self::Http(McpHttpConfig {
                    url: file.url,
                    bearer_token_env_var: file.bearer_token_env_var,
                    header_env: file.header_env,
                })
            }
        };
        Ok(config)
    }

    #[cfg(test)]
    fn stdio(&self) -> &McpStdioConfig {
        let Self::Stdio(config) = self else {
            panic!("expected stdio MCP server");
        };
        config
    }
}

pub(crate) fn validate_mcp_url(value: &str) -> std::result::Result<(), McpUrlError> {
    if value.trim().is_empty() {
        return Err(McpUrlError::Empty);
    }
    // Reject standard URL userinfo before parsing so the URL dependency never allocates its own
    // non-zeroizing copy of embedded credentials.
    let contains_userinfo = value.split_once(':').is_some_and(|(scheme, remainder)| {
        (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            && remainder
                .trim_start_matches('/')
                .split(['/', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
    });
    if contains_userinfo {
        return Err(McpUrlError::Credentials);
    }
    let url = url::Url::parse(value).map_err(McpUrlError::Parse)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(McpUrlError::UnsupportedScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(McpUrlError::Credentials);
    }
    Ok(())
}

impl McpStdioConfig {
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

impl McpHttpConfig {
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn bearer_token_env_var(&self) -> Option<&str> {
        self.bearer_token_env_var.as_deref()
    }

    pub(crate) fn header_env(&self) -> &BTreeMap<String, String> {
        &self.header_env
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

    pub(crate) const fn reasoning_mode(&self) -> ReasoningMode {
        self.reasoning_mode
    }

    pub(crate) const fn fast_mode(&self) -> bool {
        self.fast_mode
    }

    pub(crate) const fn max_subagents(&self) -> usize {
        self.max_subagents
    }

    pub(crate) fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    pub(crate) fn append_instructions(&self) -> Option<&str> {
        self.append_instructions.as_deref()
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
        McpServerConfig, ReasoningEffort, ReasoningMode, ThemeMode, validate_mcp_url,
    };
    use crate::error::{ConfigError, Error, McpUrlError};
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
        assert_eq!(config.agent.reasoning_mode, ReasoningMode::Standard);
        assert!(!config.agent.fast_mode);
        assert_eq!(config.agent.max_subagents, 32);
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
        assert_eq!(rendered["agent"]["fast_mode"].as_bool(), Some(false));
        assert_eq!(rendered["agent"]["max_subagents"].as_integer(), Some(32));
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
    fn codex_home_uses_environment_override_or_home_default() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let default_codex_home = home.join(".codex");
        let configured_codex_home = directory.path().join("configured-codex");
        let overridden = Config::load_with(
            ConfigOverrides::default(),
            Environment {
                codex_home: Some(configured_codex_home.clone()),
                home: Some(home.clone()),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();
        let defaulted = Config::load_with(
            ConfigOverrides::default(),
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
            "[agent]\nworkspace = \"workspace\"\nthinking = \"xhigh\"\nreasoning_mode = \"pro\"\nfast_mode = true\nmax_subagents = 7\n\
             instructions = \"Be concise.\"\nappend_instructions = \"Use project conventions.\"\n\
             web_search = false\nimage_generation = false\n\
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
        assert_eq!(config.agent.reasoning_mode, ReasoningMode::Pro);
        assert!(config.agent.fast_mode);
        assert_eq!(config.agent.max_subagents, 7);
        assert_eq!(config.agent.instructions.as_deref(), Some("Be concise."));
        assert_eq!(
            config.agent.append_instructions.as_deref(),
            Some("Use project conventions.")
        );
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

        let files = config.mcp_servers()["files"].stdio();
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

        let search = config.mcp_servers()["search"].stdio();
        assert!(search.args().is_empty());
        assert!(search.env().expose().next().is_none());
        assert_eq!(search.cwd(), None);
    }

    #[test]
    fn remote_mcp_servers_round_trip_environment_references() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[mcp_servers.docs]\nurl = \"https://example.com/mcp\"\n\
             bearer_token_env_var = \"MCP_TOKEN\"\n\n\
             [mcp_servers.docs.header_env]\nX-Tenant = \"TENANT_ID\"\n",
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
        let McpServerConfig::Http(server) = &config.mcp_servers()["docs"] else {
            panic!("expected HTTP MCP server");
        };
        assert_eq!(server.url(), "https://example.com/mcp");
        assert_eq!(server.bearer_token_env_var(), Some("MCP_TOKEN"));
        assert_eq!(server.header_env()["X-Tenant"], "TENANT_ID");

        let rendered = config.to_toml().unwrap();
        assert!(rendered.contains("bearer_token_env_var = \"MCP_TOKEN\""));
        assert!(rendered.contains("X-Tenant = \"TENANT_ID\""));
    }

    #[test]
    fn whitespace_remote_mcp_url_is_rejected_at_config_load() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[mcp_servers.docs]\nurl = \" \"\n").unwrap();

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
        assert!(matches!(
            error,
            Error::Config(ConfigError::McpUrl { name, .. }) if name == "docs"
        ));
    }

    #[test]
    fn remote_mcp_urls_require_http_or_https() {
        assert!(validate_mcp_url("http://localhost:8080/mcp?tenant=one").is_ok());
        assert!(validate_mcp_url("https://example.com/mcp").is_ok());
        assert!(matches!(
            validate_mcp_url("file:///tmp/mcp.sock"),
            Err(McpUrlError::UnsupportedScheme)
        ));
        assert!(matches!(
            validate_mcp_url("http:user:not-a-real-secret@example.com/mcp"),
            Err(McpUrlError::Credentials)
        ));
    }

    #[test]
    fn credential_bearing_remote_mcp_url_is_rejected_without_entering_diagnostics() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "[mcp_servers.docs]\nurl = \"https://user:not-a-real-secret@example.com/mcp\"\n",
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
        let rendered = format!("{error:?} {error}");
        assert!(matches!(
            error,
            Error::Config(ConfigError::McpUrl { name, .. }) if name == "docs"
        ));
        assert!(!rendered.contains("not-a-real-secret"));
        assert!(rendered.contains("must not contain credentials"));
    }

    #[test]
    fn invalid_mcp_transport_mixtures_are_rejected() {
        for server in [
            "command = \"server\"\nurl = \"https://example.com/mcp\"",
            "url = \"https://example.com/mcp\"\nargs = [\"--invalid\"]",
            "url = \"https://example.com/mcp\"\nenv = { TOKEN = \"secret\" }",
            "url = \"https://example.com/mcp\"\ncwd = \".\"",
            "command = \"server\"\nbearer_token_env_var = \"TOKEN\"",
            "header_env = { X = \"TOKEN\" }",
        ] {
            let directory = tempdir().unwrap();
            let config_path = directory.path().join("config.toml");
            fs::write(&config_path, format!("[mcp_servers.invalid]\n{server}\n")).unwrap();

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
    }

    #[test]
    fn adding_a_remote_mcp_server_preserves_unrelated_toml() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "# Keep this comment.\n[agent]\nthinking = \"high\"\n",
        )
        .unwrap();
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

        config
            .add_http_mcp_server(
                "docs",
                "https://example.com/mcp",
                Some("MCP_TOKEN"),
                [("X-Tenant", "TENANT_ID")].into_iter(),
            )
            .unwrap();

        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("# Keep this comment."));
        assert!(contents.contains("url = \"https://example.com/mcp\""));
        assert!(contents.contains("bearer_token_env_var = \"MCP_TOKEN\""));
        assert!(contents.contains("X-Tenant = \"TENANT_ID\""));

        let error = config
            .add_http_mcp_server(
                "docs",
                "https://other.example.com/mcp",
                None,
                std::iter::empty(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Config(ConfigError::McpServerExists { name }) if name == "docs"
        ));
        assert_eq!(fs::read_to_string(config_path).unwrap(), contents);
    }

    #[test]
    fn adding_an_mcp_server_creates_and_preserves_configuration() {
        let directory = tempdir().unwrap();
        let config_dir = directory.path().join("settings");
        let config_path = config_dir.join("config.toml");
        let config = Config::load_with_options(
            ConfigOverrides {
                path: Some(config_path.clone()),
                ..ConfigOverrides::default()
            },
            Environment {
                codex_home: Some(directory.path().join("codex")),
                ..Environment::default()
            },
            directory.path(),
            true,
        )
        .unwrap();

        config
            .add_mcp_server(
                "files.v1",
                "npx",
                &[
                    "-y".to_owned(),
                    "@modelcontextprotocol/server-filesystem".to_owned(),
                ],
                [("TOKEN", "configured-value")].into_iter(),
                Some(Path::new("servers/files")),
            )
            .unwrap();

        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("[mcp_servers.\"files.v1\"]"));
        let loaded = Config::load_with(
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
        let server = loaded.mcp_servers()["files.v1"].stdio();
        assert_eq!(server.command(), "npx");
        assert_eq!(
            server.args(),
            ["-y", "@modelcontextprotocol/server-filesystem"]
        );
        assert_eq!(
            server.cwd(),
            Some(directory.path().join("servers/files").as_path())
        );
        assert_eq!(
            server.env().expose().next(),
            Some(("TOKEN", "configured-value"))
        );
    }

    #[test]
    fn adding_an_mcp_server_preserves_unrelated_toml_and_rejects_duplicates() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            "# Keep this comment.\n[agent]\nthinking = \"high\"\n",
        )
        .unwrap();
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
        config
            .add_mcp_server("search", "search-server", &[], std::iter::empty(), None)
            .unwrap();
        let before_duplicate = fs::read_to_string(&config_path).unwrap();

        let error = config
            .add_mcp_server("search", "other-server", &[], std::iter::empty(), None)
            .unwrap_err();

        assert!(matches!(
            error,
            Error::Config(ConfigError::McpServerExists { name }) if name == "search"
        ));
        assert_eq!(fs::read_to_string(config_path).unwrap(), before_duplicate);
        assert!(before_duplicate.contains("# Keep this comment."));
        assert!(before_duplicate.contains("thinking = \"high\""));
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
            &config.mcp_servers["files"].stdio().env,
            &cloned.mcp_servers["files"].stdio().env,
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
    fn persisting_reasoning_mode_preserves_the_rest_of_the_config() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "# Keep this comment.\n[agent]\nreasoning_mode = \"standard\"\nweb_search = false\n",
        )
        .unwrap();

        Config::persist_reasoning_mode_at(&path, ReasoningMode::Pro).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let document = toml::from_str::<toml::Value>(&contents).unwrap();
        assert!(contents.contains("# Keep this comment."));
        assert_eq!(document["agent"]["reasoning_mode"].as_str(), Some("pro"));
        assert_eq!(document["agent"]["web_search"].as_bool(), Some(false));
    }

    #[test]
    fn persisting_fast_mode_preserves_the_rest_of_the_config() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "# Keep this comment.\n[agent]\nfast_mode = false\nweb_search = false\n\n\
             [theme]\naccent = \"#AABBCC\"\n",
        )
        .unwrap();

        Config::persist_fast_mode_at(&path, true).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let document = toml::from_str::<toml::Value>(&contents).unwrap();
        assert!(contents.contains("# Keep this comment."));
        assert_eq!(document["agent"]["fast_mode"].as_bool(), Some(true));
        assert_eq!(document["agent"]["web_search"].as_bool(), Some(false));
        assert_eq!(document["theme"]["accent"].as_str(), Some("#AABBCC"));
    }

    #[test]
    fn persisting_max_subagents_preserves_the_rest_of_the_config() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "# Keep this comment.\n[agent]\nmax_subagents = 32\nweb_search = false\n",
        )
        .unwrap();

        Config::persist_max_subagents_at(&path, 8).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let document = toml::from_str::<toml::Value>(&contents).unwrap();
        assert!(contents.contains("# Keep this comment."));
        assert_eq!(document["agent"]["max_subagents"].as_integer(), Some(8));
        assert_eq!(document["agent"]["web_search"].as_bool(), Some(false));
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
