//! Remote client behavior against stub servers: retries, bookmarks, and response validation.

use super::{RemoteClientError, RemoteMemoryClient, RemoteToken};
use crate::{
    MemoryCandidate, MemoryError, MemoryKey, MemoryStore,
    protocol::{
        self, ErrorResponse, ExportPage, ExportRequest, ListResponse, PutResponse, ReadResponse,
        RemoteErrorCode, ScanResponse, SyncReport,
    },
    test_support::{live_server, record},
};
use axum::{
    Json, Router,
    body::Body,
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::post,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinHandle;

fn alice(endpoint: &str) -> RemoteMemoryClient {
    RemoteMemoryClient::new(
        endpoint,
        "alice".to_owned(),
        RemoteToken::new("alice-test-token-000000000001".to_owned()).unwrap(),
    )
    .unwrap()
}

async fn serve(app: Router) -> (RemoteMemoryClient, JoinHandle<()>) {
    let (endpoint, task) = live_server(app).await;
    (alice(&endpoint), task)
}

#[derive(Clone)]
struct RetryState {
    list_calls: Arc<AtomicUsize>,
    put_calls: Arc<AtomicUsize>,
}

async fn retrying_list(
    axum::extract::State(state): axum::extract::State<RetryState>,
) -> Response<Body> {
    if state.list_calls.fetch_add(1, Ordering::SeqCst) == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                code: RemoteErrorCode::Unavailable,
                maximum: None,
            }),
        )
            .into_response();
    }
    Json(ListResponse {
        memories: Vec::new(),
    })
    .into_response()
}

#[derive(Clone, Default)]
struct BookmarkState {
    requests: Arc<Mutex<Vec<Option<String>>>>,
}

async fn bookmarked_list(
    axum::extract::State(state): axum::extract::State<BookmarkState>,
    headers: axum::http::HeaderMap,
) -> Response<Body> {
    let bookmark = headers
        .get(protocol::BOOKMARK_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.requests.lock().unwrap().push(bookmark.clone());

    tokio::time::sleep(Duration::from_millis(50)).await;
    let response_bookmark = match bookmark.as_deref() {
        None => "bookmark-1",
        Some("bookmark-1") => "bookmark-2",
        Some("bookmark-2") => "bookmark-3",
        _ => return StatusCode::CONFLICT.into_response(),
    };
    let mut response = Json(ListResponse {
        memories: Vec::new(),
    })
    .into_response();
    response.headers_mut().insert(
        protocol::BOOKMARK_HEADER,
        response_bookmark.parse().unwrap(),
    );
    response
}

async fn unavailable_put(
    axum::extract::State(state): axum::extract::State<RetryState>,
) -> Response<Body> {
    state.put_calls.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            code: RemoteErrorCode::Unavailable,
            maximum: None,
        }),
    )
        .into_response()
}

async fn rate_limited() -> StatusCode {
    StatusCode::TOO_MANY_REQUESTS
}

async fn oversized_list() -> Json<ListResponse> {
    Json(ListResponse {
        memories: (1..=protocol::MAX_LIST_RECORDS + 1)
            .map(|id| {
                let mut memory = record(id as i64, 1, "visible");
                memory.key = MemoryKey::remote("alice".to_owned(), id as i64, 1);
                memory
            })
            .collect(),
    })
}

async fn unsafe_scan() -> Json<ScanResponse> {
    Json(ScanResponse {
        candidates: vec![MemoryCandidate {
            key: MemoryKey::remote("alice".to_owned(), 1, 1),
            preview: "password=hunter2".to_owned(),
            score: 1.0,
        }],
    })
}

async fn oversized_scan() -> Json<ScanResponse> {
    Json(ScanResponse {
        candidates: (1..=2)
            .map(|id| MemoryCandidate {
                key: MemoryKey::remote("alice".to_owned(), id, 1),
                preview: format!("candidate {id}"),
                score: 1.0,
            })
            .collect(),
    })
}

async fn ambiguous_version_scan() -> Json<ScanResponse> {
    Json(ScanResponse {
        candidates: (1..=2)
            .map(|version| MemoryCandidate {
                key: MemoryKey::remote("alice".to_owned(), 1, version),
                preview: format!("version {version}"),
                score: 1.0,
            })
            .collect(),
    })
}

async fn ascending_score_scan() -> Json<ScanResponse> {
    Json(ScanResponse {
        candidates: (1..=2)
            .map(|id| MemoryCandidate {
                key: MemoryKey::remote("alice".to_owned(), id, 1),
                preview: format!("candidate {id}"),
                score: id as f64,
            })
            .collect(),
    })
}

