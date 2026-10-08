//! Writes to the configuration file.
//!
//! Every write is a targeted edit that preserves the file's formatting, comments, and unrelated
//! settings, including literal credentials. The new contents are staged in a temporary file
//! beside the configuration and renamed over it, so readers never observe a partial file and a
//! refused edit leaves the file untouched. A missing file and its parent directory are created.

use super::{
    Config, ConfigOverrides,
    file::ConfigFile,
    mcp::validate_mcp_url,
    resolve_path,
    settings::{ReasoningEffort, ReasoningMode, Speed},
};
use crate::app::{
    error::{ConfigEditError, ConfigError, ConfigSyntaxError, Result},
    theme::ThemeMode,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
    result::Result as StdResult,
};
use tempfile::NamedTempFile;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value, value};
use zeroize::Zeroizing;

/// The configuration file as a remote editor sees it. Only files without credentials are offered:
/// credentials never leave the terminal.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ConfigDocument {
    pub(crate) path: PathBuf,
    /// The file's text; empty when the file does not exist yet.
    pub(crate) text: String,
    /// A digest of `text`. A write must name the revision it replaces so it cannot silently
    /// discard a concurrent edit.
    pub(crate) revision: String,
}

impl ConfigDocument {
    fn revision_of(text: &str) -> String {
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// A runtime preference that front-ends persist when the user changes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Setting {
    Thinking(ReasoningEffort),
    ReasoningMode(ReasoningMode),
    Speed(Speed),
    MaxSubagents(usize),
    ThemeMode(ThemeMode),
}

impl Setting {
    const fn section(self) -> &'static str {
        match self {
            Self::Thinking(_) | Self::ReasoningMode(_) | Self::Speed(_) | Self::MaxSubagents(_) => {
                "agent"
            }
            Self::ThemeMode(_) => "theme",
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::Thinking(_) => "thinking",
            Self::ReasoningMode(_) => "reasoning_mode",
            Self::Speed(_) => "speed",
            Self::MaxSubagents(_) => "max_subagents",
            Self::ThemeMode(_) => "mode",
        }
    }

    /// Legacy keys in the same section that this setting replaces. They are removed so the
    /// file holds one unambiguous value.
    const fn superseded_keys(self) -> &'static [&'static str] {
        match self {
            Self::Speed(_) => &["fast_mode"],
            _ => &[],
        }
    }

    fn value(self) -> Value {
        match self {
            Self::Thinking(effort) => effort.as_str().into(),
            Self::ReasoningMode(mode) => mode.as_str().into(),
            Self::Speed(speed) => speed.as_str().into(),
            Self::MaxSubagents(limit) => i64::try_from(limit).unwrap_or(i64::MAX).into(),
            Self::ThemeMode(mode) => mode.as_str().into(),
        }
    }

    /// Writes this setting into the configuration file at `path`.
    fn write_to(self, path: &Path) -> Result<()> {
        let mut document = EditableFile::open(path)?;
        let section = document.section(self.section());
        for key in self.superseded_keys() {
            section.remove(key);
        }
        section.insert(self.key(), Item::Value(self.value()));
        Ok(document.save()?)
    }
}

impl Config {
    /// Writes a changed preference to the configuration file.
    pub(crate) fn persist(&self, setting: Setting) -> Result<()> {
        setting.write_to(&self.path)
    }

    /// Reads the configuration file for a remote editor.
    pub(crate) fn document(&self) -> StdResult<ConfigDocument, ConfigEditError> {
        let text = Self::read_editable(&self.path)?;
        Ok(ConfigDocument {
            path: self.path.clone(),
            revision: ConfigDocument::revision_of(&text),
            text: text.to_string(),
        })
    }

    /// Replaces the configuration file with `text` if the file still has `revision` and `text`
    /// loads as a valid configuration under this process's overrides and environment. The file
    /// is replaced atomically and is untouched on refusal. The caller reloads afterwards.
    pub(crate) fn replace_document(
        &self,
        text: &str,
        revision: &str,
    ) -> StdResult<(), ConfigEditError> {
        let current = Self::read_editable(&self.path)?;
        if ConfigDocument::revision_of(&current) != revision {
            return Err(ConfigEditError::Stale);
        }
        let parsed = ConfigFile::parse(text, &self.path)
            .map_err(|error| ConfigEditError::Invalid(error.into()))?;
        if parsed.has_credentials() {
            return Err(ConfigEditError::HoldsCredentials);
        }
        let candidate =
            StagedFile::write(&self.path, text.as_bytes()).map_err(ConfigEditError::Io)?;
        let overrides = ConfigOverrides {
            path: Some(candidate.path().to_path_buf()),
            ..self.reload.overrides.clone()
        };
        Self::load_with(
            overrides,
            self.reload.environment.clone(),
            &self.reload.current_dir,
        )
        .map_err(ConfigEditError::Invalid)?;
        candidate.commit().map_err(ConfigEditError::Io)
    }

