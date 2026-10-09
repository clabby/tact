//! HTTP surface of the web server: authentication, hardening, reads, commands, the event stream,
//! and static assets. Review routes live in [`super::review`].

use super::{
    assets::AssetStore,
    hub::Hub,
    machines::Registry,
    proxy,
    registry::{self, InstanceRecord},
    tailscale::Tailnet,
    token::MachineToken,
    wire::PROTOCOL_VERSION,
    workspaces::{WorkspaceError, Workspaces},
};
use crate::core::protocol::{
    Command, CommandEnvelope, CommandError, OpenSpec, Query, QueryReply, QueryRequest, Request,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{
        DefaultBodyLimit, FromRequest, Path, Query as QueryParams, State, rejection::JsonRejection,
    },
    http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri, header},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{any, get, post},
};
use futures_util::{StreamExt as _, future::join_all, stream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};
use tact_subagents::AgentId;
use tokio::{
    sync::{mpsc, oneshot},
    time::{interval, timeout},
};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Commands may carry pasted images in a data URL.
pub(super) const MAX_COMMAND_BODY_BYTES: usize = 32 * 1024 * 1024;
const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// Longest a command may wait for the terminal loop. Opening a large session can be slow.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const SIBLING_TIMEOUT: Duration = Duration::from_millis(500);
const COOKIE_NAME: &str = "tact";
const ENTRY_NOT_FOUND: &str = "the entry does not exist";

/// How other devices reach this server, as configured.
pub(super) enum PublicOrigin {
    /// Only this computer can reach it.
    None,
    /// The address of a tunnel the user runs (`web.public_url`).
    Fixed(String),
    /// A Tailscale address that exists only while the server is published to the tailnet
    /// (`web.tailscale`).
    Tailnet(Tailnet),
}

/// Everything the handlers share. Built once the listener is bound.
pub(super) struct AppState {
    pub(super) token: MachineToken,
    pub(super) hub: Hub,
    pub(super) requests: mpsc::UnboundedSender<Request>,
    pub(super) queries: mpsc::UnboundedSender<QueryRequest>,
    pub(super) workspace: PathBuf,
    pub(super) port: u16,
    pub(super) public_origin: PublicOrigin,
    pub(super) workspaces: Arc<Workspaces>,
    pub(super) registry_directory: PathBuf,
    /// The web interface's files, or `None` for `tact serve`, which serves only its API.
    pub(super) assets: Option<AssetStore>,
    /// Carries this machine's token to sibling instances; see [`sibling_client`].
    pub(super) client: reqwest::Client,
    /// The linked machines, read from disk on every request.
    pub(super) machines: Registry,
    /// Carries peer tokens to linked machines; see [`super::machines::peer_client`].
    pub(super) peer_client: reqwest::Client,
    pub(super) shutdown: CancellationToken,
}

/// Builds the router. `extra` routes are served behind the same guard.
pub(super) fn router(state: Arc<AppState>, extra: Router<Arc<AppState>>) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/instance", get(instance))
        .route("/api/link", get(link))
        .route("/api/file", get(local_image))
        .route("/api/instances", get(instances))
        .route("/api/sessions/{session}/entries/{entry}", get(entry_detail))
        .route(
            "/api/sessions/{session}/entries/{entry}/images/{index}",
            get(user_image),
        )
        .route(
            "/api/sessions/{session}/agents/{agent}/entries",
            get(agent_entries),
        )
        .route(
            "/api/sessions/{session}/agents/{agent}/entries/{entry}",
            get(agent_entry_detail),
        )
        .route("/api/stream", get(stream_events))
        .route(
            "/api/cmd",
            post(command).layer(DefaultBodyLimit::max(MAX_COMMAND_BODY_BYTES)),
        )
        .route("/api/query", post(query))
        .route("/api/machines", get(proxy::machines))
        .route("/api/m/{name}/{*rest}", any(proxy::relay))
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
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication required",
        )
    }
}

