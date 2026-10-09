//! The headless event loop of `tact serve`, driven through its web API.

use super::{EventLoop, Interface, TaskKind, frontend::Frontend, run};
use crate::{
    app::{
        config::{Config, ConfigOverrides},
        error::{Error, RuntimeError},
    },
    core::{pane::PaneId, session::SessionStore},
    tui::{StartupMode, components::RootEffect},
    web::StartError,
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use tempfile::{TempDir, tempdir};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const LOOPBACK_WEB: &str = "enabled = true\nbind = '127.0.0.1'\nport = 0\n";
/// How long any one step may take before the test fails instead of hanging.
const STEP: Duration = Duration::from_secs(20);

/// A Tact home whose configuration serves the web interface as `web` describes. Model
/// requests go to a closed local port, so turns fail without leaving the machine.
fn peer_home(web: &str) -> (TempDir, Config) {
    let home = tempdir().unwrap();
    let path = home.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "[auth]\nmode = 'api-key'\n[openai]\napi_key = 'sk-serve-fixture'\n\
             [agent]\ntransport = 'https'\napi_base_url = 'http://127.0.0.1:9/v1'\n\
             websocket_url = 'ws://127.0.0.1:9/responses'\nweb_search = false\n\
             image_generation = false\n[skills]\nenabled = false\n[memory]\nenabled = false\n\
             [subagents]\nenabled = false\n[web]\n{web}"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config = Config::load(ConfigOverrides {
        path: Some(path),
        auth_file: Some(home.path().join("unused-auth.json")),
        workspace: Some(home.path().to_owned()),
        ..ConfigOverrides::default()
    })
    .unwrap();
    (home, config)
}

fn new_session(config: &Config) -> StartupMode {
    StartupMode::NewSession(config.agent().model())
}

/// An authenticated client of a running peer.
struct Api {
    origin: String,
    cookie: String,
    client: reqwest::Client,
}

impl Api {
    /// Finds the peer through the instance registry in `home` and signs in with its token.
    async fn connect(home: &Path) -> Self {
        let instances = home.join("web/instances");
        let port = timeout(STEP, async {
            loop {
                let port = fs::read_dir(&instances)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                    .find_map(|entry| {
                        let record: Value =
                            serde_json::from_slice(&fs::read(entry.path()).ok()?).ok()?;
                        record["port"].as_u64()
                    });
                if let Some(port) = port {
                    return port;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the peer registers its server");
        let token = fs::read_to_string(home.join("web/token")).unwrap();
        Self {
            origin: format!("http://127.0.0.1:{port}"),
            cookie: format!("tact={}", token.trim()),
            client: reqwest::Client::new(),
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.origin))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
    }

    async fn command(&self, command: &str, args: Value) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}/api/cmd", self.origin))
            .header("cookie", &self.cookie)
            .header("x-tact", "1")
            .json(&json!({"client": 1, "cmd": command, "args": args}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap())
    }

    async fn open_session(&self) -> String {
        let (status, reply) = self.command("open_session", json!({"new": {}})).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        reply["session"].as_str().unwrap().to_owned()
    }

    async fn events(&self) -> Events {
        let response = self.get("/api/stream").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        Events {
            response,
            buffer: String::new(),
        }
    }
}

/// The server-sent events of `/api/stream`.
struct Events {
    response: reqwest::Response,
    buffer: String,
}

impl Events {
    /// The next named event and its data, skipping keep-alive comments.
    async fn next(&mut self) -> (String, Value) {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let message: String = self.buffer.drain(..end + 2).collect();
                let Some((name, data)) = message
                    .strip_prefix("event: ")
                    .and_then(|message| message.split_once("\ndata: "))
                else {
                    continue;
                };
                return (
                    name.to_owned(),
                    serde_json::from_str(data.trim_end()).unwrap(),
                );
            }
            let chunk = timeout(STEP, self.response.chunk())
                .await
                .expect("the stream stays live")
                .unwrap()
                .expect("the stream stays open");
            self.buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }

    /// The first value `select` picks from an event's name and data.
    async fn find<T>(&mut self, select: impl Fn(&str, &Value) -> Option<T>) -> T {
        loop {
            let (event, data) = self.next().await;
            if let Some(found) = select(&event, &data) {
                return found;
            }
        }
    }
}

#[tokio::test]
async fn a_headless_peer_serves_the_web_api_and_journals_web_prompts() {
    let (home, config) = peer_home(LOOPBACK_WEB);
    let config_path = config.path().to_owned();
    let shutdown = CancellationToken::new();
    let peer = run(
        config.clone(),
        new_session(&config),
        shutdown.clone(),
        Interface::Headless,
    );
    let hub = async {
        let api = Api::connect(home.path()).await;
        let instance = api.get("/api/instance").await;
        assert_eq!(instance.status(), StatusCode::OK);
        let instance: Value = instance.json().await.unwrap();
        assert!(instance["protocol_version"].is_u64(), "{instance}");
        let mut events = api.events().await;
        assert_eq!(events.next().await.0, "hello");

        let session = api.open_session().await;
        let (status, _) = api
            .command(
                "set_draft",
                json!({"session": session, "text": "hello from the hub"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        // The draft arrives on its own, or inside the snapshot of the newly active session.
        let rev = events
            .find(|event, data| {
                let draft = match event {
                    "draft" => data,
                    "snapshot" => &data["draft"],
                    _ => return None,
                };
                (data["session"] == session.as_str() && draft["text"] == "hello from the hub")
                    .then(|| draft["rev"].clone())
            })
            .await;
        let (status, reply) = api
            .command("submit", json!({"session": session, "rev": rev}))
            .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        shutdown.cancel();
        session
    };

    let (served, session) = timeout(STEP, async { tokio::join!(peer, hub) })
        .await
        .expect("shutdown completes");

    served.unwrap();
    let prompts = SessionStore::new(&config_path)
        .load_transcript(&session)
        .unwrap()
        .into_records()
        .iter()
        .filter_map(|record| record.prompt_text().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(prompts, ["hello from the hub"]);
}

#[tokio::test]
async fn a_headless_peer_keeps_its_last_live_session_and_keeps_serving() {
    let (home, config) = peer_home(LOOPBACK_WEB);
    let shutdown = CancellationToken::new();
    let peer = run(
        config.clone(),
        new_session(&config),
        shutdown.clone(),
        Interface::Headless,
    );
    let hub = async {
        let api = Api::connect(home.path()).await;
        let mut events = api.events().await;
        let first = events
            .find(|event, data| {
                (event == "live")
                    .then(|| data["active"].as_str().map(str::to_owned))
                    .flatten()
            })
            .await;
        let second = api.open_session().await;
        let close = |session: &str| {
            api.command("close_session", json!({"session": session, "force": true}))
        };

        assert_eq!(close(&first).await.0, StatusCode::OK);
        let (status, refused) = close(&second).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(refused["code"], "invalid_request");
        assert_eq!(
            refused["message"],
            "the last session cannot be closed; stop Tact instead"
        );
        assert_eq!(api.get("/api/instance").await.status(), StatusCode::OK);
        let session = api.open_session().await;
        assert_ne!(session, second);
        shutdown.cancel();
    };

    let (served, ()) = timeout(STEP, async { tokio::join!(peer, hub) })
        .await
        .expect("shutdown completes");

    served.unwrap();
}

#[tokio::test]
async fn a_headless_peer_serves_only_its_api() {
    let (home, config) = peer_home(LOOPBACK_WEB);
    let shutdown = CancellationToken::new();
    let peer = run(
        config.clone(),
        new_session(&config),
        shutdown.clone(),
        Interface::Headless,
    );
    let hub = async {
        let api = Api::connect(home.path()).await;
        for path in ["/", "/index.html"] {
            let response = api.get(path).await;

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            let headers = response.headers();
            assert_eq!(headers["content-type"], "application/json", "{path}");
            assert_eq!(headers["x-content-type-options"], "nosniff", "{path}");
            assert!(
                headers["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .contains("default-src 'none'"),
                "{path}"
            );
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["code"], "unknown_route", "{path}");
        }
        shutdown.cancel();
    };

    let (served, ()) = timeout(STEP, async { tokio::join!(peer, hub) })
        .await
        .expect("shutdown completes");

    served.unwrap();
}

#[tokio::test]
async fn a_headless_peer_that_cannot_listen_fails_before_it_starts_a_session() {
    // TEST-NET-1 is never assigned to a local interface, so binding to it fails.
    let (home, config) = peer_home("enabled = true\nbind = '192.0.2.1'\nport = 0\n");
    let shutdown = CancellationToken::new();

    let result = timeout(
        STEP,
        run(
            config.clone(),
            new_session(&config),
            shutdown.clone(),
            Interface::Headless,
        ),
    )
    .await
    .unwrap();

    assert!(
        matches!(start_error(&result), Some(StartError::Bind { .. })),
        "{result:?}"
    );
    assert!(shutdown.is_cancelled());
    let sessions = SessionStore::new(config.path())
        .list(&[home.path().to_owned()], false)
        .unwrap();
    assert!(sessions.is_empty());
}

#[tokio::test]
async fn a_headless_peer_with_the_web_interface_disabled_does_not_start() {
    let (_home, config) = peer_home("enabled = false\n");

    let result = run(
        config.clone(),
        new_session(&config),
        CancellationToken::new(),
        Interface::Headless,
    )
    .await;

    assert!(
        matches!(start_error(&result), Some(StartError::Disabled)),
        "{result:?}"
    );
}

fn start_error<T>(result: &Result<T, Error>) -> Option<&StartError> {
    match result {
        Err(Error::Runtime(RuntimeError::Web(error))) => error.downcast_ref(),
        _ => None,
    }
}

#[tokio::test]
async fn terminal_only_effects_are_refused_without_a_terminal() {
    let refusal = Frontend::Headless.session().err().unwrap();
    assert_eq!(refusal.code(), "invalid_request");

    let (_home, config) = peer_home(LOOPBACK_WEB);
    let shutdown = CancellationToken::new();
    let (mut event_loop, mut inbox, web_server) = EventLoop::start(
        config.clone(),
        new_session(&config),
        shutdown.clone(),
        Interface::Headless,
    )
    .await
    .unwrap();

    for effect in [
        RootEffect::OpenDraftEditor,
        RootEffect::OpenConfigEditor,
        RootEffect::Copy("text".to_owned()),
        RootEffect::CopyWebLink,
    ] {
        event_loop.apply_pane_effect(PaneId::Main, effect).unwrap();
    }

    assert!(!event_loop.tasks.is_active(TaskKind::Editor));
    shutdown.cancel();
    timeout(STEP, event_loop.serve(&mut inbox))
        .await
        .expect("shutdown completes")
        .unwrap();
    web_server.stop().await;
}