    /// Reads the file's text, refusing a file that holds credentials.
    fn read_editable(path: &Path) -> StdResult<Zeroizing<String>, ConfigEditError> {
        let text = read_text(path).map_err(ConfigEditError::Io)?;
        // A file that does not parse may still hold credentials, so it is not offered either.
        let file = ConfigFile::parse(&text, path)
            .map_err(|error| ConfigEditError::Invalid(error.into()))?;
        if file.has_credentials() {
            return Err(ConfigEditError::HoldsCredentials);
        }
        Ok(text)
    }

    /// Adds a stdio MCP server. A relative working directory is resolved against the process's
    /// current directory, because that is where the user typed it.
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
            let cwd = resolve_path(cwd.to_path_buf(), &self.reload.current_dir);
            let cwd = cwd
                .to_str()
                .ok_or_else(|| ConfigError::McpWorkingDirectoryNotUnicode(cwd.clone()))?;
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

    /// Adds a remote MCP server whose credentials are read from named environment variables.
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

    /// Adds `server` unless a server with `name` is loaded or already present in the file.
    fn add_mcp_server_table(&self, name: &str, server: Table) -> Result<()> {
        let mut document = EditableFile::open(&self.path)?;
        let servers = document.section("mcp_servers");
        if self.mcp_servers.contains_key(name) || servers.contains_key(name) {
            return Err(ConfigError::McpServerExists {
                name: name.to_owned(),
            }
            .into());
        }
        servers.insert(name, Item::Table(server));
        Ok(document.save()?)
    }
}

/// The configuration file parsed for a format-preserving edit.
struct EditableFile<'a> {
    path: &'a Path,
    document: DocumentMut,
}

impl<'a> EditableFile<'a> {
    /// Opens the file, or an empty document when it does not exist.
    fn open(path: &'a Path) -> StdResult<Self, ConfigError> {
        let contents = read_text(path)?;
        let document =
            contents
                .parse::<DocumentMut>()
                .map_err(|source| ConfigError::UpdateParse {
                    path: path.to_path_buf(),
                    // The parser's error retains the source document, which may hold credentials.
                    source: ConfigSyntaxError {
                        message: source.message().to_owned(),
                    },
                })?;
        Ok(Self { path, document })
    }

    /// The top-level table `name`, created when absent.
    fn section(&mut self, name: &str) -> &mut dyn TableLike {
        self.document
            .entry(name)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_like_mut()
            .expect("configuration sections are tables")
    }

    fn save(self) -> StdResult<(), ConfigError> {
        // toml_edit owns non-zeroizing copies of file values while the document is alive.
        // Zeroization covers Tact's input and output buffers and secret owners, not the
        // dependency's transient document representation.
        let rendered = Zeroizing::new(self.document.to_string());
        StagedFile::write(self.path, rendered.as_bytes())?.commit()
    }
}

/// Complete new contents for the configuration file, durably written beside it and not yet
/// visible at its path.
struct StagedFile<'a> {
    destination: &'a Path,
    file: NamedTempFile,
}

impl<'a> StagedFile<'a> {
    fn write(destination: &'a Path, contents: &[u8]) -> StdResult<Self, ConfigError> {
        let write_error = |source| ConfigError::Write {
            path: destination.to_path_buf(),
            source,
        };
        let parent = destination.parent().ok_or_else(|| {
            write_error(io::Error::other(
                "configuration path has no parent directory",
            ))
        })?;
        fs::create_dir_all(parent).map_err(write_error)?;
        let mut file = NamedTempFile::new_in(parent).map_err(write_error)?;
        file.write_all(contents)
            .and_then(|()| file.as_file().sync_all())
            .map_err(write_error)?;
        Ok(Self { destination, file })
    }

    fn path(&self) -> &Path {
        self.file.path()
    }

    /// Atomically replaces the destination with the staged contents.
    fn commit(self) -> StdResult<(), ConfigError> {
        self.file
            .persist(self.destination)
            .map_err(|error| ConfigError::Write {
                path: self.destination.to_path_buf(),
                source: error.error,
            })?;
        Ok(())
    }
}