impl From<WorkspaceError> for ApiError {
    fn from(error: WorkspaceError) -> Self {
        match error {
            WorkspaceError::UnknownSession => {
                Self::new(StatusCode::NOT_FOUND, "unknown_session", error.to_string())
            }
            WorkspaceError::NotACheckout(_) => Self::invalid(error.to_string()),
        }
    }
}

impl From<CommandError> for ApiError {
    /// The code comes from [`CommandError::code`]; the status is the transport's choice.
    fn from(error: CommandError) -> Self {
        let status = match &error {
            CommandError::UnknownSession => StatusCode::NOT_FOUND,
            CommandError::Invalid(_) => StatusCode::BAD_REQUEST,
            CommandError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::CONFLICT,
        };
        Self::new(status, error.code(), error.to_string())
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

async fn command(
    State(state): State<Arc<AppState>>,
    ApiJson(mut body): ApiJson<CommandEnvelope>,
) -> ApiResult<ApiError> {
    if let Command::Open(OpenSpec::New {
        workspace: Some(workspace),
        ..
    }) = &mut body.command
    {
        // The terminal loop accepts any directory; only checkouts of known repositories are
        // offered to clients.
        let allowed = state
            .workspaces
            .startable(std::path::Path::new(workspace.as_str()))
            .await?;
        *workspace = allowed.to_string_lossy().into_owned();
    }
    let (reply, outcome) = oneshot::channel();
    let request = Request {
        command: body.command,
        client: body.client,
        reply,
    };
    state.requests.send(request).map_err(|_| unavailable())?;
    Ok(secure_json(StatusCode::OK, answer(outcome).await?))
}

async fn query(
    State(state): State<Arc<AppState>>,
    ApiJson(query): ApiJson<Query>,
) -> ApiResult<ApiError> {
    if let Query::Workspaces { session } = &query {
        let reply = state.workspaces.list(session.as_deref()).await?;
        return Ok(secure_json(StatusCode::OK, reply));
    }
    let (reply, outcome) = oneshot::channel();
    state
        .queries
        .send(QueryRequest { query, reply })
        .map_err(|_| unavailable())?;
    let reply: QueryReply = answer(outcome).await?;
    Ok(secure_json(StatusCode::OK, reply))
}

/// Waits for the terminal loop's answer to a command or query.
async fn answer<T>(outcome: oneshot::Receiver<Result<T, CommandError>>) -> Result<T, ApiError> {
    let answered = timeout(COMMAND_TIMEOUT, outcome).await.map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "failed",
            "the terminal did not answer in time",
        )
    })?;
    Ok(answered.map_err(|_| unavailable())??)
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

/// The largest local image the transcript will display.
const MAX_LOCAL_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Deserialize)]
struct FileQuery {
    path: String,
    /// Relative paths are the session's workspace's; without a session, the default workspace's.
    session: Option<String>,
}

/// Serves an image that an agent's Markdown refers to, wherever it lives on this machine, as the
/// terminal transcript displays such images. Only files whose bytes are a raster image the browser
/// renders inertly are served, so this reads no other file. Relative paths are the workspace's.
async fn local_image(
    State(state): State<Arc<AppState>>,
    QueryParams(query): QueryParams<FileQuery>,
) -> Result<Response<Body>, ApiError> {
    let missing = || ApiError::new(StatusCode::NOT_FOUND, "invalid_request", "no such image");
    let path = match query.path.strip_prefix("file://") {
        Some(rest) => PathBuf::from(rest),
        None => PathBuf::from(&query.path),
    };
    let workspace = query
        .session
        .as_deref()
        .and_then(|session| state.hub.session_workspace(session))
        .unwrap_or_else(|| state.workspace.clone());
    let path = workspace.join(path);
    let metadata = tokio::fs::metadata(&path).await.map_err(|_| missing())?;
    if !metadata.is_file() || metadata.len() > MAX_LOCAL_IMAGE_BYTES {
        return Err(missing());
    }
    let bytes = tokio::fs::read(&path).await.map_err(|_| missing())?;
    let media_type = sniff_image(&bytes).ok_or_else(missing)?;
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(media_type));
    secure(&mut response);
    Ok(response)
}

