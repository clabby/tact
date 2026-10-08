//! The effective web interface section: where the server listens and how it is exposed.

use super::file::WebConfigFile;
use crate::app::error::{ConfigError, Result};
use serde::Serialize;
use std::{
    net::{IpAddr, Ipv4Addr},
    num::NonZeroUsize,
};

const DEFAULT_PORT: u16 = 7878;
const DEFAULT_MAX_LIVE_SESSIONS: NonZeroUsize = NonZeroUsize::new(8).expect("8 is non-zero");

/// Effective web interface configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct WebConfig {
    enabled: bool,
    bind: IpAddr,
    port: u16,
    /// Empty when unset, so the materialized defaults list every key.
    public_url: String,
    /// Publishes the server to the tailnet with `tailscale serve` while this process runs.
    /// Mutually exclusive with `public_url`.
    tailscale: bool,
    max_live_sessions: NonZeroUsize,
}

impl WebConfig {
    /// The server is enabled on loopback unless the CLI or the file says otherwise. A blank
    /// public URL is unset.
    pub(super) fn new(file: WebConfigFile, enabled_override: Option<bool>) -> Result<Self> {
        let public_url = file.public_url.unwrap_or_default().trim().to_owned();
        let tailscale = file.tailscale.unwrap_or(false);
        if tailscale && !public_url.is_empty() {
            return Err(ConfigError::WebExposureConflict.into());
        }
        Ok(Self {
            enabled: enabled_override.or(file.enabled).unwrap_or(true),
            bind: file.bind.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            port: file.port.unwrap_or(DEFAULT_PORT),
            public_url,
            tailscale,
            max_live_sessions: file.max_live_sessions.unwrap_or(DEFAULT_MAX_LIVE_SESSIONS),
        })
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) const fn bind(&self) -> IpAddr {
        self.bind
    }

    /// The first port the server tries.
    pub(crate) const fn port(&self) -> u16 {
        self.port
    }

    /// The externally reachable origin used only to build copyable links.
    pub(crate) fn public_url(&self) -> Option<&str> {
        Some(self.public_url.as_str()).filter(|url| !url.is_empty())
    }

    /// Whether the server publishes itself to the tailnet while this process runs.
    pub(crate) const fn tailscale(&self) -> bool {
        self.tailscale
    }

    /// The most live sessions one process may host, across the terminal and the web interface.
    pub(crate) const fn max_live_sessions(&self) -> usize {
        self.max_live_sessions.get()
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::test_support::load_config,
        error::{ConfigError, Error},
    };

    #[test]
    fn web_interface_is_enabled_on_loopback_by_default() {
        let config = load_config("").unwrap();
        assert!(config.web().enabled());
        assert!(config.web().bind().is_loopback());
        assert_eq!(config.web().port(), 7878);
        assert_eq!(config.web().public_url(), None);
        assert!(!config.web().tailscale());
        assert_eq!(config.web().max_live_sessions(), 8);

        let config = load_config(
            "[web]\nenabled = false\nport = 9000\npublic_url = \"https://host.ts.net\"\nmax_live_sessions = 3\n",
        )
        .unwrap();
        assert!(!config.web().enabled());
        assert_eq!(config.web().port(), 9000);
        assert_eq!(config.web().public_url(), Some("https://host.ts.net"));
        assert_eq!(config.web().max_live_sessions(), 3);
        assert!(load_config("[web]\nmax_live_sessions = 0\n").is_err());
    }

    #[test]
    fn web_tailscale_and_public_url_are_mutually_exclusive() {
        let config = load_config("[web]\ntailscale = true\n").unwrap();
        assert!(config.web().tailscale());
        assert_eq!(config.web().public_url(), None);

        let config = load_config("[web]\npublic_url = \"https://host.example\"\n").unwrap();
        assert!(!config.web().tailscale());

        // A blank public_url is unset, so it does not conflict.
        assert!(load_config("[web]\ntailscale = true\npublic_url = \" \"\n").is_ok());
        assert!(matches!(
            load_config("[web]\ntailscale = true\npublic_url = \"https://host.example\"\n"),
            Err(Error::Config(ConfigError::WebExposureConflict))
        ));
    }
}
