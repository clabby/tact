//! HTTP surface of the web server: authentication, hardening, reads, commands, the event stream,
//! and static assets. Review routes live in [`super::review`].

use super::{
    assets::AssetStore,
    bridge::{ClientId, Command, CommandError, Request},
    hub::Hub,
    registry::{self, InstanceRecord},
    token::MachineToken,
    wire::PROTOCOL_VERSION,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, FromRequest, Path, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri, header},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::{StreamExt as _, future::join_all, stream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::{mpsc, oneshot}, time::{interval, timeout}};
use tokio_util::sync::CancellationToken;

const MAX_BODY_BYTES: usize = 1024 * 1024;
const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// Longest a command may wait for the terminal loop. Opening a large session can be slow.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const SIBLING_TIMEOUT: Duration = Duration::from_millis(500);
const COOKIE_NAME: &str = "tact";
const ENTRY_NOT_FOUND: &str = "the entry does not exist";

/// Everything the handlers share. Built once the listener is bound.
pub(super) struct AppState {
    pub(super) token: MachineToken,
    pub(super) hub: Hub,
    pub(super) requests: mpsc::UnboundedSender<Request>,
    pub(super) workspace: PathBuf,
    pub(super) port: u16,
    pub(super) registry_directory: PathBuf,
    pub(super) assets: AssetStore,
    pub(super) client: reqwest::Client,
    pub(super) shutdown: CancellationToken,
}

/// Builds the router. `extra` routes are served behind the same guard.
pub(super) fn router(state: Arc<AppState>, extra: Router<Arc<AppState>>) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/instance", get(instance))
        .route("/api/instances", get(instances))
        .route("/api/sessions/{session}/entries/{entry}", get(entry_detail))
        .route("/api/stream", get(stream_events))
        .route("/api/cmd", post(command))
        .merge(extra)
        .fallback(static_asset)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(Arc::clone(&state), guard))
        .with_state(state)
}

/// An API failure: a status, a stable machine-readable code, and a human message.
pub(super) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub(super) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "authentication required")
    }
}

impl From<CommandError> for ApiError {
    /// The single mapping from the loop's typed refusals to wire codes.
    fn from(error: CommandError) -> Self {
        let (status, code) = match &error {
            CommandError::TurnRunning => (StatusCode::CONFLICT, "turn_running"),
            CommandError::QueueNotEmpty => (StatusCode::CONFLICT, "queue_not_empty"),
            CommandError::NothingRunning => (StatusCode::CONFLICT, "nothing_running"),
            CommandError::DraftChanged => (StatusCode::CONFLICT, "draft_changed"),
            CommandError::SessionLocked => (StatusCode::CONFLICT, "session_locked"),
            CommandError::UnknownSession => (StatusCode::NOT_FOUND, "unknown_session"),
            CommandError::TooManySessions => (StatusCode::CONFLICT, "too_many_sessions"),
            CommandError::NotAvailableRemotely => {
                (StatusCode::CONFLICT, "not_available_remotely")
            }
            CommandError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            CommandError::Failed(_) => (StatusCode::INTERNAL_SERVER_ERROR, "failed"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        #[derive(Serialize)]
        struct Body<'a> {
            code: &'a str,
            message: &'a str,
        }
        secure_json(
            self.status,
            Body {
                code: self.code,
                message: &self.message,
            },
        )
    }
}

/// A JSON body whose parse failures are reported in the API's error format.
pub(super) struct ApiJson<T>(pub(super) T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: axum::extract::Request, state: &S) -> Result<Self, ApiError> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|rejection: JsonRejection| ApiError::invalid(rejection.body_text()))
    }
}

type ApiResult<T> = Result<Response<Body>, T>;

