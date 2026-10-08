use super::{ConfiguredAgent, set_speed};
use crate::{
    app::config::{Config, ConfigOverrides, ReasoningMode, Speed, Transport},
    core::session::SessionStore,
};
use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use nanocodex::{HarnessModel, Model, Nanocodex};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::{TempDir, tempdir};
use tokio::{net::TcpListener, process::Command, task::JoinHandle, time::timeout};

const ISOLATED_TEST: &str = "TACT_OPENAI_ISOLATED_TEST";
const FIXTURE_KEY: &str = "sk-fixture-openai-config-only";

#[derive(Clone)]
struct Provider(Arc<Mutex<ProviderState>>);

#[derive(Default)]
struct ProviderState {
    requests: Vec<CapturedRequest>,
    next_response_id: u64,
}

struct CapturedRequest {
    authorized: bool,
    transport: Transport,
    body: Value,
}

impl Provider {
    fn respond(&self, headers: &HeaderMap, transport: Transport, body: Value) -> Value {
        let authorized = headers
            .get(header::AUTHORIZATION)
            .is_some_and(|value| value.as_bytes() == format!("Bearer {FIXTURE_KEY}").as_bytes());
        let warmup = body["generate"] == false;
        let mut state = self.0.lock().unwrap();
        let id = format!("resp-fixture-{}", state.next_response_id);
        state.next_response_id += 1;
        state.requests.push(CapturedRequest {
            authorized,
            transport,
            body,
        });
        if warmup {
            return json!({"type":"response.completed", "response":{"id":id, "usage":null}});
        }
        json!({
            "type": "response.completed",
            "response": {
                "id": id,
                "status": "completed",
                "output": [{
                    "id": format!("msg-{id}"),
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type":"output_text", "text":"done"}]
                }],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 2,
                    "total_tokens": 12
                }
            }
        })
    }
}

async fn http_response(
    State(provider): State<Provider>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let completed = provider.respond(&headers, Transport::Https, body);
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        format!("event: response.completed\ndata: {completed}\n\n"),
    )
        .into_response()
}