/// The media type of a PNG, JPEG, GIF, or WebP file, judged by its leading bytes.
pub(super) fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

/// What a client needs to build a sign-in link for another device: the public origin, if any, and
/// the token. The token already authorizes everything this request does, so an authenticated client
/// learns nothing new; it is never logged.
///
/// With `web.tailscale` this is the moment the server is published to the tailnet, and Tailscale is
/// checked on every request, so a client switched on after an earlier refusal is picked up.
async fn link(State(state): State<Arc<AppState>>) -> Result<Response<Body>, ApiError> {
    let public_origin = match &state.public_origin {
        PublicOrigin::None => None,
        PublicOrigin::Fixed(origin) => Some(origin.clone()),
        PublicOrigin::Tailnet(tailnet) => Some(tailnet.origin().await.map_err(|error| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "tailscale_unavailable",
                format!("Cannot share over Tailscale: {error}"),
            )
        })?),
    };
    Ok(secure_json(
        StatusCode::OK,
        serde_json::json!({
            "public_origin": public_origin,
            "token": state.token.expose(),
        }),
    ))
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
    let probes = records
        .into_iter()
        .filter(|record| record.pid != own_pid)
        .map(|record| {
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
        .header(
            header::COOKIE,
            format!("{COOKIE_NAME}={}", state.token.expose()),
        )
        .timeout(SIBLING_TIMEOUT)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    response.json().await.ok()
}

/// The client for sibling probes. Probes carry the machine token, so they go straight to the
/// registered loopback port: never through a proxy from the environment, and never on to wherever
/// a redirect points.
pub(super) fn sibling_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

fn entry_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown_session", ENTRY_NOT_FOUND)
}

async fn entry_detail(
    State(state): State<Arc<AppState>>,
    Path((session, entry)): Path<(String, usize)>,
) -> ApiResult<ApiError> {
    let detail = state
        .hub
        .entry_detail(&session, entry)
        .ok_or_else(entry_not_found)?;
    Ok(secure_json(StatusCode::OK, detail))
}

async fn user_image(
    State(state): State<Arc<AppState>>,
    Path((session, entry, index)): Path<(String, usize, usize)>,
) -> Result<Response<Body>, ApiError> {
    let (media_type, bytes) = state
        .hub
        .user_image(&session, entry, index)
        .ok_or_else(entry_not_found)?;
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(media_type));
    secure(&mut response);
    Ok(response)
}

async fn agent_entries(
    State(state): State<Arc<AppState>>,
    Path((session, agent)): Path<(String, AgentId)>,
) -> ApiResult<ApiError> {
    let entries = state
        .hub
        .agent_entries(&session, agent)
        .ok_or_else(entry_not_found)?;
    Ok(secure_json(
        StatusCode::OK,
        serde_json::json!({ "entries": entries }),
    ))
}

