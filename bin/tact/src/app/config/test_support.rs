//! Fixtures shared by the configuration tests.

use super::{Config, ConfigOverrides, Environment};
use crate::app::error::{Error, Result};
use std::{
    error::Error as StdError,
    fs,
    path::{Path, PathBuf},
};
use tempfile::tempdir;

/// Loads `contents` as an explicitly selected, private configuration file.
pub(super) fn load_config(contents: &str) -> Result<Config> {
    let directory = tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    fs::write(&config_path, contents).unwrap();
    load_config_at(config_path, directory.path())
}

/// Loads an existing file as the explicitly selected configuration after making it private, with
/// the Codex home inside `current_dir`.
pub(super) fn load_config_at(config_path: PathBuf, current_dir: &Path) -> Result<Config> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    Config::load_with(
        ConfigOverrides {
            path: Some(config_path),
            ..ConfigOverrides::default()
        },
        Environment {
            codex_home: Some(current_dir.join("codex")),
            ..Environment::default()
        },
        current_dir,
    )
}

/// Loads with only the given environment and no configuration file selected.
pub(super) fn load_default(environment: Environment, current_dir: &Path) -> Result<Config> {
    Config::load_with(ConfigOverrides::default(), environment, current_dir)
}

/// Loads an explicitly selected file with no environment variables set.
pub(super) fn load_without_environment(config_path: PathBuf, current_dir: &Path) -> Result<Config> {
    Config::load_with(
        ConfigOverrides {
            path: Some(config_path),
            ..ConfigOverrides::default()
        },
        Environment::default(),
        current_dir,
    )
}

/// Asserts that no error in the chain mentions `secret` in its message or debug output.
pub(super) fn assert_error_redacts(error: &Error, secret: &str) {
    let mut source: Option<&dyn StdError> = Some(error);
    while let Some(error) = source {
        assert!(!format!("{error:?} {error}").contains(secret));
        source = error.source();
    }
}

/// Parses TOML text and returns the value at a key path.
pub(super) fn toml_value(text: &str, path: &[&str]) -> toml::Value {
    let mut value = toml::from_str::<toml::Value>(text).unwrap();
    for key in path {
        value = value
            .get(key)
            .unwrap_or_else(|| panic!("missing `{}`", path.join(".")))
            .clone();
    }
    value
}

/// Parses TOML text and returns the string stored at a key path.
pub(super) fn toml_string(text: &str, path: &[&str]) -> String {
    toml_value(text, path)
        .as_str()
        .expect("expected a TOML string")
        .to_owned()
}

pub(super) fn assert_table_fields(value: &toml::Value, expected: &[&str]) {
    let table = value.as_table().expect("expected a TOML table");
    assert_eq!(table.len(), expected.len());
    for field in expected {
        assert!(table.contains_key(*field), "missing `{field}`");
    }
}

pub(super) fn remote_memory_config(
    endpoint: &str,
    namespace: &str,
    bearer_token: &str,
    workspace_root: &str,
) -> String {
    format!(
        "[memory.remote]\n\
         endpoint = \"{endpoint}\"\n\
         namespace = \"{namespace}\"\n\
         bearer_token = \"{bearer_token}\"\n\
         workspace_roots = [\"{workspace_root}\"]\n"
    )
}
