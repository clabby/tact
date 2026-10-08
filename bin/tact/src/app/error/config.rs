//! Errors from locating, parsing, resolving, and editing the configuration file.
//!
//! Values that may hold credentials never appear in these errors: parser errors drop their
//! retained input, and rejected URLs are reported by kind only.

use std::{io, path::PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("[web] public_url and tailscale are mutually exclusive; set only one of them")]
    WebExposureConflict,
    #[error("Claude models require [claude] enabled = true in the configuration")]
    ClaudeDisabled,
    #[error("could not determine the config directory; set TACT_HOME or pass --config")]
    ConfigHomeUnavailable,
    #[error("could not determine the credential directory; set CODEX_HOME or pass --auth-file")]
    AuthHomeUnavailable,
    #[error("failed to determine the current directory: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error("failed to read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse configuration file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[cfg(unix)]
    #[error(
        "configuration file {path} contains credentials but has insecure permissions {mode:#o}; remove all group and other permissions"
    )]
    InsecureSecretPermissions { path: PathBuf, mode: u32 },
    #[cfg(not(unix))]
    #[error(
        "configuration file {path} contains credentials, but this platform's file privacy cannot be verified"
    )]
    UnsupportedSecretPermissions { path: PathBuf },
    #[error("failed to serialize the effective configuration: {0}")]
    Serialize(#[source] toml::ser::Error),
    #[error("MCP server `{name}` is already configured")]
    McpServerExists { name: String },
    #[error("MCP server `{name}` has an invalid URL: {source}")]
    McpUrl {
        name: String,
        #[source]
        source: McpUrlError,
    },
    #[error(transparent)]
    RemoteMemory(#[from] RemoteMemoryConfigError),
    #[error("remote memory is not configured")]
    RemoteMemoryNotConfigured,
    #[error("MCP environment variable {name} is not set")]
    McpEnvironmentNotPresent { name: String },
    #[error("MCP environment variable {name} is not valid Unicode")]
    McpEnvironmentNotUnicode { name: String },
    #[error("MCP server working directory is not valid Unicode: {0}")]
    McpWorkingDirectoryNotUnicode(PathBuf),
    #[error("failed to update configuration file {path}: {source}")]
    UpdateParse {
        path: PathBuf,
        #[source]
        source: ConfigSyntaxError,
    },
    #[error("failed to write configuration file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Why a remote edit of the configuration file was refused.
#[derive(Debug, Error)]
pub(crate) enum ConfigEditError {
    #[error("the configuration file changed since it was read")]
    Stale,
    #[error("the configuration file holds credentials and can only be edited in the terminal")]
    HoldsCredentials,
    /// The proposed text does not load as a configuration.
    #[error(transparent)]
    Invalid(super::Error),
    #[error(transparent)]
    Io(ConfigError),
}

/// A syntax diagnostic that excludes the parser's retained source document.
#[derive(Debug, Error)]
#[error("{message}")]
pub(crate) struct ConfigSyntaxError {
    pub(crate) message: String,
}

#[derive(Debug, Error)]
pub(crate) enum McpUrlError {
    #[error("the URL must not be empty or whitespace-only")]
    Empty,
    #[error("the URL is not valid")]
    Parse(#[source] url::ParseError),
    #[error("the URL must use the http or https scheme")]
    UnsupportedScheme,
    #[error("the URL must not contain credentials")]
    Credentials,
}

#[derive(Debug, Error)]
pub(crate) enum RemoteMemoryConfigError {
    #[error(
        "remote memory configuration requires an endpoint, namespace, bearer token, and at least one workspace root"
    )]
    Incomplete,
    #[error("remote memory endpoint is invalid: {0}")]
    Endpoint(#[source] McpUrlError),
    #[error("remote memory namespace must be non-empty and have no leading or trailing whitespace")]
    NamespaceWhitespace,
    #[error("remote memory namespace must not contain control characters")]
    NamespaceControl,
    #[error(
        "remote memory namespace must use at most 128 ASCII letters, digits, periods, hyphens, or underscores"
    )]
    NamespaceInvalid,
    #[error("failed to resolve remote memory workspace root {path}: {source}")]
    ResolveWorkspaceRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("remote memory workspace root is not a directory: {0}")]
    WorkspaceRootNotDirectory(PathBuf),
}
