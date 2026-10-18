//! The model that checks the sessions and the settings it runs under, behind a trait so tests can
//! answer for it.

use crate::{
    app::config::Config,
    core::direct::{self, DirectError, DirectRequest},
};
use futures_util::future::BoxFuture;
use nanocodex::{HarnessModel as Model, Thinking};
use std::time::Duration;
use tokio::sync::watch;

const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
/// The reply budget of Claude checks.
const MAX_CHECK_TOKENS: u32 = 12_288;

/// The monitor's settings as currently configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::web) struct Settings {
    pub(in crate::web) enabled: bool,
    /// The shortest time between automatic checks.
    pub(in crate::web) interval: Duration,
}

/// The model a check runs on and its reasoning effort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::web) struct Choice {
    pub(in crate::web) model: Model,
    pub(in crate::web) thinking: Thinking,
}

pub(in crate::web) trait MonitorModel: Send + Sync + 'static {
    /// The current settings. Cheap enough to read on every scheduling decision.
    fn settings(&self) -> Settings;

    /// The model that would run the next check, or a sentence saying why none can.
    fn resolve(&self) -> BoxFuture<'static, Result<Choice, String>>;

    /// Asks the chosen model to answer `input` under `instructions`.
    fn complete(
        &self,
        choice: Choice,
        instructions: &'static str,
        input: String,
    ) -> BoxFuture<'static, Result<String, DirectError>>;
}

/// The monitor as the configuration describes it, called with this process's credentials.
pub(in crate::web) struct ConfiguredModel {
    /// The terminal loop's current configuration, replaced when it reloads, so every check runs
    /// with the settings in force when it starts.
    config: watch::Receiver<Config>,
}

impl ConfiguredModel {
    pub(in crate::web) const fn new(config: watch::Receiver<Config>) -> Self {
        Self { config }
    }
}

impl MonitorModel for ConfiguredModel {
    fn settings(&self) -> Settings {
        let config = self.config.borrow();
        let monitor = config.web().monitor();
        Settings {
            enabled: monitor.enabled(),
            interval: monitor.interval(),
        }
    }

    fn resolve(&self) -> BoxFuture<'static, Result<Choice, String>> {
        let config = self.config.borrow().clone();
        Box::pin(async move {
            // Loading credentials reads files.
            tokio::task::spawn_blocking(move || {
                let (model, effort) = config.monitor_model();
                direct::ensure_available(&config, model)
                    .map(|()| Choice {
                        model,
                        thinking: effort.into(),
                    })
                    .map_err(|error| {
                        format!("{error} Set [web.monitor] model to a model this Tact can use.")
                    })
            })
            .await
            .unwrap_or_else(|error| Err(format!("The monitor could not check its model: {error}")))
        })
    }

    fn complete(
        &self,
        choice: Choice,
        instructions: &'static str,
        input: String,
    ) -> BoxFuture<'static, Result<String, DirectError>> {
        let config = self.config.borrow().clone();
        Box::pin(async move {
            let request = DirectRequest {
                model: choice.model,
                instructions,
                input: &input,
                thinking: choice.thinking,
                max_tokens: MAX_CHECK_TOKENS,
                timeout: CHECK_TIMEOUT,
            };
            direct::complete(&config, &request).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfiguredModel, MonitorModel, Settings};
    use crate::app::config::{Config, ConfigOverrides};
    use axum::{Json, Router, extract::Path as UrlPath, routing::post};
    use serde_json::{Value, json};
    use std::{
        fs,
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tempfile::tempdir;
    use tokio::{net::TcpListener, sync::watch};

    fn load(directory: &Path, text: &str) -> Config {
        let path = directory.join("config.toml");
        fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        Config::load(ConfigOverrides {
            path: Some(path),
            workspace: Some(directory.to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn the_model_and_settings_follow_configuration_reloads() {
        let directory = tempdir().unwrap();
        let (updates, config) = watch::channel(load(
            directory.path(),
            "[web.monitor]\nmodel = \"haiku\"\ninterval_seconds = 30\n",
        ));
        let model = ConfiguredModel::new(config);
        assert_eq!(
            model.settings(),
            Settings {
                enabled: true,
                interval: Duration::from_secs(30),
            }
        );
        let unavailable = model.resolve().await.unwrap_err();
        assert!(
            unavailable.starts_with("Haiku 5.5 needs [claude] enabled = true")
                && unavailable.ends_with("Set [web.monitor] model to a model this Tact can use."),
            "{unavailable}"
        );

        updates.send_replace(load(
            directory.path(),
            "[web.monitor]\nenabled = false\nmodel = \"mythos\"\ninterval_seconds = 10\n",
        ));
        assert_eq!(
            model.settings(),
            Settings {
                enabled: false,
                interval: Duration::from_secs(10),
            }
        );
        let unavailable = model.resolve().await.unwrap_err();
        assert!(
            unavailable.starts_with("Mythos 5.1 is access-gated"),
            "{unavailable}"
        );
    }

    #[tokio::test]
    async fn a_request_sent_after_a_reload_uses_the_reloaded_configuration() {
        crate::install_tls_provider();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let app = Router::new().route(
            "/{endpoint}/messages",
            post(move |UrlPath(endpoint): UrlPath<String>| async move {
                recorded.lock().unwrap().push(endpoint);
                Json(json!({
                    "id": "msg_1", "role": "assistant", "model": "claude-haiku-5-5",
                    "content": [{"type": "text", "text": "{\"sessions\": []}"}],
                    "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1},
                }))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let directory = tempdir().unwrap();
        let config = |endpoint: &str| {
            load(
                directory.path(),
                &format!(
                    "[claude]\nenabled = true\napi_key = \"sk-ant-api03-test\"\napi_base_url = \"http://{address}/{endpoint}\"\n[web.monitor]\nmodel = \"haiku\"\n"
                ),
            )
        };
        let (updates, current) = watch::channel(config("before"));
        let model = ConfiguredModel::new(current);
        let choice = model.resolve().await.unwrap();

        updates.send_replace(config("after"));
        let reply = model
            .complete(choice, "Check the sessions.", "{}".to_owned())
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap(),
            json!({"sessions": []})
        );
        assert_eq!(*requests.lock().unwrap(), ["after"]);
    }
}