/// Rejects cross-origin and unauthenticated requests before any handler runs.
async fn guard(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response<Body> {
    let headers = request.headers();
    if let Some(origin) = headers.get(header::ORIGIN)
        && !origin_matches_host(origin, headers.get(header::HOST))
    {
        return ApiError::invalid("cross-origin requests are refused").into_response();
    }
    let path = request.uri().path();
    if path == "/api" || path.starts_with("/api/") {
        let mutating = !matches!(*request.method(), Method::GET | Method::HEAD);
        if mutating && headers.get("x-tact").is_none_or(|value| value != "1") {
            return ApiError::invalid("the X-Tact: 1 header is required").into_response();
        }
        if path != "/api/login" && !has_valid_cookie(&state.token, headers) {
            return ApiError::unauthorized().into_response();
        }
    }
    next.run(request).await
}

fn origin_matches_host(origin: &HeaderValue, host: Option<&HeaderValue>) -> bool {
    let (Ok(origin), Some(Ok(host))) = (origin.to_str(), host.map(HeaderValue::to_str)) else {
        return false;
    };
    origin
        .split_once("://")
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

fn has_valid_cookie(token: &MachineToken, headers: &HeaderMap) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .any(|(name, value)| name == COOKIE_NAME && token.matches(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    token: String,
}

async fn login(
    State(state): State<Arc<AppState>>,
    ApiJson(request): ApiJson<LoginRequest>,
) -> ApiResult<ApiError> {
    if !state.token.matches(&request.token) {
        return Err(ApiError::unauthorized());
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let cookie = format!(
        "{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/",
        state.token.expose()
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("base64url cookies are valid header values"),
    );
    secure(&mut response);
    Ok(response)
}

/// A browser command. `client` is the tab's random identifier, used to attribute draft writes.
#[derive(Deserialize)]
struct CommandBody {
    client: ClientId,
    #[serde(flatten)]
    command: Command,
}

async fn command(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<CommandBody>,
) -> ApiResult<ApiError> {
    let (reply, outcome) = oneshot::channel();
    state
        .requests
        .send(Request {
            command: body.command,
            client: body.client,
            reply,
        })
        .map_err(|_| unavailable())?;
    let reply = timeout(COMMAND_TIMEOUT, outcome)
        .await
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "failed", "the terminal did not answer in time"))?
        .map_err(|_| unavailable())??;
    Ok(secure_json(StatusCode::OK, reply))
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "failed",
        "the terminal is not accepting commands",
    )
}

#[derive(Serialize)]
struct InstanceInfo {
    protocol_version: u32,
    workspace: String,
    repository: String,
    live: usize,
    running: bool,
}

async fn instance(State(state): State<Arc<AppState>>) -> Response<Body> {
    let (live, running) = state.hub.counts();
    let repository = state
        .workspace
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    secure_json(
        StatusCode::OK,
        InstanceInfo {
            protocol_version: PROTOCOL_VERSION,
            workspace: state.workspace.to_string_lossy().into_owned(),
            repository,
            live,
            running,
        },
    )
}

#[derive(Deserialize, Serialize)]
struct SiblingStatus {
    live: usize,
    running: bool,
}

#[derive(Serialize)]
struct InstanceEntry {
    pid: u32,
    port: u16,
    workspace: PathBuf,
    live: usize,
    running: bool,
    current: bool,
}

/// Lists this instance and every registered sibling that answers within [`SIBLING_TIMEOUT`].
async fn instances(State(state): State<Arc<AppState>>) -> Response<Body> {
    let own_pid = std::process::id();
    let records = registry::read_all(&state.registry_directory);
    let probes = records.into_iter().filter(|record| record.pid != own_pid).map(|record| {
        let state = Arc::clone(&state);
        async move {
            let status = probe(&state, &record).await?;
            Some(InstanceEntry {
                pid: record.pid,
                port: record.port,
                workspace: record.workspace,
                live: status.live,
                running: status.running,
                current: false,
            })
        }
    });
    let mut entries: Vec<_> = join_all(probes).await.into_iter().flatten().collect();
    let (live, running) = state.hub.counts();
    entries.push(InstanceEntry {
        pid: own_pid,
        port: state.port,
        workspace: state.workspace.clone(),
        live,
        running,
        current: true,
    });
    entries.sort_by_key(|entry| entry.pid);
    secure_json(StatusCode::OK, serde_json::json!({ "instances": entries }))
}

async fn probe(state: &AppState, record: &InstanceRecord) -> Option<SiblingStatus> {
    let response = state
        .client
        .get(format!("http://127.0.0.1:{}/api/instance", record.port))
        .header(header::COOKIE, format!("{COOKIE_NAME}={}", state.token.expose()))
        .timeout(SIBLING_TIMEOUT)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    response.json().await.ok()
}

async fn entry_detail(
    State(state): State<Arc<AppState>>,
    Path((session, entry)): Path<(String, usize)>,
) -> ApiResult<ApiError> {
    let detail = state
        .hub
        .entry_detail(&session, entry)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown_session", ENTRY_NOT_FOUND))?;
    Ok(secure_json(StatusCode::OK, detail))
}

async fn stream_events(State(state): State<Arc<AppState>>) -> ApiResult<ApiError> {
    let subscription = state.hub.subscribe().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "failed",
            "too many open event streams",
        )
    })?;
    let mut keep_alive = interval(KEEP_ALIVE);
    keep_alive.reset();
    let shutdown = state.shutdown.clone();
    let events = stream::unfold(
        (subscription, keep_alive, shutdown),
        |(mut subscription, mut keep_alive, shutdown)| async move {
            let chunk = tokio::select! {
                frame = subscription.frames.recv() => Bytes::copy_from_slice(frame?.as_bytes()),
                _ = keep_alive.tick() => Bytes::from_static(b": keep-alive\n\n"),
                () = shutdown.cancelled() => return None,
            };
            Some((Ok::<_, Infallible>(chunk), (subscription, keep_alive, shutdown)))
        },
    );
    let mut response = Response::new(Body::from_stream(events.map(|chunk| chunk)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    secure(&mut response);
    Ok(response)
}

async fn static_asset(State(state): State<Arc<AppState>>, uri: Uri) -> Response<Body> {
    let path = uri.path().trim_start_matches('/');
    let Some(assets) = state.assets.current().await else {
        if path.is_empty() {
            let mut response = Response::new(Body::from(state.assets.placeholder_html()));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            secure(&mut response);
            return response;
        }
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = if path.is_empty() { assets.entrypoint() } else { path };
    let Some(asset) = assets.resolve(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(contents) = tokio::fs::read(asset.path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(content_type) = HeaderValue::from_str(&asset.content_type) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut response = Response::new(Body::from(contents));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    secure(&mut response);
    if path == "overview-frame.html" {
        // The overview executes agent-authored MDX in an opaque-origin sandbox. Its policy applies
        // only to this frame; the application's policy stays strict.
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'",
            ),
        );
    }
    response
}

pub(super) fn secure_json(status: StatusCode, value: impl Serialize) -> Response<Body> {
    let mut response = (status, Json(value)).into_response();
    secure(&mut response);
    response
}

pub(super) fn secure(response: &mut Response<Body>) {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-src 'self'",
        ),
    );
}


