//! Effective MCP server definitions and the secret ownership of their environment values.
//!
//! Stdio servers may carry literal environment values, which are secrets: they are zeroized on
//! drop, shared across cloned configurations, and rendered redacted. Remote servers name
//! environment variables instead of holding credentials, and their URLs may not embed userinfo.

use super::{
    file::{McpHttpConfigFile, McpServerConfigFile, McpStdioConfigFile},
    render::REDACTED,
    resolve_path,
};
use crate::app::error::{ConfigError, McpUrlError};
use serde::{Serialize, Serializer, ser::SerializeMap};
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::{Zeroize, ZeroizeOnDrop};

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

impl McpServerConfig {
    /// Resolves a server from the file. A stdio working directory is relative to the
    /// configuration directory.
    pub(super) fn new(
        name: &str,
        file: McpServerConfigFile,
        config_dir: &Path,
    ) -> Result<Self, ConfigError> {
        match file {
            McpServerConfigFile::Stdio(file) => {
                Ok(Self::Stdio(McpStdioConfig::new(file, config_dir)))
            }
            McpServerConfigFile::Http(file) => McpHttpConfig::new(name, file).map(Self::Http),
        }
    }

    #[cfg(test)]
    pub(super) fn stdio(&self) -> &McpStdioConfig {
        let Self::Stdio(config) = self else {
            panic!("expected stdio MCP server");
        };
        config
    }
}

/// Validates an HTTP endpoint that Tact connects to. URLs carrying credentials are rejected
/// without letting the URL parser copy them.
pub(super) fn validate_mcp_url(value: &str) -> Result<(), McpUrlError> {
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
    fn new(file: McpStdioConfigFile, config_dir: &Path) -> Self {
        Self {
            command: file.command,
            args: file.args,
            env: Arc::new(McpEnvironment(
                file.env
                    .into_iter()
                    .map(|(name, value)| (name, McpSecretString(value)))
                    .collect(),
            )),
            cwd: file.cwd.map(|path| resolve_path(path, config_dir)),
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

impl McpHttpConfig {
    /// Rejected URLs are zeroized because they may embed credentials.
    fn new(name: &str, mut file: McpHttpConfigFile) -> Result<Self, ConfigError> {
        if let Err(source) = validate_mcp_url(&file.url) {
            file.url.zeroize();
            return Err(ConfigError::McpUrl {
                name: name.to_owned(),
                source,
            });
        }
        Ok(Self {
            url: file.url,
            bearer_token_env_var: file.bearer_token_env_var,
            header_env: file.header_env,
        })
    }

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
            .map(|(name, value)| (name.as_str(), value.0.as_str()))
    }
}

impl fmt::Debug for McpSecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
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
            .entries(self.0.keys().map(|name| (name, REDACTED)))
            .finish()
    }
}

fn serialize_mcp_environment<S>(
    environment: &Arc<McpEnvironment>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut map = serializer.serialize_map(Some(environment.0.len()))?;
    for name in environment.0.keys() {
        map.serialize_entry(name, REDACTED)?;
    }
    map.end()
}

#[cfg(test)]
mod tests {
    use super::{McpEnvironment, McpSecretString, validate_mcp_url};
    use crate::app::{
        config::{
            McpServerConfig,
            test_support::{load_config_at, load_without_environment, toml_string},
        },
        error::{ConfigError, Error, McpUrlError},
    };
    use std::{collections::BTreeMap, fs, sync::Arc};
    use tempfile::tempdir;
    use zeroize::Zeroize;

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

        let config = load_config_at(config_path, directory.path()).unwrap();

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

        let config = load_config_at(config_path, directory.path()).unwrap();
        let McpServerConfig::Http(server) = &config.mcp_servers()["docs"] else {
            panic!("expected HTTP MCP server");
        };
        assert_eq!(server.url(), "https://example.com/mcp");
        assert_eq!(server.bearer_token_env_var(), Some("MCP_TOKEN"));
        assert_eq!(server.header_env()["X-Tenant"], "TENANT_ID");

        let rendered = config.to_toml().unwrap();
        assert_eq!(
            toml_string(&rendered, &["mcp_servers", "docs", "bearer_token_env_var"]),
            "MCP_TOKEN"
        );
        assert_eq!(
            toml_string(
                &rendered,
                &["mcp_servers", "docs", "header_env", "X-Tenant"]
            ),
            "TENANT_ID"
        );
    }

    #[test]
    fn whitespace_remote_mcp_url_is_rejected_at_config_load() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "[mcp_servers.docs]\nurl = \" \"\n").unwrap();

        let error = load_config_at(config_path, directory.path()).unwrap_err();
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

        let error = load_config_at(config_path, directory.path()).unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(matches!(
            error,
            Error::Config(ConfigError::McpUrl { name, .. }) if name == "docs"
        ));
        assert!(!rendered.contains("not-a-real-secret"));
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

            let error = load_without_environment(config_path, directory.path()).unwrap_err();
            assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
        }
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
        let config = load_config_at(config_path, directory.path()).unwrap();

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
        let config = load_config_at(config_path, directory.path()).unwrap();

        let rendered = config.to_toml().unwrap();
        let debug = format!("{config:?}");
        assert!(!rendered.contains("secret-sentinel"));
        assert!(!debug.contains("secret-sentinel"));
        assert_eq!(
            toml_string(&rendered, &["mcp_servers", "files", "env", "TOKEN"]),
            "[REDACTED]"
        );
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

        let error = load_config_at(config_path, directory.path()).unwrap_err();

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
}