async fn agent_entry_detail(
    State(state): State<Arc<AppState>>,
    Path((session, agent, entry)): Path<(String, AgentId, usize)>,
) -> ApiResult<ApiError> {
    let detail = state
        .hub
        .agent_entry_detail(&session, agent, entry)
        .ok_or_else(entry_not_found)?;
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
                frame = subscription.frames.recv() => frame?,
                _ = keep_alive.tick() => Bytes::from_static(b": keep-alive\n\n"),
                () = shutdown.cancelled() => return None,
            };
            Some((
                Ok::<_, Infallible>(chunk),
                (subscription, keep_alive, shutdown),
            ))
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
    let Some(store) = &state.assets else {
        return ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_route",
            "this server serves only its API",
        )
        .into_response();
    };
    let path = uri.path().trim_start_matches('/');
    let Some(assets) = store.current().await else {
        if path.is_empty() {
            let mut response = Response::new(Body::from(store.placeholder_html()));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            secure(&mut response);
            return response;
        }
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = if path.is_empty() {
        assets.entrypoint()
    } else {
        path
    };
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
            "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            registry::{InstanceRecord, Registration},
            tailscale::Tailnet,
            testing::{self, Harness, Upstream},
        },
        PublicOrigin,
    };
    use crate::core::protocol::{Command, CommandError, OpenSpec, Publication, Query, Reply};
    use axum::{
        Router,
        body::Body,
        http::{Method, Request, StatusCode, header},
        routing::get,
    };
    use serde_json::json;
    use std::{path::PathBuf, time::Duration};

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
            (Method::GET, "/api/link"),
            (Method::GET, "/api/file?path=x.png"),
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
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "invalid_request");
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
        assert!(
            headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("default-src 'none'")
        );
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        let store = harness.state.assets.as_ref().unwrap();
        assert_eq!(body, store.placeholder_html().into_bytes());
        let missing = Request::builder()
            .uri("/app.js")
            .body(Body::empty())
            .unwrap();
        assert_eq!(harness.send(missing).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_application_policy_forbids_forms_base_urls_and_framing() {
        let harness = Harness::new();
        harness.install_bundle(&[
            ("index.html", "<!doctype html>"),
            ("overview-frame.html", "<!doctype html>"),
        ]);
        let get = |uri: &str| {
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, harness.cookie())
                .body(Body::empty())
                .unwrap()
        };
        let policy = |headers: &axum::http::HeaderMap| {
            headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .to_owned()
        };

        for uri in ["/", "/api/instance"] {
            let (status, headers, _) = harness.send(get(uri)).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            let policy = policy(&headers);
            for directive in [
                "form-action 'none'",
                "base-uri 'none'",
                "frame-ancestors 'none'",
            ] {
                assert!(policy.contains(directive), "{uri}: {policy}");
            }
        }
        let (status, headers, _) = harness.send(get("/overview-frame.html")).await;
        assert_eq!(status, StatusCode::OK);
        let policy = policy(&headers);
        assert!(policy.contains("frame-ancestors 'self'"), "{policy}");
        assert!(!policy.contains("frame-ancestors 'none'"), "{policy}");
    }

    #[tokio::test]
    async fn commands_round_trip_through_the_terminal_loop() {
        let mut harness = Harness::new();

        let (status, reply, request) = harness
            .command(
                json!({"client": 9, "cmd": "compact", "args": {"session": "s1"}}),
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
            (
                CommandError::TurnRunning,
                StatusCode::CONFLICT,
                "turn_running",
            ),
            (
                CommandError::UnknownSession,
                StatusCode::NOT_FOUND,
                "unknown_session",
            ),
            (
                CommandError::DraftChanged,
                StatusCode::CONFLICT,
                "draft_changed",
            ),
            (
                CommandError::Invalid("bad".into()),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                CommandError::Failed("boom".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed",
            ),
        ];
        let mut harness = Harness::new();
        for (error, expected_status, expected_code) in cases {
            let (status, body, _) = harness
                .command(
                    json!({"client": 1, "cmd": "interrupt", "args": {"session": "s"}}),
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
            .call(
                Method::POST,
                "/api/cmd",
                Some(json!({"client": 1, "cmd": "nope"})),
            )
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
                Some(json!({"client": 1, "cmd": "compact", "args": {"session": "s"}})),
            )
            .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "failed");
    }

    #[tokio::test]
    async fn only_real_images_are_served_from_disk() {
        let harness = Harness::new();
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let disguised = directory.path().join("notes.png");
        std::fs::write(&disguised, b"api key: hunter2").unwrap();
        let get = |path: &std::path::Path| {
            let uri = format!("/api/file?path={}", path.display());
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, harness.cookie())
                .body(Body::empty())
                .unwrap()
        };

        let (status, headers, body) = harness.send(get(&png)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        assert!(body.starts_with(b"\x89PNG"));
        let (status, _, _) = harness.send(get(&disguised)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "a non-image is never served");
        let (status, _, _) = harness
            .send(get(&directory.path().join("absent.png")))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_sign_in_link_is_served_to_authenticated_clients() {
        let harness = Harness::new();

        let (status, body) = harness.call(Method::GET, "/api/link", None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["token"], harness.state.token.expose());
        assert_eq!(body["public_origin"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_fixed_public_origin_is_served_with_the_token() {
        let harness = Harness::with_origin(
            testing::repository(),
            testing::idle_agent(),
            PublicOrigin::Fixed("https://tact.example.net".to_owned()),
        );

        let (status, body) = harness.call(Method::GET, "/api/link", None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["public_origin"], "https://tact.example.net");
        assert_eq!(body["token"], harness.state.token.expose());
    }

    #[tokio::test]
    async fn a_tailnet_that_cannot_be_reached_refuses_the_link_without_the_token() {
        let tailnet = Tailnet::with(
            vec![PathBuf::from("/nonexistent/tailscale")],
            7878,
            Duration::from_millis(10),
        );
        let harness = Harness::with_origin(
            testing::repository(),
            testing::idle_agent(),
            PublicOrigin::Tailnet(tailnet),
        );

        let (status, body) = harness.call(Method::GET, "/api/link", None).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "tailscale_unavailable", "{body}");
        assert!(body.get("token").is_none());
    }

    /// Registers `sibling` as another instance and lists the instances.
    async fn list_with_sibling(harness: &Harness, sibling: &Upstream) -> serde_json::Value {
        let _registration = Registration::create(
            &harness.state.registry_directory,
            &InstanceRecord {
                pid: std::process::id().wrapping_add(1),
                port: sibling.port,
                workspace: "/sibling".into(),
                started_at: 1,
            },
        )
        .unwrap();
        let (status, body) = harness.call(Method::GET, "/api/instances", None).await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    fn sibling_status() -> Router {
        Router::new().route(
            "/api/instance",
            get(|| async { axum::Json(json!({"live": 2, "running": false})) }),
        )
    }

    #[tokio::test]
    async fn sibling_probes_do_not_follow_redirects() {
        let elsewhere = Upstream::spawn(sibling_status()).await;
        let target = format!("{}/api/instance", elsewhere.origin);
        let sibling = Upstream::spawn(Router::new().route(
            "/api/instance",
            get(move || async move { (StatusCode::FOUND, [(header::LOCATION, target)]) }),
        ))
        .await;
        let harness = Harness::new();

        let body = list_with_sibling(&harness, &sibling).await;

        assert_eq!(sibling.hits(), 1);
        assert_eq!(elsewhere.hits(), 0, "the token never follows a redirect");
        assert_eq!(body["instances"].as_array().unwrap().len(), 1, "{body}");
    }

    #[tokio::test]
    async fn sibling_probes_ignore_environment_proxies() {
        let proxy = Upstream::spawn(sibling_status()).await;
        let sibling = Upstream::spawn(sibling_status()).await;
        let harness = testing::with_proxy_environment(&proxy.origin, Harness::new);

        let body = list_with_sibling(&harness, &sibling).await;

        assert_eq!(proxy.hits(), 0, "the token never goes through a proxy");
        assert_eq!(sibling.hits(), 1);
        assert_eq!(body["instances"].as_array().unwrap().len(), 2, "{body}");
    }

    #[tokio::test]
    async fn instance_reports_live_sessions_and_busy_state() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        harness.terminal.publisher.publish(Publication::Busy {
            session: "s1".into(),
            busy: crate::core::protocol::Busy {
                turns: 1,
                shells: 0,
            },
        });
        while !harness.hub.any_busy() {
            tokio::task::yield_now().await;
        }

        let (status, body) = harness.call(Method::GET, "/api/instance", None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["protocol_version"], 9);
        assert_eq!(body["live"], 1);
        assert_eq!(body["running"], true);
    }

    #[tokio::test]
    async fn queries_use_one_route_and_share_the_error_mapping() {
        let mut harness = Harness::new();
        let sender = harness.state.queries.clone();
        let loop_side = tokio::spawn({
            let mut queries = std::mem::replace(
                &mut harness.terminal.queries,
                tokio::sync::mpsc::unbounded_channel().1,
            );
            async move {
                let request = queries.recv().await.unwrap();
                assert_eq!(request.query, Query::Models);
                request
                    .reply
                    .send(Err(CommandError::Disabled("memory is off".into())))
                    .unwrap();
            }
        });
        drop(sender);

        let (status, body) = harness
            .call(Method::POST, "/api/query", Some(json!({"query": "models"})))
            .await;
        loop_side.await.unwrap();

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "disabled");
        let (status, body) = harness
            .call(Method::POST, "/api/query", Some(json!({"query": "nope"})))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_request");
    }

    #[tokio::test]
    async fn subagent_transcripts_are_served_for_known_agents_only() {
        use tact_subagents::AgentId;
        let harness = Harness::new();
        harness.open_session("s1").await;
        harness
            .terminal
            .publisher
            .publish(Publication::SubagentRecord {
                session: "s1".into(),
                agent: AgentId::new(2),
                record: std::sync::Arc::new(
                    crate::core::transcript::TranscriptRecord::from_local(
                        1,
                        1,
                        crate::core::transcript::LocalEvent::UserSubmitted(
                            crate::core::transcript::UserSubmitted {
                                id: crate::core::transcript::TurnId::new(1),
                                text: "task".into(),
                            },
                        ),
                    )
                    .unwrap(),
                ),
            });
        while harness.hub.agent_entries("s1", AgentId::new(2)).is_none() {
            tokio::task::yield_now().await;
        }

        let (status, body) = harness
            .call(Method::GET, "/api/sessions/s1/agents/2/entries", None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["entries"][0]["text"], "task");
        let (status, _) = harness
            .call(Method::GET, "/api/sessions/s1/agents/3/entries", None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn workspaces_list_the_checkouts_of_the_sessions_repository() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        let (_directory, worktree) = testing::worktree(harness.workspace.path());
        std::fs::write(worktree.join("scratch.txt"), "scratch\n").unwrap();

        let (status, body) = harness
            .call(
                Method::POST,
                "/api/query",
                Some(json!({"query": "workspaces", "args": {"session": "s1"}})),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        let checkouts = body["checkouts"].as_array().unwrap();
        assert_eq!(checkouts.len(), 2, "{body}");
        let main = checkouts
            .iter()
            .find(|entry| entry["current"] == true)
            .unwrap();
        assert_eq!(main["label"], "feature");
        assert_eq!(main["changed_files"], 1);
        let other = checkouts
            .iter()
            .find(|entry| entry["path"] == worktree.to_str().unwrap())
            .unwrap();
        assert_eq!(other["label"], "elsewhere");
        assert_eq!(other["current"], false);
        assert_eq!(other["changed_files"], 1);
        assert_eq!(other["kind"], "git");
    }

    #[tokio::test]
    async fn workspaces_omit_checkouts_whose_directory_was_deleted() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        let (_directory, worktree) = testing::worktree(harness.workspace.path());
        std::fs::remove_dir_all(&worktree).unwrap();

        let (status, body) = harness
            .call(
                Method::POST,
                "/api/query",
                Some(json!({"query": "workspaces", "args": {"session": "s1"}})),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        let checkouts = body["checkouts"].as_array().unwrap();
        assert_eq!(checkouts.len(), 1, "{body}");
        assert_eq!(checkouts[0]["current"], true);
    }

    #[tokio::test]
    async fn a_session_may_only_start_in_a_checkout_of_a_known_repository() {
        let mut harness = Harness::new();
        harness.open_session("s1").await;
        let (_directory, worktree) = testing::worktree(harness.workspace.path());
        let stranger = tempfile::tempdir().unwrap();

        let (status, body) = harness
            .call(
                Method::POST,
                "/api/cmd",
                Some(json!({"client": 1, "cmd": "open_session", "args": {
                    "new": {"workspace": stranger.path()}
                }})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_request");

        let (status, _, request) = harness
            .command(
                json!({"client": 1, "cmd": "open_session", "args": {
                    "new": {"workspace": worktree}
                }}),
                Ok(Reply::Opened {
                    session: "s2".into(),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(matches!(
            request.command,
            Command::Open(OpenSpec::New { workspace: Some(ref path), .. })
                if std::path::Path::new(path) == worktree
        ));
    }
}
