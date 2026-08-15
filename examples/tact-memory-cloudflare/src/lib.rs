//! Cloudflare Worker example for the shared Tact memory protocol.
//!
//! The Worker binds a D1 database to the Cloudflare memory store and delegates the HTTP protocol,
//! authentication, authorization, request bounds, and error surface to [`tact_memory`].

mod auth;
mod config;
mod store;

use auth::{CREDENTIALS_SECRET, parse_credentials};
use axum::{
    Json,
    body::Body,
    http::{HeaderValue, StatusCode},
    response::IntoResponse,
};
use config::scan_budget;
use std::sync::{Arc, Once};
use store::CloudflareMemoryStore;
use tact_memory::server::{
    MemoryServer,
    protocol::{self, ErrorResponse, RemoteErrorCode},
};
use tower::ServiceExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_web::{MakeConsoleWriter, performance_layer};
use worker::{Context, D1SessionConstraint, Env, HttpRequest, Result, event};
use zeroize::Zeroizing;

const DATABASE_BINDING: &str = "DB";
static TRACING: Once = Once::new();

/// Serves one authenticated memory protocol request against the bound D1 database.
#[event(fetch)]
pub async fn fetch(
    request: HttpRequest,
    environment: Env,
    _context: Context,
) -> Result<axum::response::Response> {
    initialize_tracing();

    let bookmark_headers = request.headers().get_all(protocol::D1_BOOKMARK_HEADER);
    let mut bookmarks = bookmark_headers.iter();
    let bookmark = match (bookmarks.next(), bookmarks.next()) {
        (None, None) => None,
        (Some(value), None) => match value.to_str() {
            Ok(value) if protocol::is_valid_d1_bookmark(value) => Some(value.to_owned()),
            _ => return Ok(bad_request()),
        },
        _ => return Ok(bad_request()),
    };

    // A new session may begin on a stale nearby replica; a supplied bookmark resumes at least as
    // fresh as the client's preceding response. D1 forwards mutations to the primary.
    let database = environment.d1(DATABASE_BINDING)?;
    let session = Arc::new(match bookmark.as_deref() {
        Some(bookmark) => database.with_session(Some(bookmark))?,
        None => database.with_session_constraint(D1SessionConstraint::FirstUnconstrained)?,
    });

    // Cloudflare owns another non-zeroizing copy behind `Secret`. This crate zeroizes only the
    // Rust string returned by the binding and every credential copy it constructs.
    let document = Zeroizing::new(environment.secret(CREDENTIALS_SECRET)?.to_string());
    let credentials = parse_credentials(document).map_err(worker_error)?;
    let scan_budget = scan_budget(&environment).map_err(worker_error)?;
    let store_session = Arc::clone(&session);
    let server = MemoryServer::new(
        move |namespace| {
            CloudflareMemoryStore::new(Arc::clone(&store_session), namespace, scan_budget)
        },
        credentials,
    )
    .map_err(worker_error)?;
    let mut response = server
        .router()
        .oneshot(request.map(Body::new))
        .await
        .map_err(worker_error)?;
    if let Some(bookmark) = session.get_bookmark()? {
        if !protocol::is_valid_d1_bookmark(&bookmark) {
            return Err(invalid_d1_bookmark());
        }
        let value = HeaderValue::from_str(&bookmark).map_err(|_| invalid_d1_bookmark())?;
        response
            .headers_mut()
            .insert(protocol::D1_BOOKMARK_HEADER, value);
    }
    Ok(response)
}

fn bad_request() -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            code: RemoteErrorCode::BadRequest,
        }),
    )
        .into_response()
}

fn invalid_d1_bookmark() -> worker::Error {
    worker::Error::RustError("D1 returned an invalid session bookmark".to_owned())
}

fn initialize_tracing() {
    TRACING.call_once(|| {
        tracing_subscriber::registry()
            .with(performance_layer())
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(MakeConsoleWriter),
            )
            .init();
    });
}

fn worker_error(error: impl std::error::Error) -> worker::Error {
    worker::Error::RustError(error.to_string())
}
