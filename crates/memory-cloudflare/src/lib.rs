//! Cloudflare Worker deployment for the shared Tact memory protocol.
//!
//! The Worker binds a D1 database to the Cloudflare memory store and delegates the HTTP protocol,
//! authentication, authorization, request bounds, and error surface to [`tact_memory`].

mod auth;
mod config;
mod store;

use auth::{CREDENTIALS_SECRET, parse_credentials};
use axum::body::Body;
use config::scan_budget;
use std::sync::{Arc, Once};
use store::CloudflareMemoryStore;
use tact_memory::server::MemoryServer;
use tower::ServiceExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_web::{MakeConsoleWriter, performance_layer};
use worker::{Context, Env, HttpRequest, Result, event};
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
    let database = Arc::new(environment.d1(DATABASE_BINDING)?);
    // Cloudflare owns another non-zeroizing copy behind `Secret`. This crate zeroizes only the
    // Rust string returned by the binding and every credential copy it constructs.
    let document = Zeroizing::new(environment.secret(CREDENTIALS_SECRET)?.to_string());
    let credentials = parse_credentials(document).map_err(worker_error)?;
    let scan_budget = scan_budget(&environment).map_err(worker_error)?;
    let server = MemoryServer::new(
        move |namespace| CloudflareMemoryStore::new(Arc::clone(&database), namespace, scan_budget),
        credentials,
    )
    .map_err(worker_error)?;
    server
        .router()
        .oneshot(request.map(Body::new))
        .await
        .map_err(worker_error)
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
