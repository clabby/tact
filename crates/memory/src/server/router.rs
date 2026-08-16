//! Authenticated HTTP routing for the remote-memory protocol.

use super::{
    credential::{Credential, Principal, hash_token, is_bearer_token_byte},
    protocol::{
        self, DeleteRequest, ErrorResponse, ExportRequest, ExportResponse, ListResponse,
        PutRequest, PutResponse, ReadRequest, ReadResponse, RemoteErrorCode, RemoteRole,
        ScanRequest, ScanResponse, ScanScope, SessionResponse, SyncRequest,
    },
};
use crate::{
    MemoryError, MemoryKey, MemoryLimits, MemoryNamespaceFilter, MemoryRecord, MemoryStore,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::{HeaderMap, Response, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
#[cfg(feature = "native-server")]
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
};
use thiserror::Error;
#[cfg(feature = "native-server")]
use tower::limit::ConcurrencyLimitLayer;
use tracing::info;
use web_time::Instant;

/// Covers worst-case JSON escaping for a full local corpus while bounding allocation.
pub(crate) const MAX_JSON_BODY_BYTES: usize = 2 * 1024 * 1024;

#[cfg(feature = "native-server")]
const MAX_IN_FLIGHT_REQUESTS: usize = 64;
#[cfg(feature = "native-server")]
const STORE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Failure to assemble the server's authentication table.
#[derive(Debug, Error)]
pub enum ServerBuildError {
    /// No credentials were supplied, so every request would be unauthorized.
    #[error("at least one memory credential is required")]
    NoCredentials,
    /// Two credentials supplied the same bearer token.
    #[error("duplicate bearer tokens are not allowed")]
    DuplicateBearerToken,
}

struct ServerState<S> {
    store_factory: Arc<dyn Fn(String) -> S + Send + Sync>,
    principals: HashMap<[u8; 32], Principal>,
}

/// A cloneable authenticated server for a concrete namespace-bound store type.
///
/// Retained authentication state contains only token hashes and principals. The namespace factory
/// runs only after authentication and receives the authenticated namespace rather than a
/// client-controlled value.
#[derive(Clone)]
pub struct MemoryServer<S> {
    state: Arc<ServerState<S>>,
}

impl<S: MemoryStore> MemoryServer<S> {
    /// Builds a server from a namespace factory and startup credentials.
    ///
    /// Construction rejects an empty credential set and duplicate bearer tokens. The factory may
    /// cheaply bind shared storage to its argument; it must not trust any other namespace source.
    pub fn new(
        store_factory: impl Fn(String) -> S + Send + Sync + 'static,
        credentials: impl IntoIterator<Item = Credential>,
    ) -> Result<Self, ServerBuildError> {
        let mut principals = HashMap::new();
        for credential in credentials {
            let (token_hash, principal) = credential.into_hashed_principal();
            if principals.insert(token_hash, principal).is_some() {
                return Err(ServerBuildError::DuplicateBearerToken);
            }
        }
        if principals.is_empty() {
            return Err(ServerBuildError::NoCredentials);
        }
        Ok(Self {
            state: Arc::new(ServerState {
                store_factory: Arc::new(store_factory),
                principals,
            }),
        })
    }

    /// Constructs an Axum router sharing this server's credentials and namespace factory.
    ///
    /// Each router enforces a two-MiB JSON body limit. The `native-server` feature additionally
    /// limits the process to 64 in-flight requests and applies a 30-second store timeout.
    pub fn router(&self) -> Router {
        let router = Router::new()
            .route(&route(protocol::SESSION_PATH), get(session))
            .route(&route(protocol::SCAN_PATH), post(scan))
            .route(&route(protocol::READ_PATH), post(read))
            .route(&route(protocol::LIST_PATH), post(list))
            .route(&route(protocol::PUT_PATH), post(put))
            .route(&route(protocol::DELETE_PATH), post(delete))
            .route(&route(protocol::SYNC_PATH), post(sync))
            .route(&route(protocol::EXPORT_PATH), post(export))
            .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES));
        #[cfg(feature = "native-server")]
        let router = router.layer(ConcurrencyLimitLayer::new(MAX_IN_FLIGHT_REQUESTS));
        router.with_state(self.state.clone())
    }
}