#[cfg(test)]
mod tests {
    use super::super::{
        bridge::{Command, CommandError, Publication, Reply},
        testing::Harness,
    };
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use serde_json::json;

    fn unauthenticated(method: Method, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("x-tact", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap()
    }

    #[tokio::test]
    async fn every_api_route_except_login_requires_the_cookie() {
        let harness = Harness::new();
        let routes = [
            (Method::GET, "/api/instance"),
            (Method::GET, "/api/instances"),
            (Method::GET, "/api/stream"),
            (Method::GET, "/api/sessions/s/entries/1"),
            (Method::GET, "/api/review"),
            (Method::POST, "/api/cmd"),
            (Method::POST, "/api/refresh"),
            (Method::POST, "/api/range"),
            (Method::POST, "/api/overview"),
            (Method::POST, "/api/ai-review"),
            (Method::POST, "/api/question"),
            (Method::POST, "/api/questions"),
            (Method::POST, "/api/question/cancel"),
            (Method::POST, "/api/review/compose"),
        ];
        for (method, uri) in routes {
            let (status, _, body) = harness.send(unauthenticated(method, uri)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["code"], "unauthorized", "{uri}");
        }
    }

    #[tokio::test]
    async fn a_wrong_cookie_is_rejected() {
        let harness = Harness::new();
        let mut request = unauthenticated(Method::GET, "/api/instance");
        request
            .headers_mut()
            .insert(header::COOKIE, "tact=wrong".parse().unwrap());

        let (status, _, _) = harness.send(request).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_sets_a_strict_http_only_cookie_for_the_token_only() {
        let harness = Harness::new();
        let login = |token: &str| {
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("x-tact", "1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "token": token }).to_string()))
                .unwrap()
        };

        let (status, headers, _) = harness.send(login("nope")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(headers.get(header::SET_COOKIE).is_none());

        let (status, headers, _) = harness.send(login(harness.state.token.expose())).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert_eq!(
            cookie,
            format!(
                "tact={}; HttpOnly; SameSite=Strict; Path=/",
                harness.state.token.expose()
            )
        );
        let (status, _) = harness.call(Method::GET, "/api/instance", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn posts_without_the_x_tact_header_are_rejected() {
        let harness = Harness::new();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/cmd")
            .header(header::COOKIE, harness.cookie())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();

        let (status, _, body) = harness.send(request).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8(body).unwrap().contains("invalid_request"));
    }

    #[tokio::test]
    async fn an_origin_that_does_not_match_the_host_is_rejected() {
        let harness = Harness::new();
        let request = |origin: &str| {
            Request::builder()
                .uri("/api/instance")
                .header(header::HOST, "127.0.0.1:7878")
                .header(header::ORIGIN, origin)
                .header(header::COOKIE, harness.cookie())
                .body(Body::empty())
                .unwrap()
        };

        let (status, _, _) = harness.send(request("http://evil.example")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = harness.send(request("null")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, headers, _) = harness.send(request("http://127.0.0.1:7878")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    }

    #[tokio::test]
    async fn static_assets_are_public_and_a_missing_bundle_explains_installation() {
        let harness = Harness::new();
        let request = Request::builder().uri("/").body(Body::empty()).unwrap();

        let (status, headers, body) = harness.send(request).await;

        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
        assert!(String::from_utf8(body).unwrap().contains("not installed"));
        let missing = Request::builder().uri("/app.js").body(Body::empty()).unwrap();
        assert_eq!(harness.send(missing).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn commands_round_trip_through_the_terminal_loop() {
        let mut harness = Harness::new();

        let (status, reply, request) = harness
            .command(
                json!({"command": "compact", "client": 9, "session": "s1"}),
                Ok(Reply::Done {}),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(reply, json!({}));
        assert_eq!(request.client, 9);
        assert!(matches!(
            request.command,
            Command::Compact { ref session } if session == "s1"
        ));
    }

    #[tokio::test]
    async fn refusals_map_to_wire_codes() {
        let cases = [
            (CommandError::TurnRunning, StatusCode::CONFLICT, "turn_running"),
            (CommandError::UnknownSession, StatusCode::NOT_FOUND, "unknown_session"),
            (CommandError::DraftChanged, StatusCode::CONFLICT, "draft_changed"),
            (CommandError::Invalid("bad".into()), StatusCode::BAD_REQUEST, "invalid_request"),
            (CommandError::Failed("boom".into()), StatusCode::INTERNAL_SERVER_ERROR, "failed"),
        ];
        let mut harness = Harness::new();
        for (error, expected_status, expected_code) in cases {
            let (status, body, _) = harness
                .command(
                    json!({"command": "interrupt", "client": 1, "session": "s"}),
                    Err(error),
                )
                .await;
            assert_eq!(status, expected_status);
            assert_eq!(body["code"], expected_code);
        }
    }

    #[tokio::test]
    async fn malformed_command_bodies_are_invalid_requests() {
        let harness = Harness::new();

        let (status, body) = harness
            .call(Method::POST, "/api/cmd", Some(json!({"command": "nope", "client": 1})))
            .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_request");
    }

    #[tokio::test]
    async fn a_stopped_terminal_loop_is_reported_as_unavailable() {
        let mut harness = Harness::new();
        harness.terminal.requests.close();

        let (status, body) = harness
            .call(
                Method::POST,
                "/api/cmd",
                Some(json!({"command": "compact", "client": 1, "session": "s"})),
            )
            .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "failed");
    }

    #[tokio::test]
    async fn instance_reports_live_sessions_and_busy_state() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        harness.terminal.publisher.publish(Publication::Busy {
            session: "s1".into(),
            busy: crate::web::bridge::Busy { turns: 1, shells: 0 },
        });
        while !harness.hub.any_busy() {
            tokio::task::yield_now().await;
        }

        let (status, body) = harness.call(Method::GET, "/api/instance", None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["protocol_version"], 8);
        assert_eq!(body["live"], 1);
        assert_eq!(body["running"], true);
    }
}