async fn websocket_response(
    State(provider): State<Provider>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            match message {
                Message::Text(text) => {
                    let body = serde_json::from_str(&text).unwrap();
                    let completed = provider.respond(&headers, Transport::Websocket, body);
                    if socket
                        .send(Message::Text(completed.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Ping(bytes) => {
                    if socket.send(Message::Pong(bytes)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Pong(_) => {}
                other => panic!("unexpected fixture frame: {other:?}"),
            }
        }
    })
}

struct Fixture {
    origin: String,
    provider: Provider,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = listener.local_addr().unwrap().to_string();
        let provider = Provider(Arc::default());
        let router = Router::new()
            .route("/v1/responses", post(http_response))
            .route("/responses", get(websocket_response))
            .with_state(provider.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            origin,
            provider,
            server,
        }
    }

    fn config(
        &self,
        directory: &TempDir,
        model: Model,
        transport: Transport,
        speed: Speed,
    ) -> Config {
        let path = directory.path().join("config.toml");
        let transport_name = match transport {
            Transport::Https => "https",
            Transport::Websocket => "websocket",
        };
        fs::write(&path, format!(
            "[auth]\nmode = 'api-key'\n[openai]\napi_key = '{FIXTURE_KEY}'\n\
             [agent]\nspeed = '{}'\nthinking = 'high'\ntransport = '{transport_name}'\n\
             api_base_url = 'http://{}/v1'\nwebsocket_url = 'ws://{}/responses'\n\
             web_search = false\nimage_generation = false\ninstructions = 'Keep this shared prefix stable.'\n\
             [skills]\nenabled = false\n[memory]\nenabled = false\n[subagents]\nenabled = false\n",
            speed.as_str(), self.origin, self.origin,
        )).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        Config::load(ConfigOverrides {
            path: Some(path),
            auth_file: Some(directory.path().join("unused-auth.json")),
            workspace: Some(directory.path().to_owned()),
            model: Some(HarnessModel::Codex(model)),
            ..ConfigOverrides::default()
        })
        .unwrap()
    }

    fn take_requests(&self) -> Vec<CapturedRequest> {
        std::mem::take(&mut self.provider.0.lock().unwrap().requests)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn configured(config: &Config, model: Model) -> ConfiguredAgent {
    ConfiguredAgent::from_config_with_model(
        config,
        config.agent().thinking(),
        ReasoningMode::Standard,
        HarnessModel::Codex(model),
    )
    .unwrap()
}

async fn prompt(agent: &Nanocodex, text: &str) {
    let result = timeout(Duration::from_secs(10), async {
        agent.prompt(text).await.unwrap().result().await.unwrap()
    })
    .await
    .expect("configured OpenAI turn timed out");
    assert_eq!(result.final_message(), "done");
}

fn assert_policy(
    requests: &[CapturedRequest],
    model: Model,
    transport: Transport,
    tier: Option<&str>,
) {
    assert!(
        !requests.is_empty(),
        "turn did not reach the loopback provider"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.body["generate"] != false)
    );
    for request in requests {
        assert!(
            request.authorized,
            "configured API key was not used for authorization"
        );
        assert_eq!(request.transport, transport);
        assert_eq!(request.body["model"], model.as_str());
        assert_eq!(request.body["service_tier"], json!(tier));
        assert_eq!(request.body["reasoning"]["effort"], "high");
        assert!(request.body.get("prompt_cache_options").is_none());
        assert!(
            request.body["prompt_cache_key"]
                .as_str()
                .is_some_and(|key| !key.is_empty())
        );
    }
}

fn assert_cache_lineage(requests: &[CapturedRequest]) {
    let key = &requests[0].body["prompt_cache_key"];
    let mut prefixes = Vec::new();
    for request in requests {
        assert_eq!(&request.body["prompt_cache_key"], key);
        let input = request.body["input"].as_array().unwrap();
        if input
            .first()
            .is_some_and(|item| item["type"] == "additional_tools")
        {
            assert!(input[0]["id"].as_str().unwrap().starts_with("at_"));
            assert!(input[1]["id"].as_str().unwrap().starts_with("msg_"));
            prefixes.push([input[0].clone(), input[1].clone()]);
        }
    }
    assert!(
        prefixes.len() >= 2,
        "expected shared prefixes from multiple drivers"
    );
    for prefix in &prefixes[1..] {
        assert_eq!(prefix, &prefixes[0]);
    }
}

// Each child test process owns its environment and temporary homes. The parent never reads
// credentials or changes process-global variables while other tests are running.
async fn run_isolated(test_name: &str) -> bool {
    if std::env::var(ISOLATED_TEST).as_deref() == Ok(test_name) {
        assert!(std::env::var_os("OPENAI_API_KEY").is_none());
        let _ = rustls::crypto::ring::default_provider().install_default();
        return false;
    }
    let directory = tempdir().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--nocapture"])
        .env_clear()
        .env(ISOLATED_TEST, test_name)
        .env("HOME", directory.path())
        .env("CODEX_HOME", directory.path().join("codex"))
        .env("TACT_HOME", directory.path().join("tact"))
        .env("TMPDIR", directory.path())
        .env("TMP", directory.path())
        .env("TEMP", directory.path())
        .current_dir(directory.path())
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(45), command.output())
        .await
        .expect("isolated OpenAI test timed out")
        .unwrap();
    assert!(
        output.status.success(),
        "isolated OpenAI test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    true
}

#[tokio::test]
async fn custom_workspace_reaches_the_agent_checkpoint() {
    if run_isolated("core::openai_tests::custom_workspace_reaches_the_agent_checkpoint").await {
        return;
    }
    let fixture = Fixture::start().await;
    let directory = tempdir().unwrap();
    let selected = tempdir().unwrap();
    let selected = selected.path().canonicalize().unwrap();
    let config = fixture.config(&directory, Model::Sol, Transport::Https, Speed::Standard);
    let scoped = config.with_workspace(selected.clone());
    let agent = configured(&scoped, Model::Sol);
    prompt(&agent.agent, "Inspect this checkout").await;
    let snapshot = agent.agent.snapshot().await.unwrap();
    assert_eq!(
        serde_json::to_value(snapshot).unwrap()["workspace"],
        selected.to_string_lossy().as_ref()
    );
    assert_eq!(config.agent().workspace(), directory.path());
    agent.shutdown().await.unwrap();
}

#[tokio::test]
async fn configured_api_key_and_model_tiers_reach_both_transports() {
    if run_isolated("core::openai_tests::configured_api_key_and_model_tiers_reach_both_transports")
        .await
    {
        return;
    }
    let fixture = Fixture::start().await;
    for transport in [Transport::Https, Transport::Websocket] {
        for model in [Model::Astra, Model::Sol, Model::Luna] {
            let policies = [
                (Speed::Standard, None),
                (Speed::Fast, Some("priority")),
                (
                    Speed::Ultrafast,
                    if model == Model::Astra {
                        Some("ultrafast")
                    } else {
                        Some("priority")
                    },
                ),
            ];
            for (speed, tier) in policies {
                let directory = tempdir().unwrap();
                let config = fixture.config(&directory, model, transport, speed);
                let agent = configured(&config, model);
                prompt(&agent.agent, "configured policy turn").await;
                assert_policy(&fixture.take_requests(), model, transport, tier);
                if speed == Speed::Standard {
                    for (requested, expected_tier) in &policies[1..] {
                        set_speed(&agent.agent, HarnessModel::Codex(model), *requested)
                            .await
                            .unwrap();
                        prompt(&agent.agent, "updated policy turn").await;
                        assert_policy(&fixture.take_requests(), model, transport, *expected_tier);
                    }
                }
                agent.shutdown().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn configured_lineage_and_effort_survive_speed_changes_children_and_restore() {
    if run_isolated("core::openai_tests::configured_lineage_and_effort_survive_speed_changes_children_and_restore").await {
        return;
    }
    let fixture = Fixture::start().await;
    for transport in [Transport::Https, Transport::Websocket] {
        let directory = tempdir().unwrap();
        let mut config = fixture.config(&directory, Model::Astra, transport, Speed::Ultrafast);
        let root = configured(&config, Model::Astra);
        prompt(&root.agent, "first root turn").await;
        let mut lineage = fixture.take_requests();
        prompt(&root.agent, "second root turn").await;
        lineage.extend(fixture.take_requests());
        assert_policy(&lineage, Model::Astra, transport, Some("ultrafast"));

        let (child, child_events) = root.agent.spawn().await.unwrap();
        prompt(&child, "clean child turn").await;
        let child_requests = fixture.take_requests();
        assert_policy(&child_requests, Model::Astra, transport, Some("ultrafast"));
        lineage.extend(child_requests);
        child.shutdown().await.unwrap();
        drop(child_events);

        let (fork, fork_events) = root.agent.fork().await.unwrap();
        prompt(&fork, "fork turn").await;
        let fork_requests = fixture.take_requests();
        assert_policy(&fork_requests, Model::Astra, transport, Some("ultrafast"));
        lineage.extend(fork_requests);
        fork.shutdown().await.unwrap();
        drop(fork_events);

        let session_id = root.agent.session_id().to_owned();
        let snapshot = root.agent.snapshot().await.unwrap();
        SessionStore::new(config.path())
            .save_checkpoint(&session_id, &snapshot, &root.instructions, true)
            .unwrap();
        root.shutdown().await.unwrap();
        config.set_speed(Speed::Fast);
        let restored = ConfiguredAgent::from_config_with_session(
            &config,
            config.agent().thinking(),
            ReasoningMode::Standard,
            HarnessModel::Codex(Model::Astra),
            Some(&session_id),
            Some(
                SessionStore::new(config.path())
                    .load_checkpoint(&session_id)
                    .unwrap(),
            ),
        )
        .unwrap();
        prompt(&restored.agent, "restored root turn").await;
        let restored_requests = fixture.take_requests();
        assert_policy(
            &restored_requests,
            Model::Astra,
            transport,
            Some("priority"),
        );
        lineage.extend(restored_requests);

        set_speed(
            &restored.agent,
            HarnessModel::Codex(Model::Astra),
            Speed::Standard,
        )
        .await
        .unwrap();
        prompt(&restored.agent, "standard root turn").await;
        let standard_requests = fixture.take_requests();
        assert_policy(&standard_requests, Model::Astra, transport, None);
        lineage.extend(standard_requests);
        assert_cache_lineage(&lineage);
        restored.shutdown().await.unwrap();
    }
}