fn route(path: &str) -> String {
    format!("/{path}")
}

async fn session<S: MemoryStore>(
    State(state): State<Arc<ServerState<S>>>,
    headers: HeaderMap,
) -> Response<Body> {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let operation = OperationTrace::new("session", &principal);
    let response = Json(SessionResponse {
        protocol_version: crate::VERSION,
        namespace: principal.namespace.clone(),
        role: principal.role,
    })
    .into_response();
    operation.success(OperationCounts::default());
    response
}

async fn scan<S: MemoryStore>(
    State(state): State<Arc<ServerState<S>>>,
    headers: HeaderMap,
    payload: Result<Json<ScanRequest>, JsonRejection>,
) -> Response<Body> {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let operation = OperationTrace::new("scan", &principal);
    let request = match json_payload(payload) {
        Ok(request) => request,
        Err(error) => return operation.error_response(error, OperationCounts::default()),
    };
    let counts = OperationCounts::input(1);
    if request.query.len() > MemoryLimits::PRODUCTION.query_bytes
        || request.limit == 0
        || request.limit > MemoryLimits::PRODUCTION.scan_results
    {
        let error = if request.query.len() > MemoryLimits::PRODUCTION.query_bytes {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                RemoteErrorCode::QueryTooLarge,
            )
        } else {
            ApiError::bad_request()
        };
        return operation.error_response(error, counts);
    }
    let namespaces = match request.scope {
        ScanScope::All => MemoryNamespaceFilter::All,
        ScanScope::Own => MemoryNamespaceFilter::Exact(principal.namespace.clone()),
        ScanScope::Others => MemoryNamespaceFilter::OtherThan(principal.namespace.clone()),
    };
    let store = (state.store_factory)(principal.namespace);
    match run_store(
        operation,
        counts,
        store.scan(&request.query, namespaces, request.limit),
        |candidates| OperationCounts::candidates(candidates.len()),
    )
    .await
    {
        Ok(candidates) => Json(ScanResponse { candidates }).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn read<S: MemoryStore>(
    State(state): State<Arc<ServerState<S>>>,
    headers: HeaderMap,
    payload: Result<Json<ReadRequest>, JsonRejection>,
) -> Response<Body> {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let operation = OperationTrace::new("read", &principal);
    let request = match json_payload(payload) {
        Ok(request) => request,
        Err(error) => return operation.error_response(error, OperationCounts::default()),
    };
    let counts = OperationCounts::input(request.ids.len().saturating_add(request.keys.len()));
    if counts.input_count > MemoryLimits::PRODUCTION.records
        || request.ids.iter().any(|id| *id <= 0)
        || request.keys.iter().any(|key| !valid_key(key))
    {
        return operation.error_response(ApiError::bad_request(), counts);
    }
    let store = (state.store_factory)(principal.namespace);
    match run_store(
        operation,
        counts,
        store.read(&request.ids, &request.keys),
        |memories| OperationCounts::records(memories.len()),
    )
    .await
    {
        Ok(memories) => Json(ReadResponse { memories }).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn list<S: MemoryStore>(
    State(state): State<Arc<ServerState<S>>>,
    headers: HeaderMap,
) -> Response<Body> {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let operation = OperationTrace::new("list", &principal);
    let store = (state.store_factory)(principal.namespace);
    match run_store(
        operation,
        OperationCounts::default(),
        store.list(),
        |memories| OperationCounts::records(memories.len()),
    )
    .await
    {
        Ok(memories) => Json(ListResponse { memories }).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn put<S: MemoryStore>(
    State(state): State<Arc<ServerState<S>>>,
    headers: HeaderMap,
    payload: Result<Json<PutRequest>, JsonRejection>,
) -> Response<Body> {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let operation = OperationTrace::new("put", &principal);
    if principal.role != RemoteRole::Writer {
        return operation.error_response(
            ApiError::new(StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden),
            OperationCounts::default(),
        );
    }
    let request = match json_payload(payload) {
        Ok(request) => request,
        Err(error) => return operation.error_response(error, OperationCounts::default()),
    };
    let counts = OperationCounts::input(1);
    if request.content.trim().is_empty() {
        return operation.error_response(ApiError::bad_request(), counts);
    }
    if request.content.len() > MemoryLimits::PRODUCTION.content_bytes {
        return operation.error_response(
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                RemoteErrorCode::ContentTooLarge,
            ),
            counts,
        );
    }
    if request.replacement.as_ref().is_some_and(|key| {
        !valid_key(key) || key.namespace.as_deref() != Some(principal.namespace.as_str())
    }) {
        return operation.error_response(
            ApiError::new(StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden),
            counts,
        );
    }
    let store = (state.store_factory)(principal.namespace);
    match run_store(
        operation,
        counts,
        store.put(&request.content, request.replacement),
        |_| OperationCounts::records(1),
    )
    .await
    {
        Ok(memory) => Json(PutResponse { memory }).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn delete<S: MemoryStore>(
    State(state): State<Arc<ServerState<S>>>,
    headers: HeaderMap,
    payload: Result<Json<DeleteRequest>, JsonRejection>,
) -> Response<Body> {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let operation = OperationTrace::new("delete", &principal);
    if principal.role != RemoteRole::Writer {
        return operation.error_response(
            ApiError::new(StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden),
            OperationCounts::default(),
        );
    }
    let request = match json_payload(payload) {
        Ok(request) => request,
        Err(error) => return operation.error_response(error, OperationCounts::default()),
    };
    let counts = OperationCounts::input(1);
    if !valid_key(&request.key)
        || request.key.namespace.as_deref() != Some(principal.namespace.as_str())
    {
        return operation.error_response(
            ApiError::new(StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden),
            counts,
        );
    }
    let store = (state.store_factory)(principal.namespace);
    match run_store(operation, counts, store.delete(request.key), |_| {
        OperationCounts::records(1)
    })
    .await
    {
        Ok(()) => Json(serde_json::json!({})).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn sync<S: MemoryStore>(
    State(state): State<Arc<ServerState<S>>>,
    headers: HeaderMap,
    payload: Result<Json<SyncRequest>, JsonRejection>,
) -> Response<Body> {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let operation = OperationTrace::new("sync", &principal);
    if principal.role != RemoteRole::Writer {
        return operation.error_response(
            ApiError::new(StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden),
            OperationCounts::default(),
        );
    }
    let request = match json_payload(payload) {
        Ok(request) => request,
        Err(error) => return operation.error_response(error, OperationCounts::default()),
    };
    let counts = OperationCounts::input(request.memories.len());
    if !valid_snapshot(&request.memories) {
        return operation.error_response(ApiError::bad_request(), counts);
    }
    let store = (state.store_factory)(principal.namespace);
    match run_store(operation, counts, store.sync(&request.memories), |report| {
        OperationCounts::report(*report)
    })
    .await
    {
        Ok(report) => Json(report).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn export<S: MemoryStore>(
    State(state): State<Arc<ServerState<S>>>,
    headers: HeaderMap,
    payload: Result<Json<ExportRequest>, JsonRejection>,
) -> Response<Body> {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let operation = OperationTrace::new("export", &principal);
    let request = match json_payload(payload) {
        Ok(request) => request,
        Err(error) => return operation.error_response(error, OperationCounts::default()),
    };
    let counts = OperationCounts::input(
        request
            .namespaces
            .as_ref()
            .map_or(0, |namespaces| namespaces.len()),
    );
    if request.limit == 0
        || request.limit > protocol::MAX_EXPORT_PAGE_RECORDS
        || request.namespaces.as_ref().is_some_and(|namespaces| {
            namespaces.is_empty()
                || namespaces.len() > MemoryLimits::PRODUCTION.records
                || namespaces
                    .iter()
                    .any(|namespace| !protocol::is_valid_namespace(namespace))
        })
        || request.cursor.as_ref().is_some_and(|cursor| {
            !protocol::is_valid_namespace(&cursor.namespace) || cursor.id <= 0
        })
    {
        return operation.error_response(ApiError::bad_request(), counts);
    }
    let store = (state.store_factory)(principal.namespace);
    match run_store(
        operation,
        counts,
        store.export_page(
            request.namespaces.as_deref(),
            request.cursor.as_ref(),
            request.limit,
        ),
        |(memories, _)| OperationCounts::records(memories.len()),
    )
    .await
    {
        Ok((memories, next_cursor)) => Json(ExportResponse {
            memories,
            next_cursor,
        })
        .into_response(),
        Err(error) => error.into_response(),
    }
}

fn json_payload<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    payload.map(|Json(value)| value).map_err(|rejection| {
        let status = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError::new(status, RemoteErrorCode::BadRequest)
    })
}

fn authenticate<S>(state: &ServerState<S>, headers: &HeaderMap) -> Result<Principal, ApiError> {
    // Hyper owns the incoming HeaderMap and may retain non-zeroizing header storage outside this
    // crate. Authentication only borrows that storage long enough to hash the token and never
    // clones the raw value into server state.
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiError::unauthorized)?;
    let (scheme, token) = authorization
        .split_once(' ')
        .filter(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("bearer")
                && !token.is_empty()
                && token.bytes().all(is_bearer_token_byte)
        })
        .ok_or_else(ApiError::unauthorized)?;
    let _ = scheme;
    let token_hash = hash_token(token);
    let principal = state
        .principals
        .get(&token_hash)
        .cloned()
        .ok_or_else(ApiError::unauthorized)?;
    let asserted_namespace = headers
        .get(protocol::NAMESPACE_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiError::namespace_mismatch)?;
    if asserted_namespace != principal.namespace {
        return Err(ApiError::namespace_mismatch());
    }
    Ok(principal)
}

fn valid_key(key: &MemoryKey) -> bool {
    key.id > 0
        && key.version > 0
        && key
            .namespace
            .as_deref()
            .is_none_or(protocol::is_valid_namespace)
}

fn valid_snapshot(memories: &[MemoryRecord]) -> bool {
    let mut ids = HashSet::with_capacity(memories.len());
    memories.len() <= MemoryLimits::PRODUCTION.records
        && memories.iter().all(|memory| {
            memory.key.is_local()
                && valid_key(&memory.key)
                && ids.insert(memory.key.id)
                && !memory.content.trim().is_empty()
                && memory.content.len() <= MemoryLimits::PRODUCTION.content_bytes
                && memory.created_at_ms >= 0
                && memory.updated_at_ms >= memory.created_at_ms
        })
        && memories
            .iter()
            .map(|memory| memory.content.len())
            .try_fold(0usize, usize::checked_add)
            .is_some_and(|bytes| bytes <= MemoryLimits::PRODUCTION.total_content_bytes)
}

#[derive(Clone, Copy, Default)]
struct OperationCounts {
    candidate_count: usize,
    record_count: usize,
    input_count: usize,
    report_inserted: usize,
    report_replaced: usize,
    report_unchanged: usize,
    report_deleted: usize,
}

impl OperationCounts {
    fn candidates(candidate_count: usize) -> Self {
        Self {
            candidate_count,
            ..Self::default()
        }
    }

    fn records(record_count: usize) -> Self {
        Self {
            record_count,
            ..Self::default()
        }
    }

    fn input(input_count: usize) -> Self {
        Self {
            input_count,
            ..Self::default()
        }
    }

    fn report(report: protocol::SyncReport) -> Self {
        Self {
            report_inserted: report.inserted,
            report_replaced: report.replaced,
            report_unchanged: report.unchanged,
            report_deleted: report.deleted,
            ..Self::default()
        }
    }
}

struct OperationTrace {
    operation: &'static str,
    namespace: String,
    role: RemoteRole,
    started_at: Instant,
}

impl OperationTrace {
    fn new(operation: &'static str, principal: &Principal) -> Self {
        Self {
            operation,
            namespace: principal.namespace.clone(),
            role: principal.role,
            started_at: Instant::now(),
        }
    }

    fn success(self, counts: OperationCounts) {
        info!(
            operation = self.operation,
            namespace = %self.namespace,
            role = ?self.role,
            success = true,
            elapsed_ms = self.started_at.elapsed().as_millis() as u64,
            candidate_count = counts.candidate_count,
            record_count = counts.record_count,
            input_count = counts.input_count,
            report_inserted = counts.report_inserted,
            report_replaced = counts.report_replaced,
            report_unchanged = counts.report_unchanged,
            report_deleted = counts.report_deleted,
            "remote memory operation"
        );
    }

    fn failure(self, error: ApiError, counts: OperationCounts) {
        info!(
            operation = self.operation,
            namespace = %self.namespace,
            role = ?self.role,
            success = false,
            elapsed_ms = self.started_at.elapsed().as_millis() as u64,
            status = error.status.as_u16(),
            error_code = ?error.code,
            candidate_count = counts.candidate_count,
            record_count = counts.record_count,
            input_count = counts.input_count,
            report_inserted = counts.report_inserted,
            report_replaced = counts.report_replaced,
            report_unchanged = counts.report_unchanged,
            report_deleted = counts.report_deleted,
            "remote memory operation"
        );
    }

    fn error_response(self, error: ApiError, counts: OperationCounts) -> Response<Body> {
        self.failure(error, counts);
        error.into_response()
    }
}

async fn run_store<T, C>(
    trace: OperationTrace,
    request_counts: OperationCounts,
    operation: impl Future<Output = Result<T, MemoryError>> + Send,
    success_counts: C,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    C: FnOnce(&T) -> OperationCounts,
{
    #[cfg(not(feature = "native-server"))]
    let result = operation.await;
    #[cfg(feature = "native-server")]
    let result = match tokio::time::timeout(STORE_OPERATION_TIMEOUT, operation).await {
        Err(_) => {
            let error = ApiError::unavailable();
            trace.failure(error, request_counts);
            return Err(error);
        }
        Ok(result) => result,
    };

    match result {
        Ok(value) => {
            let mut counts = success_counts(&value);
            counts.input_count = request_counts.input_count;
            trace.success(counts);
            Ok(value)
        }
        Err(source) => {
            let error = ApiError::from(source);
            trace.failure(error, request_counts);
            Err(error)
        }
    }
}

#[derive(Clone, Copy)]
struct ApiError {
    status: StatusCode,
    code: RemoteErrorCode,
}

impl ApiError {
    const fn new(status: StatusCode, code: RemoteErrorCode) -> Self {
        Self { status, code }
    }

    const fn bad_request() -> Self {
        Self::new(StatusCode::BAD_REQUEST, RemoteErrorCode::BadRequest)
    }

    const fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, RemoteErrorCode::Unauthorized)
    }

    const fn namespace_mismatch() -> Self {
        Self::new(StatusCode::FORBIDDEN, RemoteErrorCode::NamespaceMismatch)
    }

    const fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            RemoteErrorCode::Unavailable,
        )
    }

    const fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, RemoteErrorCode::Internal)
    }
}

impl From<MemoryError> for ApiError {
    fn from(error: MemoryError) -> Self {
        if error.is_retryable() {
            return Self::unavailable();
        }
        match error {
            MemoryError::EmptyContent => Self::bad_request(),
            MemoryError::ContentTooLarge { .. } => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                RemoteErrorCode::ContentTooLarge,
            ),
            MemoryError::QueryTooLarge { .. } => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                RemoteErrorCode::QueryTooLarge,
            ),
            MemoryError::RecordCapacity { .. } => Self::new(
                StatusCode::INSUFFICIENT_STORAGE,
                RemoteErrorCode::RecordCapacity,
            ),
            MemoryError::ContentCapacity { .. } | MemoryError::StorageCapacity => Self::new(
                StatusCode::INSUFFICIENT_STORAGE,
                RemoteErrorCode::ContentCapacity,
            ),
            MemoryError::SecretRejected => Self::bad_request(),
            MemoryError::Duplicate => Self::new(StatusCode::CONFLICT, RemoteErrorCode::Duplicate),
            MemoryError::NotFound => Self::new(StatusCode::NOT_FOUND, RemoteErrorCode::NotFound),
            MemoryError::Conflict => Self::new(StatusCode::CONFLICT, RemoteErrorCode::Conflict),
            MemoryError::RemoteReadOnly => {
                Self::new(StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden)
            }
            MemoryError::UnsupportedSchemaVersion { .. }
            | MemoryError::InvalidPagination
            | MemoryError::Backend { .. }
            | MemoryError::Unavailable { .. } => Self::internal(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response<Body> {
        (self.status, Json(ErrorResponse { code: self.code })).into_response()
    }
}