/// Reads the file's text into a zeroizing buffer; a missing file reads as empty.
fn read_text(path: &Path) -> StdResult<Zeroizing<String>, ConfigError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Zeroizing::new(text)),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(Zeroizing::new(String::new())),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::{
            Config, ConfigOverrides, Environment, MissingFile, ReasoningEffort, ReasoningMode,
            Setting, Speed,
            test_support::{
                assert_error_redacts, load_config_at, remote_memory_config, toml_string,
            },
        },
        error::{ConfigEditError, ConfigError, Error},
        theme::ThemeMode,
    };
    use std::{fs, path::Path};
    use tempfile::tempdir;

    fn editable_config(text: &str) -> (tempfile::TempDir, Config) {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, text).unwrap();
        let config = load_config_at(config_path, directory.path()).unwrap();
        (directory, config)
    }

    #[test]
    fn remote_config_edits_replace_a_current_valid_file() {
        let (_directory, config) = editable_config("[skills]\nenabled = false\n");
        let document = config.document().unwrap();
        assert_eq!(document.text, "[skills]\nenabled = false\n");

        let replacement = "[skills]\nenabled = true\n";
        config
            .replace_document(replacement, &document.revision)
            .unwrap();
        assert_eq!(fs::read_to_string(config.path()).unwrap(), replacement);
        assert!(matches!(
            config.replace_document("", &document.revision),
            Err(ConfigEditError::Stale)
        ));
    }

    #[test]
    fn remote_config_edits_refuse_invalid_text_and_leave_the_file_alone() {
        let original = "[skills]\nenabled = false\n";
        let (directory, config) = editable_config(original);
        let revision = config.document().unwrap().revision;

        for invalid in ["[skills\n", "[agent]\nno_such_setting = 1\n"] {
            assert!(matches!(
                config.replace_document(invalid, &revision),
                Err(ConfigEditError::Invalid(_))
            ));
        }
        assert_eq!(fs::read_to_string(config.path()).unwrap(), original);
        assert_eq!(
            fs::read_dir(directory.path()).unwrap().count(),
            1,
            "a refused candidate leaves no file behind"
        );
    }

    #[test]
    fn remote_config_edits_never_carry_credentials() {
        let (_directory, config) = editable_config("");
        let revision = config.document().unwrap().revision;
        assert!(matches!(
            config.replace_document("[openai]\napi_key = \"sk-test\"\n", &revision),
            Err(ConfigEditError::HoldsCredentials)
        ));

        let (_directory, config) = editable_config("");
        fs::write(config.path(), "[claude]\napi_key = \"sk-test\"\n").unwrap();
        assert!(matches!(
            config.document(),
            Err(ConfigEditError::HoldsCredentials)
        ));
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
        let config = load_config_at(config_path.clone(), directory.path()).unwrap();

        config
            .add_http_mcp_server(
                "docs",
                "https://example.com/mcp",
                Some("MCP_TOKEN"),
                [("X-Tenant", "TENANT_ID")].into_iter(),
            )
            .unwrap();

        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(contents.starts_with("# Keep this comment."));
        assert_eq!(toml_string(&contents, &["agent", "thinking"]), "high");
        assert_eq!(
            toml_string(&contents, &["mcp_servers", "docs", "url"]),
            "https://example.com/mcp"
        );
        assert_eq!(
            toml_string(&contents, &["mcp_servers", "docs", "bearer_token_env_var"]),
            "MCP_TOKEN"
        );
        assert_eq!(
            toml_string(
                &contents,
                &["mcp_servers", "docs", "header_env", "X-Tenant"]
            ),
            "TENANT_ID"
        );

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
            MissingFile::Allowed,
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
        assert_eq!(
            toml_string(&contents, &["mcp_servers", "files.v1", "command"]),
            "npx"
        );
        let loaded = load_config_at(config_path, directory.path()).unwrap();
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
        let config = load_config_at(config_path.clone(), directory.path()).unwrap();
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
        assert!(before_duplicate.starts_with("# Keep this comment."));
        assert_eq!(
            toml_string(&before_duplicate, &["agent", "thinking"]),
            "high"
        );
    }

    #[test]
    fn persisting_a_setting_changes_only_its_key() {
        const COMMENT: &str = "# Keep this comment.";
        let rows: [(Setting, &str, &str, toml::Value, &[&str]); 5] = [
            (
                Setting::Thinking(ReasoningEffort::Xhigh),
                "agent",
                "thinking",
                "xhigh".into(),
                &[],
            ),
            (
                Setting::ReasoningMode(ReasoningMode::Pro),
                "agent",
                "reasoning_mode",
                "pro".into(),
                &[],
            ),
            (
                Setting::Speed(Speed::Ultrafast),
                "agent",
                "speed",
                "ultrafast".into(),
                &["fast_mode"],
            ),
            (
                Setting::MaxSubagents(8),
                "agent",
                "max_subagents",
                8.into(),
                &[],
            ),
            (
                Setting::ThemeMode(ThemeMode::Light),
                "theme",
                "mode",
                "light".into(),
                &[],
            ),
        ];
        for (setting, section, key, value, removed) in rows {
            let directory = tempdir().unwrap();
            let path = directory.path().join("config.toml");
            let original = format!(
                "{COMMENT}\n[agent]\nthinking = 'low'\nreasoning_mode = 'standard'\n\
                 fast_mode = false\nmax_subagents = 32\nweb_search = false\n\n\
                 [theme]\nmode = 'auto'\naccent = '#AABBCC'\n"
            );
            fs::write(&path, &original).unwrap();

            setting.write_to(&path).unwrap();

            let contents = fs::read_to_string(&path).unwrap();
            assert!(contents.starts_with(COMMENT), "{setting:?}");
            let mut expected = toml::from_str::<toml::Value>(&original).unwrap();
            let table = expected[section].as_table_mut().unwrap();
            for key in removed {
                table.remove(*key);
            }
            table.insert(key.to_owned(), value);
            assert_eq!(
                toml::from_str::<toml::Value>(&contents).unwrap(),
                expected,
                "{setting:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn persisting_settings_preserves_literal_credentials_and_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let claude_key = " sk-ant-api-persistence-fixture ";
        let openai_key = "openai-persistence-fixture";
        let memory_token = "memory-persistence-fixture";
        fs::write(
            &path,
            format!(
                "# Keep this comment.\n[openai]\napi_key = '{openai_key}'\n[claude]\napi_key = '{claude_key}'\n\n{}\n\
                 [agent]\nthinking = 'low'\nfast_mode = true\n",
                remote_memory_config("https://memory.example/", "personal", memory_token, ".")
            ),
        )
        .unwrap();
        let config = load_config_at(path.clone(), directory.path()).unwrap();
        config
            .persist(Setting::Thinking(ReasoningEffort::Max))
            .unwrap();
        config.persist(Setting::Speed(Speed::Ultrafast)).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let document: toml::Value = toml::from_str(&contents).unwrap();
        assert!(contents.starts_with("# Keep this comment."));
        assert_eq!(document["claude"]["api_key"].as_str(), Some(claude_key));
        assert_eq!(document["openai"]["api_key"].as_str(), Some(openai_key));
        assert_eq!(document["agent"]["speed"].as_str(), Some("ultrafast"));
        assert!(document["agent"].get("fast_mode").is_none());
        assert_eq!(
            document["memory"]["remote"]["bearer_token"].as_str(),
            Some(memory_token)
        );
        assert_eq!(document["agent"]["thinking"].as_str(), Some("max"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        let reloaded = load_config_at(path, directory.path()).unwrap();
        assert_eq!(
            reloaded.auth().api_key().unwrap().expose_secret(),
            openai_key
        );
        assert_eq!(reloaded.agent().speed(), Speed::Ultrafast);
        assert_eq!(
            reloaded.claude().api_key().unwrap().expose_secret(),
            claude_key
        );
        assert_eq!(
            reloaded.memory().remote().unwrap().bearer_token(),
            memory_token
        );
    }

    #[test]
    fn update_parse_errors_do_not_retain_credentials() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let secret = "secret-sentinel";
        let contents = format!("[claude]\napi_key = '{secret}' trailing\n");
        fs::write(&path, &contents).unwrap();

        let error = Setting::Thinking(ReasoningEffort::Max)
            .write_to(&path)
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Config(ConfigError::UpdateParse { .. })
        ));
        assert_error_redacts(&error, secret);
        assert_eq!(fs::read_to_string(path).unwrap(), contents);
    }

    #[test]
    fn persisting_thinking_creates_a_missing_config_and_parent_directory() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/config.toml");

        Setting::Thinking(ReasoningEffort::Max)
            .write_to(&path)
            .unwrap();

        let document = toml::from_str::<toml::Value>(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            document,
            toml::from_str::<toml::Value>("[agent]\nthinking = 'max'\n").unwrap()
        );
    }
}
