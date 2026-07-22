//! Conversion from application MCP configuration to the Nanocodex provider.

use crate::{
    config::{Config, McpServerConfig},
    error::{Error, Result},
};
use nanocodex::{Mcp, McpServer};
use std::{env, env::VarError};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

// Process environments cannot contain NUL, so Nanocodex can only resolve this reference as
// missing. It lets one misconfigured server fail independently without formatting the original
// VarError.
const UNAVAILABLE_REMOTE_SECRET_ENV: &str = "TACT_MCP_REMOTE_SECRET_UNAVAILABLE\0";

#[derive(Zeroize, ZeroizeOnDrop)]
enum RemoteSecret {
    Value(Zeroizing<String>),
    Unavailable,
}

pub(crate) fn provider(config: &Config) -> Result<Option<Mcp>> {
    if config.mcp_servers().is_empty() {
        return Ok(None);
    }

    let mut builder = Mcp::builder();
    for (name, config) in config.mcp_servers() {
        let server = match config {
            McpServerConfig::Stdio(config) => {
                let mut server = McpServer::stdio(config.command()).args(config.args());

                // Nanocodex retains non-zeroizing String copies for the provider's lifetime.
                for (name, value) in config.env().expose() {
                    server = server.env(name, value);
                }
                if let Some(cwd) = config.cwd() {
                    server = server.cwd(cwd);
                }
                server
            }
            McpServerConfig::Http(config) => {
                let mut server = McpServer::http(config.url());
                // Tact zeroizes each transient environment read after Nanocodex copies it.
                // Nanocodex's retained, non-zeroizing copy is outside tact's memory ownership.
                if let Some(variable) = config.bearer_token_env_var() {
                    let secret = remote_secret(variable, |name| env::var(name));
                    server = match &secret {
                        RemoteSecret::Value(value) => server.bearer_token(value.as_str()),
                        RemoteSecret::Unavailable => {
                            server.bearer_token_env(UNAVAILABLE_REMOTE_SECRET_ENV)
                        }
                    };
                }
                for (header, variable) in config.header_env() {
                    let secret = remote_secret(variable, |name| env::var(name));
                    server = match &secret {
                        RemoteSecret::Value(value) => server.header(header, value.as_str()),
                        RemoteSecret::Unavailable => {
                            server.header_env(header, UNAVAILABLE_REMOTE_SECRET_ENV)
                        }
                    };
                }
                server
            }
        };
        builder = builder.server(name, server);
    }

    builder.build().map(Some).map_err(Error::Mcp)
}

fn remote_secret(
    variable: &str,
    read: impl FnOnce(&str) -> std::result::Result<String, VarError>,
) -> RemoteSecret {
    match read(variable) {
        Ok(value) => RemoteSecret::Value(Zeroizing::new(value)),
        Err(VarError::NotPresent) => RemoteSecret::Unavailable,
        // VarError owns a copy of the secret. Consume and zeroize that allocation without
        // formatting it; Nanocodex receives only the guaranteed-missing reference above.
        Err(VarError::NotUnicode(value)) => {
            let mut bytes = value.into_encoded_bytes();
            bytes.zeroize();
            RemoteSecret::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteSecret, UNAVAILABLE_REMOTE_SECRET_ENV, provider, remote_secret};
    use crate::{
        config::{Config, ConfigOverrides},
        error::Error,
    };
    use nanocodex::McpBuildError;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use std::{env::VarError, ffi::OsString, fs};
    use tempfile::tempdir;

    fn load(contents: &str) -> Config {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, contents).unwrap();
        Config::load(ConfigOverrides {
            path: Some(path),
            ..ConfigOverrides::default()
        })
        .unwrap()
    }

    #[test]
    fn empty_configuration_has_no_provider() {
        assert!(provider(&load("")).unwrap().is_none());
    }

    #[test]
    fn multiple_named_stdio_servers_build_a_provider() {
        let config = load(
            r#"
            [mcp_servers.files]
            command = "files-server"
            args = ["--stdio"]
            cwd = "servers/files"

            [mcp_servers.files.env]
            TOKEN = "secret"

            [mcp_servers.search]
            command = "search-server"
            "#,
        );

        assert!(provider(&config).unwrap().is_some());
    }

    #[test]
    fn remote_server_with_environment_backed_auth_builds_a_provider() {
        let config = load(
            r#"
            [mcp_servers.docs]
            url = "https://example.com/mcp"
            bearer_token_env_var = "MCP_TOKEN"

            [mcp_servers.docs.header_env]
            X-Tenant = "TENANT_ID"
            "#,
        );

        assert!(provider(&config).unwrap().is_some());
    }

    #[test]
    fn non_unicode_remote_auth_is_replaced_before_nanocodex_can_format_it() {
        for variable in ["MCP_TOKEN", "MCP_HEADER"] {
            #[cfg(unix)]
            let value = OsString::from_vec(b"secret-sentinel\xff".to_vec());
            #[cfg(not(unix))]
            let value = OsString::from("secret-sentinel");
            let secret = remote_secret(variable, |_| Err(VarError::NotUnicode(value)));
            assert!(matches!(secret, RemoteSecret::Unavailable));
        }

        let error = std::env::var(UNAVAILABLE_REMOTE_SECRET_ENV).unwrap_err();
        let diagnostic = format!(
            "environment variable `{UNAVAILABLE_REMOTE_SECRET_ENV}` is unavailable: {error}"
        );
        assert!(!diagnostic.contains("secret-sentinel"));
        assert!(matches!(error, VarError::NotPresent));
    }

    #[test]
    fn invalid_server_preserves_the_build_error() {
        let error = provider(&load(
            r#"
            [mcp_servers.invalid]
            command = ""
            "#,
        ))
        .err()
        .unwrap();

        assert!(matches!(
            error,
            Error::Mcp(McpBuildError::EmptyField { server, field })
                if server == "invalid" && field == "command"
        ));
    }
}
