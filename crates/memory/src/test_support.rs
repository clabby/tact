//! Fixtures shared by store, client, and server tests.

use crate::{MemoryKey, MemoryRecord};
use axum::Router;
use tokio::{net::TcpListener, task::JoinHandle};

/// Builds a local record whose timestamps derive from its version.
pub(crate) fn record(id: i64, version: u64, content: &str) -> MemoryRecord {
    MemoryRecord {
        key: MemoryKey::local(id, version),
        content: content.to_owned(),
        created_at_ms: 10,
        updated_at_ms: 10 + i64::try_from(version).unwrap(),
        last_scanned_at_ms: None,
        scan_count: 0,
        last_used_at_ms: None,
        use_count: 0,
        probation_until_ms: None,
    }
}

/// Serves `router` on an ephemeral loopback port and returns its base URL.
pub(crate) async fn live_server(router: Router) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/"), task)
}