async fn oversized_export() -> Json<ExportPage> {
    Json(ExportPage {
        memories: (1..=2)
            .map(|id| {
                let mut memory = record(id, 1, &format!("memory {id}"));
                memory.key = MemoryKey::remote("alice".to_owned(), id, 1);
                memory
            })
            .collect(),
        next_cursor: None,
    })
}

async fn impossible_sync_report() -> Json<SyncReport> {
    Json(SyncReport {
        inserted: 2,
        replaced: 0,
        unchanged: 0,
        deleted: 0,
    })
}

async fn unrelated_put() -> Json<PutResponse> {
    let mut memory = record(2, 1, "different content");
    memory.key = MemoryKey::remote("alice".to_owned(), 2, 1);
    Json(PutResponse { memory })
}

async fn equivalent_content_read() -> Json<ReadResponse> {
    let mut alice = record(1, 1, "shared operating note");
    alice.key = MemoryKey::remote("alice".to_owned(), 1, 1);
    let mut bob = record(1, 1, "shared operating note");
    bob.key = MemoryKey::remote("bob".to_owned(), 1, 1);
    Json(ReadResponse {
        memories: vec![alice, bob],
    })
}

async fn ambiguous_version_read() -> Json<ReadResponse> {
    Json(ReadResponse {
        memories: (1..=3)
            .map(|version| {
                let mut memory = record(1, version, &format!("version {version}"));
                memory.key = MemoryKey::remote("alice".to_owned(), 1, version);
                memory
            })
            .collect(),
    })
}

async fn ambiguous_version_list() -> Json<ListResponse> {
    Json(ListResponse {
        memories: (1..=2)
            .map(|version| {
                let mut memory = record(1, version, &format!("version {version}"));
                memory.key = MemoryKey::remote("alice".to_owned(), 1, version);
                memory
            })
            .collect(),
    })
}

