//! The effective web interface section: where the server listens, how it is exposed, and the
//! monitor model that triages the live sessions while a browser shows the Monitor.

use super::{
    Config,
    file::{WebConfigFile, WebMonitorConfigFile},
    settings::ReasoningEffort,
};
use crate::app::{
    error::{ConfigError, Result},
    model,
};
use clap::ValueEnum;
use serde::{Serialize, Serializer};
use std::{
    net::{IpAddr, Ipv4Addr},
    num::NonZeroUsize,
    time::Duration,
};
use tact_subagents::HarnessModel as Model;

const DEFAULT_PORT: u16 = 7878;
const DEFAULT_MAX_LIVE_SESSIONS: NonZeroUsize = NonZeroUsize::new(8).expect("8 is non-zero");
const DEFAULT_MONITOR_INTERVAL_SECONDS: u64 = 120;
/// The shortest interval between automatic checks, matching the shortest time between checks a
/// browser can ask for explicitly.
const MIN_MONITOR_INTERVAL_SECONDS: u64 = 10;

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
    monitor: MonitorConfig,
}

/// Effective `[web.monitor]` configuration.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct MonitorConfig {
    enabled: bool,
    /// `None` uses Tact's default model (`[agent] model`). Renders as an empty string, so the
    /// materialized defaults list every key.
    #[serde(serialize_with = "serialize_optional_model")]
    model: Option<Model>,
    /// `None` uses `[agent] thinking` with the inherited model, else the configured model's
    /// catalog default. Renders as an empty string.
    #[serde(serialize_with = "serialize_optional_effort")]
    thinking: Option<ReasoningEffort>,
    interval_seconds: u64,
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
            monitor: MonitorConfig::new(file.monitor)?,
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

    pub(crate) const fn monitor(&self) -> &MonitorConfig {
        &self.monitor
    }
}

impl MonitorConfig {
    /// Blank values inherit. Whether the chosen model has credentials is decided when the
    /// monitor runs, not here.
    fn new(file: WebMonitorConfigFile) -> Result<Self> {
        let model = match file.model.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(id) => Some(model::parse(id).map_err(ConfigError::MonitorModel)?),
        };
        let thinking = match file.thinking.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(effort) => Some(
                ReasoningEffort::from_str(effort, true)
                    .map_err(|_| ConfigError::MonitorThinking(effort.to_owned()))?,
            ),
        };
        let interval_seconds = file
            .interval_seconds
            .unwrap_or(DEFAULT_MONITOR_INTERVAL_SECONDS);
        if interval_seconds < MIN_MONITOR_INTERVAL_SECONDS {
            return Err(ConfigError::MonitorInterval {
                minimum: MIN_MONITOR_INTERVAL_SECONDS,
            }
            .into());
        }
        Ok(Self {
            enabled: file.enabled.unwrap_or(true),
            model,
            thinking,
            interval_seconds,
        })
    }

    /// Whether the monitor model checks the sessions while a browser shows the Monitor.
    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    /// The shortest time between automatic checks.
    pub(crate) const fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_seconds)
    }
}

impl Config {
    /// The model and reasoning effort the monitor uses, chosen like Tact's default model: the
    /// configured `[web.monitor]` values, else `[agent] model` and `[agent] thinking` (after
    /// command-line overrides). An explicit model without an explicit effort runs at its catalog
    /// default effort rather than one chosen for another model.
    pub(crate) fn monitor_model(&self) -> (Model, ReasoningEffort) {
        let monitor = self.web().monitor();
        match monitor.model {
            Some(model) => (
                model,
                monitor
                    .thinking
                    .unwrap_or_else(|| ReasoningEffort::default_for(model)),
            ),
            None => (
                self.agent().model(),
                monitor.thinking.unwrap_or_else(|| self.agent().thinking()),
            ),
        }
    }
}

fn serialize_optional_model<S: Serializer>(
    model: &Option<Model>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(model.map_or("", Model::as_str))
}

fn serialize_optional_effort<S: Serializer>(
    effort: &Option<ReasoningEffort>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(effort.map_or("", ReasoningEffort::as_str))
}

#[cfg(test)]
mod tests {
    use crate::app::{
        config::{ReasoningEffort, test_support::load_config},
        error::{ConfigError, Error},
    };
    use nanocode::Model as CodexModel;
    use std::time::Duration;
    use tact_subagents::HarnessModel as Model;

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

    #[test]
    fn the_monitor_inherits_tacts_default_model_and_checks_every_two_minutes() {
        let config = load_config("").unwrap();
        let monitor = config.web().monitor();
        assert!(monitor.enabled());
        assert_eq!(monitor.interval(), Duration::from_secs(120));
        assert_eq!(
            config.monitor_model(),
            (config.agent().model(), config.agent().thinking())
        );

        let config =
            load_config("[agent]\nmodel = \"astra\"\nthinking = \"high\"\n[web.monitor]\nmodel = \"\"\nthinking = \"\"\n")
                .unwrap();
        assert_eq!(
            config.monitor_model(),
            (Model::Codex(CodexModel::Astra), ReasoningEffort::High),
            "blank values inherit [agent] model and thinking"
        );
    }

    #[test]
    fn an_explicit_monitor_model_runs_at_its_catalog_effort_unless_thinking_is_set() {
        let config = load_config(
            "[agent]\nthinking = \"max\"\n[web.monitor]\nenabled = false\nmodel = \"luna\"\ninterval_seconds = 10\n",
        )
        .unwrap();
        let monitor = config.web().monitor();
        assert!(!monitor.enabled());
        assert_eq!(monitor.interval(), Duration::from_secs(10));
        let luna = Model::Codex(CodexModel::Luna);
        assert_eq!(
            config.monitor_model(),
            (luna, ReasoningEffort::default_for(luna)),
            "the agent's effort was chosen for another model"
        );

        let config =
            load_config("[web.monitor]\nmodel = \"luna\"\nthinking = \"xhigh\"\n").unwrap();
        assert_eq!(config.monitor_model(), (luna, ReasoningEffort::Xhigh));
        let config =
            load_config("[agent]\nmodel = \"astra\"\n[web.monitor]\nthinking = \"low\"\n").unwrap();
        assert_eq!(
            config.monitor_model(),
            (Model::Codex(CodexModel::Astra), ReasoningEffort::Low)
        );
    }

    #[test]
    fn invalid_monitor_models_efforts_and_short_intervals_are_configuration_errors() {
        assert!(matches!(
            load_config("[web.monitor]\nmodel = \"terra\"\n"),
            Err(Error::Config(ConfigError::MonitorModel(_)))
        ));
        assert!(matches!(
            load_config("[web.monitor]\nthinking = \"extreme\"\n"),
            Err(Error::Config(ConfigError::MonitorThinking(_)))
        ));
        assert!(matches!(
            load_config("[web.monitor]\ninterval_seconds = 9\n"),
            Err(Error::Config(ConfigError::MonitorInterval { minimum: 10 }))
        ));
        assert!(load_config("[web.monitor]\nunknown = 1\n").is_err());
    }
}
