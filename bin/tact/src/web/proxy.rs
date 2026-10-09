//! The relay from this hub's web interface to linked machines (see [`super::machines`]).
//!
//! `/api/m/{name}/...` sits behind the hub's own guard, so only a signed-in browser reaches it. A
//! linked machine is trusted to run sessions but not with the hub's origin: whatever it answers must
//! not run script here, set cookies, redirect, or learn the hub's token. So the relay forwards only a
//! fixed list of API routes, rebuilds every request from typed values with headers of its own, and
//! rebuilds every response with a status, content type, and security headers that the hub chooses.
//! Everything that can be refused is refused before a connection is opened.

use super::{
    api::{
        ApiError, AppState, MAX_BODY_BYTES, MAX_COMMAND_BODY_BYTES, secure, secure_json,
        sniff_image,
    },
    machines::{Machine, read_limited},
};
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderValue, Method, Response, StatusCode, header},
    response::IntoResponse,
};
use futures_util::stream;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use url::form_urlencoded;

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// A peer's event stream sends a keep-alive every 15 seconds, so this much silence means it is gone.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_SESSION_ID_BYTES: usize = 128;
/// Proxied responses are data, never documents: nothing in them may load, run, or navigate.
const PROXIED_POLICY: &str = "default-src 'none'; sandbox";

/// `GET /api/machines`: the names of the linked machines, and nothing else about them.
pub(super) async fn machines(State(state): State<Arc<AppState>>) -> Response<Body> {
    let names: Vec<_> = state
        .machines
        .all()
        .into_iter()
        .map(|machine| serde_json::json!({ "name": machine.name }))
        .collect();
    secure_json(StatusCode::OK, serde_json::json!({ "machines": names }))
}

/// `/api/m/{name}/{*rest}`: relays one allowlisted request to the named machine.
pub(super) async fn relay(State(state): State<Arc<AppState>>, request: Request) -> Response<Body> {
    let mut response = match relay_checked(&state, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(PROXIED_POLICY),
    );
    response
}

async fn relay_checked(state: &AppState, request: Request) -> Result<Response<Body>, ApiError> {
    let (parts, body) = request.into_parts();
    // The raw, still percent-encoded path: decoding first would let `..%2F` change the route.
    let (name, rest) = parts
        .uri
        .path()
        .strip_prefix("/api/m/")
        .and_then(|path| path.split_once('/'))
        .ok_or_else(unknown_route)?;
    let machine = state.machines.load(name).ok().flatten().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_machine",
            "no machine of that name is linked",
        )
    })?;
    let route = PeerRoute::parse(&parts.method, rest, parts.uri.query())?;
    let body = match route.body_limit() {
        Some(limit) => Some(axum::body::to_bytes(body, limit).await.map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request",
                "the request body is too large",
            )
        })?),
        None => None,
    };
    let shutdown = state.shutdown.clone();
    tokio::select! {
        response = exchange(&state.peer_client, &machine, &route, body, &shutdown) => response,
        () = shutdown.cancelled() => Err(unreachable(&machine)),
    }
}

/// Sends the request and builds the hub's response. Only an event stream is passed on as it
/// arrives; every other answer is buffered and checked first.
async fn exchange(
    client: &reqwest::Client,
    machine: &Machine,
    route: &PeerRoute,
    body: Option<Bytes>,
    shutdown: &CancellationToken,
) -> Result<Response<Body>, ApiError> {
    let kind = route.response_kind();
    let mut upstream = client
        .request(
            route.method(),
            format!("{}{}", machine.origin, route.path()),
        )
        .header(header::COOKIE, machine.cookie())
        .header("x-tact", "1")
        .header(header::ACCEPT, kind.accept());
    if let Some(body) = body {
        upstream = upstream
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let sent = upstream.send();
    // A peer answers a stream request at once, so waiting for its headers is bounded like the
    // silence between frames. Other routes may run an agent first and are not bounded.
    let response = if kind == ResponseKind::EventStream {
        timeout(STREAM_IDLE_TIMEOUT, sent)
            .await
            .map_err(|_| unreachable(machine))?
    } else {
        sent.await
    }
    .map_err(|_| unreachable(machine))?;
    let status = match response.status().as_u16() {
        200 | 204 | 400 | 404 | 409 | 413 | 422 | 500 | 503 => response.status(),
        401 => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "machine_unauthorized",
                format!(
                    "machine `{}` refused its token; link it again with `tact machine add --replace`",
                    machine.name
                ),
            ));
        }
        _ => return Err(protocol_error(machine)),
    };
    if status == StatusCode::OK && kind == ResponseKind::EventStream {
        return Ok(event_stream(response, shutdown.clone()));
    }
    let bytes = read_limited(response, MAX_RESPONSE_BYTES)
        .await
        .map_err(|_| unreachable(machine))?
        .ok_or_else(|| protocol_error(machine))?;
    let content_type = match (status, kind) {
        (StatusCode::NO_CONTENT, _) if bytes.is_empty() => None,
        (StatusCode::OK, ResponseKind::Image) => {
            Some(sniff_image(&bytes).ok_or_else(|| protocol_error(machine))?)
        }
        _ => {
            serde_json::from_slice::<serde::de::IgnoredAny>(&bytes)
                .map_err(|_| protocol_error(machine))?;
            Some("application/json")
        }
    };
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    secure(&mut response);
    Ok(response)
}

