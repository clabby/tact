//! The effective terminal interface section.

use super::file::TuiConfigFile;
use serde::Serialize;
use std::num::NonZeroU16;

/// Effective terminal interface configuration.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct TuiConfig {
    pub(crate) mouse_scroll_lines: NonZeroU16,
}

impl TuiConfig {
    pub(super) fn new(file: TuiConfigFile) -> Self {
        Self {
            mouse_scroll_lines: file
                .mouse_scroll_lines
                .unwrap_or_else(|| Self::default().mouse_scroll_lines),
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            mouse_scroll_lines: NonZeroU16::new(3).expect("3 is non-zero"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::test_support::load_config,
        error::{ConfigError, Error},
    };

    #[test]
    fn tui_mouse_scroll_lines_can_be_configured_and_rendered() {
        let config = load_config("[tui]\nmouse_scroll_lines = 5\n").unwrap();

        assert_eq!(config.tui().mouse_scroll_lines.get(), 5);
        let rendered_toml = config.to_toml().unwrap();
        let rendered: toml::Value = toml::from_str(&rendered_toml).unwrap();
        assert_eq!(rendered["tui"]["mouse_scroll_lines"].as_integer(), Some(5));
        assert_eq!(
            load_config(&rendered_toml)
                .unwrap()
                .tui()
                .mouse_scroll_lines
                .get(),
            5
        );
    }

    #[test]
    fn invalid_tui_mouse_scroll_lines_are_rejected() {
        for contents in [
            "[tui]\nmouse_scroll_lines = 0\n",
            "[tui]\nmouse_scroll_lines = -1\n",
            "[tui]\nmouse_scroll_lines = 65536\n",
            "[tui]\nunknown = true\n",
        ] {
            let error = load_config(contents).unwrap_err();
            assert!(matches!(error, Error::Config(ConfigError::Parse { .. })));
        }
    }
}