#[tokio::test]
async fn client_retries_safe_operations_but_does_not_replay_put_responses() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = RetryState {
        list_calls: Arc::new(AtomicUsize::new(0)),
        put_calls: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route(&format!("/{}", protocol::LIST_PATH), post(retrying_list))
        .route(&format!("/{}", protocol::PUT_PATH), post(unavailable_put))
        .with_state(state.clone());
    let (client, task) = serve(app).await;

    assert!(client.list().await.unwrap().is_empty());
    assert_eq!(state.list_calls.load(Ordering::SeqCst), 2);
    let error = client.put("one-shot put", None).await.unwrap_err();
    let MemoryError::Unavailable { source } = error else {
        panic!("expected unavailable error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::Rejected {
            code: RemoteErrorCode::Unavailable,
            maximum: None,
        })
    ));
    assert_eq!(state.put_calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn client_carries_bookmarks_across_concurrent_clones() {
    let state = BookmarkState::default();
    let app = Router::new()
        .route(&format!("/{}", protocol::LIST_PATH), post(bookmarked_list))
        .with_state(state.clone());
    let (client, task) = serve(app).await;
    let clone = client.clone();

    let (first, second) = tokio::join!(client.list(), clone.list());
    assert!(first.unwrap().is_empty());
    assert!(second.unwrap().is_empty());
    assert!(client.list().await.unwrap().is_empty());
    assert_eq!(
        *state.requests.lock().unwrap(),
        vec![
            None,
            Some("bookmark-1".to_owned()),
            Some("bookmark-2".to_owned()),
        ]
    );
    task.abort();
}

#[tokio::test]
async fn client_preserves_empty_content_and_exhausted_rate_limit_errors() {
    let client = alice("http://127.0.0.1:1/");
    assert!(matches!(
        client.put("   ", None).await,
        Err(MemoryError::EmptyContent)
    ));

    let app = Router::new().route(&format!("/{}", protocol::LIST_PATH), post(rate_limited));
    let (client, task) = serve(app).await;
    assert!(matches!(
        client.list().await,
        Err(MemoryError::Unavailable { .. })
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_secret_content_before_sending() {
    let client = alice("http://127.0.0.1:1/");
    let secret = "password=hunter2";

    assert!(matches!(
        client.put(secret, None).await,
        Err(MemoryError::SecretRejected)
    ));
    assert!(matches!(
        client.sync(&[record(1, 1, secret)]).await,
        Err(MemoryError::SecretRejected)
    ));
}

#[tokio::test]
async fn client_reports_a_missing_versioned_session_route_as_incompatible() {
    let (client, task) = serve(Router::new()).await;

    assert!(matches!(
        client.session().await,
        Err(RemoteClientError::IncompatibleProtocol)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_an_unbounded_list_window() {
    let app = Router::new().route(&format!("/{}", protocol::LIST_PATH), post(oversized_list));
    let (client, task) = serve(app).await;

    let error = client.list().await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_suppresses_unsafe_scan_previews() {
    let app = Router::new().route(&format!("/{}", protocol::SCAN_PATH), post(unsafe_scan));
    let (client, task) = serve(app).await;

    assert!(
        client
            .scan("password", 5)
            .await
            .unwrap()
            .candidates
            .is_empty()
    );
    task.abort();
}

#[tokio::test]
async fn client_rejects_oversized_scan_responses() {
    let app = Router::new().route(&format!("/{}", protocol::SCAN_PATH), post(oversized_scan));
    let (client, task) = serve(app).await;

    let error = client.scan("candidate", 1).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_ambiguous_versions_in_scan_responses() {
    let app = Router::new().route(
        &format!("/{}", protocol::SCAN_PATH),
        post(ambiguous_version_scan),
    );
    let (client, task) = serve(app).await;

    let error = client.scan("version", 2).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_scan_responses_out_of_rank_order() {
    let app = Router::new().route(
        &format!("/{}", protocol::SCAN_PATH),
        post(ascending_score_scan),
    );
    let (client, task) = serve(app).await;

    let error = client.scan("candidate", 2).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_enforces_the_requested_export_page_size() {
    let app = Router::new().route(
        &format!("/{}", protocol::EXPORT_PATH),
        post(oversized_export),
    );
    let (client, task) = serve(app).await;

    let error = client.export_page(None, None, 1).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_does_not_retry_unrecoverable_export_responses() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let app = Router::new().route(
        &format!("/{}", protocol::EXPORT_PATH),
        post(move |Json(request): Json<ExportRequest>| {
            let observed = observed.clone();
            async move {
                observed.lock().unwrap().push(request.limit);
                if request.limit == 1 {
                    let mut memory = record(1, 1, &"x".repeat(8 * 1_024 * 1_024));
                    memory.key.namespace = Some("alice".to_owned());
                    return Json(ExportPage {
                        memories: vec![memory],
                        next_cursor: None,
                    })
                    .into_response();
                }
                "{".into_response()
            }
        }),
    );
    let (client, task) = serve(app).await;

    let error = client.export_page(None, None, 1).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::ResponseTooLarge)
    ));
    assert_eq!(*requests.lock().unwrap(), [1]);

    let error = client
        .export_page(None, None, protocol::MAX_EXPORT_PAGE_RECORDS)
        .await
        .unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    assert_eq!(
        *requests.lock().unwrap(),
        [1, protocol::MAX_EXPORT_PAGE_RECORDS]
    );
    task.abort();
}

#[tokio::test]
async fn client_rejects_sync_reports_that_do_not_match_the_snapshot() {
    let app = Router::new().route(
        &format!("/{}", protocol::SYNC_PATH),
        post(impossible_sync_report),
    );
    let (client, task) = serve(app).await;

    let error = client.sync(&[record(1, 1, "snapshot")]).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_put_responses_unrelated_to_the_request() {
    let app = Router::new().route(&format!("/{}", protocol::PUT_PATH), post(unrelated_put));
    let (client, task) = serve(app).await;

    for replacement in [None, Some(MemoryKey::remote("alice".to_owned(), 1, 1))] {
        let error = client
            .put("submitted content", replacement)
            .await
            .unwrap_err();
        let MemoryError::Backend { source } = error else {
            panic!("expected backend error, got {error:?}");
        };
        assert!(matches!(
            source.downcast_ref::<RemoteClientError>(),
            Some(RemoteClientError::InvalidResponse)
        ));
    }
    task.abort();
}

#[tokio::test]
async fn client_preserves_equivalent_content_from_distinct_namespaces() {
    let app = Router::new().route(
        &format!("/{}", protocol::READ_PATH),
        post(equivalent_content_read),
    );
    let (client, task) = serve(app).await;

    let memories = client
        .read(
            &[],
            &[
                MemoryKey::remote("alice".to_owned(), 1, 1),
                MemoryKey::remote("bob".to_owned(), 1, 1),
            ],
        )
        .await
        .unwrap();
    assert_eq!(memories.len(), 2);
    task.abort();
}

#[tokio::test]
async fn client_rejects_ambiguous_versions_for_an_unversioned_id() {
    let app = Router::new().route(
        &format!("/{}", protocol::READ_PATH),
        post(ambiguous_version_read),
    );
    let (client, task) = serve(app).await;

    let error = client.read(&[1], &[]).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_ignores_namespace_less_remote_keys() {
    let client = alice("http://127.0.0.1:1/");

    assert!(
        client
            .read(&[], &[MemoryKey::local(1, 1)])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn client_rejects_ambiguous_versions_in_list_responses() {
    let app = Router::new().route(
        &format!("/{}", protocol::LIST_PATH),
        post(ambiguous_version_list),
    );
    let (client, task) = serve(app).await;

    let error = client.list().await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}