/// Relays a peer's event stream until the peer ends it, falls silent, or the hub shuts down. The
/// stream outlives the handler, so it watches the shutdown signal itself.
fn event_stream(response: reqwest::Response, shutdown: CancellationToken) -> Response<Body> {
    let frames = stream::unfold(
        (response, shutdown),
        |(mut response, shutdown)| async move {
            let chunk = tokio::select! {
                chunk = timeout(STREAM_IDLE_TIMEOUT, response.chunk()) => chunk,
                () = shutdown.cancelled() => return None,
            };
            match chunk {
                Ok(Ok(Some(chunk))) => Some((Ok::<_, Infallible>(chunk), (response, shutdown))),
                _ => None,
            }
        },
    );
    let mut relayed = Response::new(Body::from_stream(frames));
    relayed.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    secure(&mut relayed);
    relayed
}

fn unknown_route() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_route",
        "this request is not relayed to other machines",
    )
}

fn unreachable(machine: &Machine) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "machine_unreachable",
        format!("could not reach machine `{}`", machine.name),
    )
}

fn protocol_error(machine: &Machine) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "machine_protocol_error",
        format!("machine `{}` did not answer as a Tact server", machine.name),
    )
}

/// What a route answers, and so which content type the hub gives the answer.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ResponseKind {
    Json,
    EventStream,
    /// A raster image, accepted only if its bytes are PNG, JPEG, GIF, or WebP.
    Image,
}

impl ResponseKind {
    fn accept(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::EventStream => "text/event-stream",
            Self::Image => "image/png, image/jpeg, image/gif, image/webp",
        }
    }
}

/// A session identifier as Tact issues them: ASCII letters, digits, `-`, and `_`.
struct SessionId(String);

impl SessionId {
    fn parse(text: &str) -> Option<Self> {
        let valid = (1..=MAX_SESSION_ID_BYTES).contains(&text.len())
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        valid.then(|| Self(text.to_owned()))
    }
}

/// A review action that takes a JSON body and answers JSON.
#[derive(Clone, Copy)]
enum ReviewAction {
    Compose,
    Refresh,
    Range,
    Questions,
    CancelQuestion,
    /// The next three run an agent and answer only when it finishes.
    Overview,
    AiReview,
    Question,
}

impl ReviewAction {
    fn path(self) -> &'static str {
        match self {
            Self::Compose => "/api/review/compose",
            Self::Refresh => "/api/refresh",
            Self::Range => "/api/range",
            Self::Questions => "/api/questions",
            Self::CancelQuestion => "/api/question/cancel",
            Self::Overview => "/api/overview",
            Self::AiReview => "/api/ai-review",
            Self::Question => "/api/question",
        }
    }
}

/// Every request the hub relays. Each variant holds only typed values, and the upstream path and
/// query are written from them, never copied from the browser's request.
enum PeerRoute {
    Instance,
    Stream,
    Command,
    Query,
    File {
        path: String,
        session: Option<SessionId>,
    },
    Entry {
        session: SessionId,
        entry: u64,
    },
    UserImage {
        session: SessionId,
        entry: u64,
        index: u64,
    },
    AgentEntries {
        session: SessionId,
        agent: u64,
    },
    AgentEntry {
        session: SessionId,
        agent: u64,
        entry: u64,
    },
    Review {
        session: Option<SessionId>,
        checkout: Option<String>,
    },
    ReviewAction(ReviewAction),
}

