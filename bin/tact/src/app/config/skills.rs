//! The effective skills section: where local model skills are discovered.

use super::{Environment, file::SkillsConfigFile, resolve_path};
use serde::Serialize;
use std::path::{Path, PathBuf};

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

impl SkillsConfig {
    /// Enabling skills adds the standard Codex and agent skill roots to the configured ones.
    /// Configured roots are relative to the configuration file's directory.
    pub(super) fn new(
        file: SkillsConfigFile,
        config_dir: &Path,
        environment: &Environment,
    ) -> Self {
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
                .map(|root| resolve_path(root, config_dir)),
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

#[cfg(test)]
mod tests {
    use crate::app::config::{
        Config, ConfigOverrides, Environment,
        test_support::{load_config_at, load_default},
    };
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn skills_are_disabled_by_default_without_discovery_roots() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let config = load_default(
            Environment {
                codex_home: Some(directory.path().join("codex")),
                home: Some(home),
                ..Environment::default()
            },
            directory.path(),
        )
        .unwrap();

        assert!(!config.skills().enabled());
        assert!(config.skills().roots().is_empty());
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

        assert!(config.skills().enabled());
        assert_eq!(
            config.skills().roots(),
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
        let config = load_config_at(config_path.clone(), directory.path()).unwrap();

        fs::write(
            &config_path,
            "[skills]\nenabled = true\nroots = [\"extra\"]\n",
        )
        .unwrap();
        let (reloaded, _) = config.reload().unwrap().into_parts();

        assert!(reloaded.skills().enabled());
        assert_eq!(
            reloaded.skills().roots(),
            [
                directory.path().join("codex/skills"),
                directory.path().join("extra")
            ]
        );
    }
}
