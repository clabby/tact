//! Conversion from application MCP configuration to the Nanocodex provider.

use crate::{
    config::Config,
    error::{Error, Result},
};
use nanocodex::{Mcp, McpServer};

pub(crate) fn provider(config: &Config) -> Result<Option<Mcp>> {
    if config.mcp_servers().is_empty() {
        return Ok(None);
    }

    let mut builder = Mcp::builder();
    for (name, config) in config.mcp_servers() {
        let mut server = McpServer::stdio(config.command()).args(config.args());

        // Nanocodex retains non-zeroizing String copies for the lifetime of the provider.
        for (name, value) in config.env().expose() {
            server = server.env(name, value);
        }
        if let Some(cwd) = config.cwd() {
            server = server.cwd(cwd);
        }

        builder = builder.server(name, server);
    }

    builder.build().map(Some).map_err(Error::Mcp)
}

#[cfg(test)]
mod tests {
    use super::provider;
    use crate::{
        config::{Config, ConfigOverrides},
        error::Error,
    };
    use nanocodex::McpBuildError;
    use std::fs;
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