impl PeerRoute {
    /// Recognizes `rest`, the raw path after `/api/m/{name}/`, with its query. A path that is
    /// not relayed is 404; a relayed path with another method is 405.
    fn parse(method: &Method, rest: &str, query: Option<&str>) -> Result<Self, ApiError> {
        let segments: Vec<&str> = rest.split('/').collect();
        let session = |text: &str| SessionId::parse(text).ok_or_else(unknown_route);
        let number = |text: &str| {
            text.bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| text.parse::<u64>().ok())
                .flatten()
                .ok_or_else(unknown_route)
        };
        let route = match segments.as_slice() {
            ["instance"] => Self::Instance,
            ["stream"] => Self::Stream,
            ["cmd"] => Self::Command,
            ["query"] => Self::Query,
            ["file"] => {
                let mut fields = QueryFields::parse(query, &["path", "session"])?;
                Self::File {
                    path: fields.take("path").ok_or_else(unknown_route)?,
                    session: fields
                        .take("session")
                        .map(|text| session(&text))
                        .transpose()?,
                }
            }
            ["sessions", id, "entries", entry] => Self::Entry {
                session: session(id)?,
                entry: number(entry)?,
            },
            ["sessions", id, "entries", entry, "images", index] => Self::UserImage {
                session: session(id)?,
                entry: number(entry)?,
                index: number(index)?,
            },
            ["sessions", id, "agents", agent, "entries"] => Self::AgentEntries {
                session: session(id)?,
                agent: number(agent)?,
            },
            ["sessions", id, "agents", agent, "entries", entry] => Self::AgentEntry {
                session: session(id)?,
                agent: number(agent)?,
                entry: number(entry)?,
            },
            ["review"] => {
                let mut fields = QueryFields::parse(query, &["session", "checkout"])?;
                Self::Review {
                    session: fields
                        .take("session")
                        .map(|text| session(&text))
                        .transpose()?,
                    checkout: fields.take("checkout"),
                }
            }
            ["review", "compose"] => Self::ReviewAction(ReviewAction::Compose),
            ["refresh"] => Self::ReviewAction(ReviewAction::Refresh),
            ["range"] => Self::ReviewAction(ReviewAction::Range),
            ["questions"] => Self::ReviewAction(ReviewAction::Questions),
            ["question", "cancel"] => Self::ReviewAction(ReviewAction::CancelQuestion),
            ["overview"] => Self::ReviewAction(ReviewAction::Overview),
            ["ai-review"] => Self::ReviewAction(ReviewAction::AiReview),
            ["question"] => Self::ReviewAction(ReviewAction::Question),
            _ => return Err(unknown_route()),
        };
        if !matches!(route, Self::File { .. } | Self::Review { .. }) && query.is_some() {
            return Err(unknown_route());
        }
        if *method != route.method() {
            return Err(ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "unknown_route",
                "this method is not relayed for this path",
            ));
        }
        Ok(route)
    }

    fn method(&self) -> Method {
        match self {
            Self::Command | Self::Query | Self::ReviewAction(_) => Method::POST,
            _ => Method::GET,
        }
    }

    /// The largest request body relayed, or `None` for a request that sends no body.
    fn body_limit(&self) -> Option<usize> {
        match self {
            Self::Command => Some(MAX_COMMAND_BODY_BYTES),
            Self::Query | Self::ReviewAction(_) => Some(MAX_BODY_BYTES),
            _ => None,
        }
    }

    fn response_kind(&self) -> ResponseKind {
        match self {
            Self::Stream => ResponseKind::EventStream,
            Self::File { .. } | Self::UserImage { .. } => ResponseKind::Image,
            _ => ResponseKind::Json,
        }
    }

    /// The upstream path and query, written from the typed values.
    fn path(&self) -> String {
        match self {
            Self::Instance => "/api/instance".to_owned(),
            Self::Stream => "/api/stream".to_owned(),
            Self::Command => "/api/cmd".to_owned(),
            Self::Query => "/api/query".to_owned(),
            Self::File { path, session } => {
                let mut query = form_urlencoded::Serializer::new(String::new());
                query.append_pair("path", path);
                if let Some(session) = session {
                    query.append_pair("session", &session.0);
                }
                format!("/api/file?{}", query.finish())
            }
            Self::Entry { session, entry } => {
                format!("/api/sessions/{}/entries/{entry}", session.0)
            }
            Self::UserImage {
                session,
                entry,
                index,
            } => format!("/api/sessions/{}/entries/{entry}/images/{index}", session.0),
            Self::AgentEntries { session, agent } => {
                format!("/api/sessions/{}/agents/{agent}/entries", session.0)
            }
            Self::AgentEntry {
                session,
                agent,
                entry,
            } => format!("/api/sessions/{}/agents/{agent}/entries/{entry}", session.0),
            Self::Review { session, checkout } => {
                let mut query = form_urlencoded::Serializer::new(String::new());
                if let Some(session) = session {
                    query.append_pair("session", &session.0);
                }
                if let Some(checkout) = checkout {
                    query.append_pair("checkout", checkout);
                }
                let query = query.finish();
                if query.is_empty() {
                    "/api/review".to_owned()
                } else {
                    format!("/api/review?{query}")
                }
            }
            Self::ReviewAction(action) => action.path().to_owned(),
        }
    }
}

/// The decoded fields of a query string, each named in an allowlist and given at most once.
struct QueryFields(Vec<(String, String)>);

impl QueryFields {
    fn parse(query: Option<&str>, allowed: &[&str]) -> Result<Self, ApiError> {
        let mut fields: Vec<(String, String)> = Vec::new();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            if !allowed.contains(&key.as_ref()) || fields.iter().any(|(seen, _)| *seen == key) {
                return Err(unknown_route());
            }
            fields.push((key.into_owned(), value.into_owned()));
        }
        Ok(Self(fields))
    }

    fn take(&mut self, key: &str) -> Option<String> {
        let index = self.0.iter().position(|(name, _)| name == key)?;
        Some(self.0.swap_remove(index).1)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_BODY_BYTES, MAX_COMMAND_BODY_BYTES, MAX_RESPONSE_BYTES, PROXIED_POLICY};
    use crate::web::testing::{Harness, Upstream};
    use axum::{
        Json, Router,
        body::{Body, Bytes},
        http::{HeaderMap, Method, Request, StatusCode, Uri, header},
        response::{IntoResponse, Response},
        routing::get,
    };
    use futures_util::{StreamExt as _, stream};
    use serde_json::json;
    use std::{convert::Infallible, time::Duration};
    use tokio::sync::mpsc;
    use tower::ServiceExt as _;

    const PEER_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage";

    /// A peer that answers every path: images where the route serves images, an event stream for
    /// `/api/stream`, and otherwise JSON naming the path it saw.
    async fn peer_answer(uri: Uri) -> Response<Body> {
        let path = uri.path();
        if path == "/api/file" || path.contains("/images/") {
            return ([(header::CONTENT_TYPE, "image/png")], PNG).into_response();
        }
        if path == "/api/stream" {
            return (
                [(header::CONTENT_TYPE, "text/event-stream")],
                "event: hello\ndata: {}\n\n",
            )
                .into_response();
        }
        if path == "/api/question/cancel" {
            return StatusCode::NO_CONTENT.into_response();
        }
        Json(json!({ "path": path })).into_response()
    }

    /// A harness with `devbox` linked to a peer served by `router`.
    async fn linked(router: Router) -> (Harness, Upstream) {
        let harness = Harness::new();
        let peer = Upstream::spawn(router).await;
        harness.link_machine("devbox", &peer.origin, PEER_TOKEN);
        (harness, peer)
    }

    /// A request as the browser application sends it.
    fn browser(
        harness: &Harness,
        method: Method,
        uri: &str,
        body: impl Into<Body>,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, harness.cookie())
            .header("x-tact", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.into())
            .unwrap()
    }

    #[tokio::test]
    async fn every_allowlisted_route_reaches_the_peer_exactly() {
        let (harness, peer) = linked(Router::new().fallback(peer_answer)).await;
        let get = Method::GET;
        let post = Method::POST;
        let routes = [
            (&get, "instance", "/api/instance", "application/json"),
            (&get, "stream", "/api/stream", "text/event-stream"),
            (&post, "cmd", "/api/cmd", "application/json"),
            (&post, "query", "/api/query", "application/json"),
            (
                &get,
                "file?path=a%20b%2F..%2Fc.png&session=claude-1_a",
                "/api/file?path=a+b%2F..%2Fc.png&session=claude-1_a",
                "image/png",
            ),
            (
                &get,
                "sessions/s-1/entries/7",
                "/api/sessions/s-1/entries/7",
                "application/json",
            ),
            (
                &get,
                "sessions/s-1/entries/7/images/0",
                "/api/sessions/s-1/entries/7/images/0",
                "image/png",
            ),
            (
                &get,
                "sessions/s-1/agents/2/entries",
                "/api/sessions/s-1/agents/2/entries",
                "application/json",
            ),
            (
                &get,
                "sessions/s-1/agents/2/entries/3",
                "/api/sessions/s-1/agents/2/entries/3",
                "application/json",
            ),
            (&get, "review", "/api/review", "application/json"),
            (
                &get,
                "review?checkout=%2Fwork%2Ftree&session=s-1",
                "/api/review?session=s-1&checkout=%2Fwork%2Ftree",
                "application/json",
            ),
            (
                &post,
                "review/compose",
                "/api/review/compose",
                "application/json",
            ),
            (&post, "refresh", "/api/refresh", "application/json"),
            (&post, "range", "/api/range", "application/json"),
            (&post, "questions", "/api/questions", "application/json"),
            (&post, "question/cancel", "/api/question/cancel", ""),
            (&post, "overview", "/api/overview", "application/json"),
            (&post, "ai-review", "/api/ai-review", "application/json"),
            (&post, "question", "/api/question", "application/json"),
        ];

        for (index, (method, path, upstream, content_type)) in routes.into_iter().enumerate() {
            let body = if *method == Method::POST {
                format!("{{\"n\":{index}}}")
            } else {
                String::new()
            };
            let request = browser(
                &harness,
                method.clone(),
                &format!("/api/m/devbox/{path}"),
                body.clone(),
            );

            let (status, headers, response) = harness.send(request).await;

            let expected_status = if path == "question/cancel" {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::OK
            };
            assert_eq!(
                status,
                expected_status,
                "{path}: {}",
                String::from_utf8_lossy(&response)
            );
            assert_eq!(
                headers
                    .get(header::CONTENT_TYPE)
                    .map(|value| value.to_str().unwrap())
                    .unwrap_or(""),
                content_type,
                "{path}"
            );
            let received = peer.received().pop().unwrap();
            assert_eq!(peer.hits(), index + 1, "{path}");
            assert_eq!(
                (&received.method, received.uri.as_str()),
                (method, upstream),
                "{path}"
            );
            assert_eq!(received.body, Bytes::from(body), "{path}");
        }
    }

    #[tokio::test]
    async fn the_peer_sees_only_the_hubs_headers() {
        let (harness, peer) = linked(Router::new().fallback(peer_answer)).await;
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/m/devbox/query")
            .header(header::HOST, "hub.example:7878")
            .header(header::ORIGIN, "http://hub.example:7878")
            .header(header::COOKIE, format!("{}; other=crumb", harness.cookie()))
            .header("x-tact", "1")
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::AUTHORIZATION, "Bearer browser")
            .header(header::REFERER, "http://hub.example:7878/")
            .header(header::USER_AGENT, "browser")
            .header("forwarded", "for=evil")
            .header("x-forwarded-for", "10.0.0.1")
            .header("x-http-method-override", "DELETE")
            .header(header::CONNECTION, "x-tact")
            .header(header::ACCEPT_ENCODING, "gzip")
            .body(Body::from("{}"))
            .unwrap();

        let (status, _, _) = harness.send(request).await;

        assert_eq!(status, StatusCode::OK);
        let received = peer.received().pop().unwrap();
        let mut names: Vec<&str> = received.headers.keys().map(|name| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "accept",
                "content-length",
                "content-type",
                "cookie",
                "host",
                "x-tact"
            ]
        );
        let header = |name: &str| received.headers[name].to_str().unwrap();
        assert_eq!(header("cookie"), format!("tact={PEER_TOKEN}"));
        assert_eq!(header("host"), format!("127.0.0.1:{}", peer.port));
        assert_eq!(header("content-type"), "application/json");
        assert_eq!(header("accept"), "application/json");
        assert_eq!(header("x-tact"), "1");
        assert!(!format!("{:?}", received.headers).contains(harness.state.token.expose()));
    }

    #[tokio::test]
    async fn no_peer_header_reaches_the_browser() {
        let (harness, _peer) = linked(Router::new().fallback(|| async {
            (
                [
                    (header::SET_COOKIE, "tact=stolen; Path=/"),
                    (header::CONTENT_SECURITY_POLICY, "default-src *"),
                    (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                    (header::LOCATION, "https://evil.example/"),
                    (header::REFRESH, "0; url=https://evil.example/"),
                    (header::CONTENT_DISPOSITION, "attachment; filename=x.html"),
                    (header::CONTENT_TYPE, "text/html"),
                    (header::CACHE_CONTROL, "public, max-age=99999"),
                ],
                "{\"ok\":true}",
            )
        }))
        .await;

        let (status, headers, body) = harness
            .send(browser(
                &harness,
                Method::GET,
                "/api/m/devbox/instance",
                Body::empty(),
            ))
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"{\"ok\":true}");
        for name in [
            header::SET_COOKIE,
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            header::LOCATION,
            header::REFRESH,
            header::CONTENT_DISPOSITION,
        ] {
            assert!(headers.get(&name).is_none(), "{name}");
        }
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], PROXIED_POLICY);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers["x-content-type-options"], "nosniff");
    }

    #[tokio::test]
    async fn peer_answers_are_mapped_to_statuses_and_content_the_hub_chooses() {
        let (harness, _peer) = linked(Router::new().fallback(|uri: Uri| async move {
            match uri.path() {
                "/api/instance" => {
                    (StatusCode::FOUND, [(header::LOCATION, "/api/link")]).into_response()
                }
                "/api/query" => StatusCode::UNAUTHORIZED.into_response(),
                "/api/cmd" => {
                    (StatusCode::CONFLICT, Json(json!({"code": "turn_running"}))).into_response()
                }
                "/api/refresh" => StatusCode::IM_A_TEAPOT.into_response(),
                "/api/range" => (
                    [(header::CONTENT_TYPE, "text/html")],
                    "<script>alert(1)</script>",
                )
                    .into_response(),
                "/api/file" => (
                    [(header::CONTENT_TYPE, "image/svg+xml")],
                    "<svg onload=alert(1)>",
                )
                    .into_response(),
                _ => (
                    [(header::CONTENT_TYPE, "image/png")],
                    "<html><script>alert(1)</script>",
                )
                    .into_response(),
            }
        }))
        .await;
        let cases = [
            (
                Method::GET,
                "instance",
                StatusCode::BAD_GATEWAY,
                "machine_protocol_error",
            ),
            (
                Method::POST,
                "query",
                StatusCode::CONFLICT,
                "machine_unauthorized",
            ),
            (Method::POST, "cmd", StatusCode::CONFLICT, "turn_running"),
            (
                Method::POST,
                "refresh",
                StatusCode::BAD_GATEWAY,
                "machine_protocol_error",
            ),
            (
                Method::POST,
                "range",
                StatusCode::BAD_GATEWAY,
                "machine_protocol_error",
            ),
            (
                Method::GET,
                "file?path=x.svg",
                StatusCode::BAD_GATEWAY,
                "machine_protocol_error",
            ),
            (
                Method::GET,
                "sessions/s/entries/1/images/0",
                StatusCode::BAD_GATEWAY,
                "machine_protocol_error",
            ),
        ];

        for (method, path, expected_status, expected_code) in cases {
            let (status, headers, body) = harness
                .send(browser(
                    &harness,
                    method,
                    &format!("/api/m/devbox/{path}"),
                    "{}",
                ))
                .await;

            assert_eq!(status, expected_status, "{path}");
            assert_eq!(headers[header::CONTENT_TYPE], "application/json", "{path}");
            assert_eq!(
                headers[header::CONTENT_SECURITY_POLICY],
                PROXIED_POLICY,
                "{path}"
            );
            assert!(headers.get(header::LOCATION).is_none(), "{path}");
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["code"], expected_code, "{path}");
            assert!(!body.to_string().contains(PEER_TOKEN), "{path}");
        }
    }

    #[tokio::test]
    async fn refused_requests_never_reach_the_peer() {
        let (harness, peer) = linked(Router::new().fallback(peer_answer)).await;
        let not_found = [
            "/api/m/devbox/login",
            "/api/m/devbox/link",
            "/api/m/devbox/instances",
            "/api/m/devbox/machines",
            "/api/m/devbox/m/devbox/instance",
            "/api/m/devbox/index.html",
            "/api/m/devbox/app.js",
            "/api/m/devbox/",
            "/api/m/devbox//instance",
            "/api/m/devbox/instance/",
            "/api/m/devbox/instance?x=1",
            "/api/m/devbox/stream?session=s",
            "/api/m/devbox/..%2Flink",
            "/api/m/devbox/..%2F..%2Fapi%2Flink",
            "/api/m/devbox/%2e%2e/link",
            "/api/m/devbox/%69nstance",
            "/api/m/devbox/sessions/..%2F..%2Flink/entries/1",
            "/api/m/devbox/sessions/s%2F..%2F..%2Flink/entries/1",
            "/api/m/devbox/sessions/a%5c..%5clink/entries/1",
            "/api/m/devbox/sessions/%252e%252e%252flink/entries/1",
            "/api/m/devbox/sessions/../entries/1",
            "/api/m/devbox/sessions/s.1/entries/1",
            "/api/m/devbox/sessions/s/entries/-1",
            "/api/m/devbox/sessions/s/entries/+1",
            "/api/m/devbox/sessions/s/entries/1x",
            "/api/m/devbox/sessions/s/entries/99999999999999999999999",
            "/api/m/devbox/sessions/s/agents/x/entries",
            "/api/m/devbox/file",
            "/api/m/devbox/file?path=a&path=b",
            "/api/m/devbox/file?path=a&evil=1",
            "/api/m/devbox/file?path=a&session=..%2Flink",
            "/api/m/devbox/review?session=s&session=t",
            "/api/m/nobody/instance",
            "/api/m/DEVBOX/instance",
            "/api/m/..%2Fdevbox/instance",
            "/api/m/devbox%2F..%2Fdevbox/instance",
        ];
        for uri in not_found {
            for method in [Method::GET, Method::POST] {
                let (status, _, _) = harness.send(browser(&harness, method, uri, "{}")).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            }
        }
        for (method, uri) in [
            (Method::POST, "/api/m/devbox/instance"),
            (Method::POST, "/api/m/devbox/stream"),
            (Method::GET, "/api/m/devbox/cmd"),
            (Method::GET, "/api/m/devbox/overview"),
            (Method::PUT, "/api/m/devbox/cmd"),
            (Method::DELETE, "/api/m/devbox/review"),
        ] {
            let (status, _, _) = harness.send(browser(&harness, method, uri, "{}")).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
        }
        let without_cookie = Request::builder()
            .uri("/api/m/devbox/instance")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            harness.send(without_cookie).await.0,
            StatusCode::UNAUTHORIZED
        );
        let without_x_tact = Request::builder()
            .method(Method::POST)
            .uri("/api/m/devbox/query")
            .header(header::COOKIE, harness.cookie())
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            harness.send(without_x_tact).await.0,
            StatusCode::BAD_REQUEST
        );
        let mut foreign = browser(
            &harness,
            Method::GET,
            "/api/m/devbox/instance",
            Body::empty(),
        );
        foreign
            .headers_mut()
            .insert(header::HOST, "hub.example".parse().unwrap());
        foreign
            .headers_mut()
            .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert_eq!(harness.send(foreign).await.0, StatusCode::BAD_REQUEST);
        for (path, limit) in [
            ("query", MAX_BODY_BYTES),
            ("overview", MAX_BODY_BYTES),
            ("cmd", MAX_COMMAND_BODY_BYTES),
        ] {
            let oversized = vec![b' '; limit + 1];
            let (status, _, _) = harness
                .send(browser(
                    &harness,
                    Method::POST,
                    &format!("/api/m/devbox/{path}"),
                    oversized,
                ))
                .await;
            assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        }

        assert_eq!(peer.hits(), 0);
    }

    #[tokio::test]
    async fn the_machine_list_names_machines_only_and_follows_the_registry() {
        let (harness, peer) = linked(Router::new().fallback(peer_answer)).await;
        harness.link_machine("laptop", "https://laptop.example.net", PEER_TOKEN);

        let (status, body) = harness.call(Method::GET, "/api/machines", None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"machines": [{"name": "devbox"}, {"name": "laptop"}]})
        );
        let instance = || {
            browser(
                &harness,
                Method::GET,
                "/api/m/devbox/instance",
                Body::empty(),
            )
        };
        assert_eq!(harness.send(instance()).await.0, StatusCode::OK);
        harness.state.machines.remove("devbox").unwrap();
        let (status, _, body) = harness.send(instance()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "unknown_machine");
        assert_eq!(peer.hits(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_machine_is_reported_without_details() {
        let harness = Harness::new();
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        harness.link_machine("devbox", &origin, PEER_TOKEN);

        let (status, body) = harness
            .call(Method::GET, "/api/m/devbox/instance", None)
            .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["code"], "machine_unreachable");
        assert_eq!(body["message"], "could not reach machine `devbox`");
    }

    /// A peer whose event stream sends what the test pushes and stays open otherwise.
    async fn streaming_peer() -> (Harness, Upstream, mpsc::UnboundedSender<&'static str>) {
        let (frames, receiver) = mpsc::unbounded_channel::<&'static str>();
        let receiver = std::sync::Arc::new(tokio::sync::Mutex::new(Some(receiver)));
        let (harness, peer) = linked(Router::new().route(
            "/api/stream",
            get(move || {
                let receiver = std::sync::Arc::clone(&receiver);
                async move {
                    let receiver = receiver.lock().await.take().unwrap();
                    let frames = stream::unfold(receiver, |mut receiver| async move {
                        let frame = receiver.recv().await?;
                        Some((
                            Ok::<_, Infallible>(Bytes::from_static(frame.as_bytes())),
                            receiver,
                        ))
                    });
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(frames),
                    )
                }
            }),
        ))
        .await;
        (harness, peer, frames)
    }

    async fn open_stream(
        harness: &Harness,
    ) -> (HeaderMap, impl futures_util::Stream<Item = Bytes>) {
        let response = harness
            .app
            .clone()
            .oneshot(browser(
                harness,
                Method::GET,
                "/api/m/devbox/stream",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        let frames = response
            .into_body()
            .into_data_stream()
            .map(|chunk| chunk.unwrap());
        (headers, frames)
    }

    #[tokio::test]
    async fn stream_frames_arrive_as_the_peer_sends_them() {
        let (harness, _peer, frames) = streaming_peer().await;
        frames.send("event: hello\ndata: {}\n\n").unwrap();

        let (headers, mut relayed) = open_stream(&harness).await;

        assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], PROXIED_POLICY);
        assert_eq!(relayed.next().await.unwrap(), "event: hello\ndata: {}\n\n");
        frames.send("event: live\ndata: {}\n\n").unwrap();
        assert_eq!(relayed.next().await.unwrap(), "event: live\ndata: {}\n\n");
        drop(frames);
        assert!(
            relayed.next().await.is_none(),
            "the relay ends with the peer's stream"
        );
    }

    // The clock is paused only once the connection is up: a paused clock skips ahead whenever the
    // runtime waits, which would also expire the connect timeout while a real socket connects.
    #[tokio::test]
    async fn a_silent_stream_is_closed() {
        let (harness, _peer, frames) = streaming_peer().await;
        frames.send(": keep-alive\n\n").unwrap();
        let (_, mut relayed) = open_stream(&harness).await;
        assert_eq!(relayed.next().await.unwrap(), ": keep-alive\n\n");

        tokio::time::pause();
        let started = tokio::time::Instant::now();
        assert!(relayed.next().await.is_none());

        assert!(started.elapsed() >= Duration::from_secs(45));
        drop(frames);
    }

    #[tokio::test]
    async fn hub_shutdown_closes_relayed_streams() {
        let (harness, _peer, frames) = streaming_peer().await;
        frames.send(": keep-alive\n\n").unwrap();
        let (_, mut relayed) = open_stream(&harness).await;
        relayed.next().await.unwrap();

        harness.shutdown.cancel();

        let ended = tokio::time::timeout(Duration::from_secs(1), relayed.next()).await;
        assert!(matches!(ended, Ok(None)));
        drop(frames);
    }

    #[tokio::test]
    async fn requests_that_run_an_agent_are_not_timed_out() {
        let (harness, peer) = linked(Router::new().fallback(|| async {
            tokio::time::sleep(Duration::from_secs(600)).await;
            Json(json!({"overview": "done"}))
        }))
        .await;
        let request = browser(&harness, Method::POST, "/api/m/devbox/overview", "{}");
        let response = tokio::spawn(harness.app.clone().oneshot(request));
        while peer.hits() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        tokio::time::pause();
        let response = response.await.unwrap().unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"{\"overview\":\"done\"}");
    }
    #[tokio::test]
    async fn a_stream_whose_headers_never_arrive_is_unreachable() {
        let (harness, peer) =
            linked(Router::new().route("/api/stream", get(std::future::pending::<StatusCode>)))
                .await;
        let request = browser(&harness, Method::GET, "/api/m/devbox/stream", Body::empty());
        let response = tokio::spawn(harness.app.clone().oneshot(request));
        while peer.hits() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        tokio::time::pause();
        let response = response.await.unwrap().unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "machine_unreachable");
    }

    #[tokio::test]
    async fn an_answer_over_the_buffer_cap_is_a_protocol_error() {
        let (harness, _peer) = linked(Router::new().fallback(|| async {
            (
                [(header::CONTENT_TYPE, "application/json")],
                vec![b' '; MAX_RESPONSE_BYTES + 1],
            )
        }))
        .await;

        let (status, body) = harness
            .call(Method::GET, "/api/m/devbox/instance", None)
            .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["code"], "machine_protocol_error");
    }
}
