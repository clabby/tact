use crate::{
    MemoryCandidate, MemoryError, MemoryKey, MemoryLimits, MemoryNamespaceFilter, MemoryRecord,
    MemoryStore, server::protocol,
};
use protocol::{
    DeleteRequest, ErrorResponse, ExportRequest, ExportResponse, ListResponse, PutRequest,
    PutResponse, ReadRequest, ReadResponse, RemoteErrorCode, RemoteRole, ScanRequest, ScanResponse,
    ScanScope, SessionResponse, SyncReport, SyncRequest,
};
use reqwest::{Client, Response, StatusCode, Url};
use serde::{Serialize, de::DeserializeOwned};
use std::{collections::HashSet, fmt, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::time::sleep;
use zeroize::{Zeroize, Zeroizing};

const ATTEMPTS: usize = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(750);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const RETRY_BACKOFFS: [Option<Duration>; ATTEMPTS] = [
    Some(Duration::from_millis(100)),
    Some(Duration::from_millis(250)),
    None,
];

/// Secret bearer token for a remote memory service.
pub struct RemoteToken(Zeroizing<String>);

impl RemoteToken {
    /// Wraps a non-empty token in zeroizing storage.
    pub fn new(token: String) -> Result<Self, RemoteClientError> {
        if token.trim().is_empty() {
            return Err(RemoteClientError::EmptyToken);
        }
        Ok(Self(Zeroizing::new(token)))
    }

    fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for RemoteToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteToken([REDACTED])")
    }
}

impl Drop for RemoteToken {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl Zeroize for RemoteToken {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

/// Failure while configuring or calling a remote memory service.
#[derive(Debug, Error)]
pub enum RemoteClientError {
    /// Endpoint is not an HTTP(S) URL without embedded credentials.
    #[error("remote memory endpoint is invalid")]
    InvalidEndpoint,
    /// Namespace violates the wire-format contract.
    #[error("remote memory namespace is invalid")]
    InvalidNamespace,
    /// Bearer token is empty.
    #[error("remote memory token is empty")]
    EmptyToken,
    /// Request failed before a valid response arrived.
    #[error("remote memory request could not reach the server")]
    Transport,
    /// Server rejected the bearer token.
    #[error("remote memory server rejected authentication")]
    Unauthorized,
    /// Credential cannot mutate its namespace.
    #[error("remote memory credential is read-only")]
    ReadOnly,
    /// Server returned a different authenticated namespace.
    #[error("remote memory namespace does not match the credential")]
    NamespaceMismatch,
    /// Server speaks a different protocol version.
    #[error("remote memory protocol is incompatible")]
    IncompatibleProtocol,
    /// Server rejected the request with a protocol error code.
    #[error("remote memory server rejected the operation: {code:?}")]
    Rejected {
        /// Stable server failure category.
        code: RemoteErrorCode,
    },
    /// Response violated bounds, ordering, or ownership constraints.
    #[error("remote memory server returned an invalid response")]
    InvalidResponse,
    /// Service remained unavailable after bounded retries.
    #[error("remote memory service is unavailable")]
    Unavailable,
}

/// Authenticated HTTP implementation of [`MemoryStore`].
#[derive(Clone)]
pub struct RemoteMemoryClient {
    inner: Arc<RemoteClientInner>,
}

impl fmt::Debug for RemoteMemoryClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteMemoryClient")
            .field("endpoint", &self.inner.endpoint)
            .field("namespace", &self.inner.namespace)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

struct RemoteClientInner {
    endpoint: Url,
    namespace: String,
    token: RemoteToken,
    client: Client,
    role: tokio::sync::OnceCell<RemoteRole>,
    bookmark: tokio::sync::Mutex<Option<String>>,
}

impl RemoteMemoryClient {
    /// Creates a client bound to one validated namespace.
    pub fn new(
        endpoint: &str,
        namespace: String,
        token: RemoteToken,
    ) -> Result<Self, RemoteClientError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut endpoint = Url::parse(endpoint).map_err(|_| RemoteClientError::InvalidEndpoint)?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
        {
            return Err(RemoteClientError::InvalidEndpoint);
        }
        if !protocol::is_valid_namespace(&namespace) {
            return Err(RemoteClientError::InvalidNamespace);
        }
        if !endpoint.path().ends_with('/') {
            let mut path = endpoint.path().to_owned();
            path.push('/');
            endpoint.set_path(&path);
        }
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| RemoteClientError::InvalidEndpoint)?;
        Ok(Self {
            inner: Arc::new(RemoteClientInner {
                endpoint,
                namespace,
                token,
                client,
                role: tokio::sync::OnceCell::new(),
                bookmark: tokio::sync::Mutex::new(None),
            }),
        })
    }

    /// Returns the configured namespace without network I/O.
    pub fn namespace(&self) -> &str {
        &self.inner.namespace
    }

    /// Negotiates and caches the authenticated remote role.
    pub async fn session(&self) -> Result<RemoteRole, RemoteClientError> {
        self.inner
            .role
            .get_or_try_init(|| async {
                let response: SessionResponse =
                    self.get(protocol::SESSION_PATH, Replay::Safe).await?;
                if response.protocol_version != crate::VERSION {
                    return Err(RemoteClientError::IncompatibleProtocol);
                }
                if response.namespace != self.inner.namespace {
                    return Err(RemoteClientError::NamespaceMismatch);
                }
                Ok(response.role)
            })
            .await
            .copied()
    }

    async fn scan(
        &self,
        query: &str,
        scope: ScanScope,
        limit: usize,
    ) -> Result<Vec<MemoryCandidate>, RemoteClientError> {
        let limit = limit.min(MemoryLimits::PRODUCTION.scan_results);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let response: ScanResponse = self
            .post(
                protocol::SCAN_PATH,
                &ScanRequest {
                    query: query.to_owned(),
                    scope,
                    limit,
                },
                Replay::ConnectOnly,
            )
            .await?;
        if response.candidates.len() > limit {
            return Err(RemoteClientError::InvalidResponse);
        }

        let mut seen = HashSet::new();
        let mut validated = Vec::new();
        let mut previous_score = None;
        for candidate in response.candidates {
            if !Self::valid_candidate(&candidate) {
                return Err(RemoteClientError::InvalidResponse);
            }
            let Some(namespace) = candidate.key.namespace.clone() else {
                return Err(RemoteClientError::InvalidResponse);
            };
            let owned = namespace == self.namespace();
            let in_scope = match scope {
                ScanScope::All => true,
                ScanScope::Own => owned,
                ScanScope::Others => !owned,
            };
            if !in_scope {
                return Err(RemoteClientError::InvalidResponse);
            }
            if previous_score.is_some_and(|score| candidate.score > score) {
                return Err(RemoteClientError::InvalidResponse);
            }
            previous_score = Some(candidate.score);
            if !seen.insert((namespace, candidate.key.id)) {
                return Err(RemoteClientError::InvalidResponse);
            }
            if crate::secrets::contains_likely_secret(&candidate.preview) {
                continue;
            }
            validated.push(candidate);
        }
        Ok(validated)
    }

    async fn read(
        &self,
        ids: &[i64],
        keys: &[MemoryKey],
    ) -> Result<Vec<MemoryRecord>, RemoteClientError> {
        let keys = keys
            .iter()
            .filter(|key| Self::valid_key(key) && key.namespace.is_some())
            .cloned()
            .collect::<Vec<_>>();
        let ids = ids.iter().copied().filter(|id| *id > 0).collect::<Vec<_>>();
        if keys.is_empty() && ids.is_empty() {
            return Ok(Vec::new());
        }
        let requested = keys.iter().cloned().collect::<HashSet<_>>();
        let requested_ids = ids.iter().copied().collect::<HashSet<_>>();
        let response: ReadResponse = self
            .post(
                protocol::READ_PATH,
                &ReadRequest { ids, keys },
                Replay::ConnectOnly,
            )
            .await?;
        let requested_records = requested
            .iter()
            .filter_map(|key| key.namespace.clone().map(|namespace| (namespace, key.id)))
            .chain(
                requested_ids
                    .iter()
                    .map(|id| (self.namespace().to_owned(), *id)),
            )
            .collect::<HashSet<_>>();
        if response.memories.len() > requested_records.len() {
            return Err(RemoteClientError::InvalidResponse);
        }

        let mut seen = HashSet::new();
        let mut memories = Vec::new();
        for memory in response.memories {
            if !(requested.contains(&memory.key)
                || (memory.key.namespace.as_deref() == Some(self.namespace())
                    && requested_ids.contains(&memory.key.id)))
                || !Self::valid_record(&memory)
            {
                continue;
            }
            let logical_key = (memory.key.namespace.clone().unwrap(), memory.key.id);
            if !seen.insert(logical_key) {
                return Err(RemoteClientError::InvalidResponse);
            }
            memories.push(memory);
        }
        Ok(memories)
    }

    async fn list(&self) -> Result<Vec<MemoryRecord>, RemoteClientError> {
        let response: ListResponse = self.post(protocol::LIST_PATH, &(), Replay::Safe).await?;
        if response.memories.len() > MemoryLimits::PRODUCTION.records {
            return Err(RemoteClientError::InvalidResponse);
        }
        let mut seen = HashSet::new();
        let mut memories = Vec::new();
        for memory in response.memories {
            if !Self::valid_record(&memory) {
                continue;
            }
            let Some(namespace) = memory.key.namespace.clone() else {
                continue;
            };
            let logical_key = (namespace, memory.key.id);
            if !seen.insert(logical_key) {
                return Err(RemoteClientError::InvalidResponse);
            }
            memories.push(memory);
        }
        Ok(memories)
    }

    async fn put(
        &self,
        content: &str,
        replacement: Option<&MemoryKey>,
    ) -> Result<MemoryRecord, RemoteClientError> {
        let response: PutResponse = self
            .post(
                protocol::PUT_PATH,
                &PutRequest {
                    content: content.to_owned(),
                    replacement: replacement.cloned(),
                },
                Replay::ConnectOnly,
            )
            .await?;
        if !Self::valid_record(&response.memory)
            || response.memory.key.namespace.as_deref() != Some(self.namespace())
            || response.memory.content != content
            || match replacement {
                Some(replacement) => {
                    response.memory.key.id != replacement.id
                        || replacement
                            .version
                            .checked_add(1)
                            .is_none_or(|version| response.memory.key.version != version)
                }
                None => response.memory.key.version != 1,
            }
        {
            return Err(RemoteClientError::InvalidResponse);
        }
        Ok(response.memory)
    }

    async fn delete(&self, key: &MemoryKey) -> Result<(), RemoteClientError> {
        if key.namespace.as_deref() != Some(self.namespace()) {
            return Err(RemoteClientError::NamespaceMismatch);
        }
        self.post::<_, serde_json::Value>(
            protocol::DELETE_PATH,
            &DeleteRequest { key: key.clone() },
            Replay::Safe,
        )
        .await?;
        Ok(())
    }

    async fn sync(&self, memories: &[MemoryRecord]) -> Result<SyncReport, RemoteClientError> {
        let report: SyncReport = self
            .post(
                protocol::SYNC_PATH,
                &SyncRequest {
                    memories: memories.to_vec(),
                },
                Replay::Safe,
            )
            .await?;
        let applied = report
            .inserted
            .checked_add(report.replaced)
            .and_then(|count| count.checked_add(report.unchanged));
        if applied != Some(memories.len()) || report.deleted > MemoryLimits::PRODUCTION.records {
            return Err(RemoteClientError::InvalidResponse);
        }
        Ok(report)
    }

    fn validate_export_page(
        namespaces: Option<&[String]>,
        cursor: Option<&protocol::ExportCursor>,
        accumulated_records: usize,
        accumulated_content_bytes: usize,
        response: &ExportResponse,
    ) -> Result<usize, RemoteClientError> {
        if response.memories.len() > protocol::MAX_EXPORT_PAGE_RECORDS
            || accumulated_records
                .checked_add(response.memories.len())
                .is_none_or(|count| count > MemoryLimits::PRODUCTION.records)
        {
            return Err(RemoteClientError::InvalidResponse);
        }

        let mut previous = cursor.cloned();
        let mut page_content_bytes = 0usize;
        for memory in &response.memories {
            let Some(namespace) = memory.key.namespace.as_deref() else {
                return Err(RemoteClientError::InvalidResponse);
            };
            let selected = namespaces
                .is_none_or(|selected| selected.iter().any(|candidate| candidate == namespace));
            let ordered = previous.as_ref().is_none_or(|previous| {
                (namespace, memory.key.id) > (previous.namespace.as_str(), previous.id)
            });
            if !Self::valid_record_structure(memory) || !selected || !ordered {
                return Err(RemoteClientError::InvalidResponse);
            }

            page_content_bytes = page_content_bytes
                .checked_add(memory.content.len())
                .ok_or(RemoteClientError::InvalidResponse)?;
            if accumulated_content_bytes
                .checked_add(page_content_bytes)
                .is_none_or(|bytes| bytes > MemoryLimits::PRODUCTION.total_content_bytes)
            {
                return Err(RemoteClientError::InvalidResponse);
            }
            previous = Some(protocol::ExportCursor {
                namespace: namespace.to_owned(),
                id: memory.key.id,
            });
        }

        if let Some(next_cursor) = &response.next_cursor {
            let exact_last_key = response.memories.last().is_some_and(|memory| {
                memory.key.namespace.as_deref() == Some(next_cursor.namespace.as_str())
                    && memory.key.id == next_cursor.id
            });
            if !exact_last_key {
                return Err(RemoteClientError::InvalidResponse);
            }
        }
        Ok(page_content_bytes)
    }

    fn valid_key(key: &MemoryKey) -> bool {
        key.namespace
            .as_deref()
            .is_none_or(protocol::is_valid_namespace)
            && key.id > 0
            && key.version > 0
    }

    fn valid_candidate(candidate: &MemoryCandidate) -> bool {
        Self::valid_key(&candidate.key)
            && candidate.key.namespace.is_some()
            && candidate.preview.len() <= 64
            && candidate.score.is_finite()
            && candidate.score >= 0.0
    }

    fn valid_record(memory: &MemoryRecord) -> bool {
        Self::valid_record_structure(memory)
            && !crate::secrets::contains_likely_secret(&memory.content)
    }

    fn valid_record_structure(memory: &MemoryRecord) -> bool {
        Self::valid_key(&memory.key)
            && memory.key.namespace.is_some()
            && !memory.content.trim().is_empty()
            && memory.content.len() <= MemoryLimits::PRODUCTION.content_bytes
            && memory.created_at_ms >= 0
            && memory.updated_at_ms >= memory.created_at_ms
    }

    async fn get<Response>(&self, path: &str, replay: Replay) -> Result<Response, RemoteClientError>
    where
        Response: DeserializeOwned,
    {
        self.send(path, None::<&()>, replay).await
    }

    async fn post<Request, Response>(
        &self,
        path: &str,
        body: &Request,
        replay: Replay,
    ) -> Result<Response, RemoteClientError>
    where
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        self.send(path, Some(body), replay).await
    }

    async fn send<Request, Response>(
        &self,
        path: &str,
        body: Option<&Request>,
        replay: Replay,
    ) -> Result<Response, RemoteClientError>
    where
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        // Bookmarks are opaque, so concurrent responses cannot be merged safely. Holding this guard
        // across the exchange makes this client and its clones one monotonic session.
        let mut bookmark = self.inner.bookmark.lock().await;
        let url = self
            .inner
            .endpoint
            .join(path)
            .map_err(|_| RemoteClientError::InvalidEndpoint)?;
        for backoff in RETRY_BACKOFFS {
            let request = match body {
                Some(body) => self.inner.client.post(url.clone()).json(body),
                None => self.inner.client.get(url.clone()),
            }
            // Reqwest owns a transient, non-zeroizing header copy for the request lifetime.
            .bearer_auth(self.inner.token.expose())
            .header(protocol::NAMESPACE_HEADER, &self.inner.namespace);
            let request = match bookmark.as_deref() {
                Some(bookmark) => request.header(protocol::BOOKMARK_HEADER, bookmark),
                None => request,
            };

            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    let response_bookmark = bookmark_from_headers(response.headers())?;
                    let decoded = decode_response(response).await?;
                    if let Some(response_bookmark) = response_bookmark {
                        *bookmark = Some(response_bookmark);
                    }
                    return Ok(decoded);
                }
                Ok(response) if replay == Replay::Safe && retryable_status(response.status()) => {
                    let Some(backoff) = backoff else {
                        return Err(response_error(response).await);
                    };
                    sleep(response_retry_delay(response.headers(), backoff)).await;
                }
                Ok(response) => return Err(response_error(response).await),
                Err(error) if error.is_connect() => {
                    let Some(backoff) = backoff else {
                        return Err(RemoteClientError::Transport);
                    };
                    sleep(backoff).await;
                }
                Err(_) => return Err(RemoteClientError::Transport),
            }
        }
        Err(RemoteClientError::Unavailable)
    }
}

impl MemoryStore for RemoteMemoryClient {
    fn scan(
        &self,
        query: &str,
        namespaces: MemoryNamespaceFilter,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<MemoryCandidate>, MemoryError>> + Send {
        async move {
            let scope = match namespaces {
                MemoryNamespaceFilter::All => ScanScope::All,
                MemoryNamespaceFilter::Exact(namespace) if namespace == self.namespace() => {
                    ScanScope::Own
                }
                MemoryNamespaceFilter::OtherThan(namespace) if namespace == self.namespace() => {
                    ScanScope::Others
                }
                MemoryNamespaceFilter::Exact(_) | MemoryNamespaceFilter::OtherThan(_) => {
                    return Err(RemoteClientError::NamespaceMismatch.into());
                }
            };
            Ok(RemoteMemoryClient::scan(self, query, scope, limit).await?)
        }
    }
    fn read(
        &self,
        ids: &[i64],
        keys: &[MemoryKey],
    ) -> impl std::future::Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        async move { Ok(RemoteMemoryClient::read(self, ids, keys).await?) }
    }
    fn list(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        async move { Ok(RemoteMemoryClient::list(self).await?) }
    }
    fn put(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
    ) -> impl std::future::Future<Output = Result<MemoryRecord, MemoryError>> + Send {
        async move {
            if content.trim().is_empty() {
                return Err(MemoryError::EmptyContent);
            }
            Ok(RemoteMemoryClient::put(self, content, replacement.as_ref()).await?)
        }
    }
    fn delete(
        &self,
        key: MemoryKey,
    ) -> impl std::future::Future<Output = Result<(), MemoryError>> + Send {
        async move { Ok(RemoteMemoryClient::delete(self, &key).await?) }
    }
    fn sync(
        &self,
        memories: &[MemoryRecord],
    ) -> impl std::future::Future<Output = Result<SyncReport, MemoryError>> + Send {
        async move { Ok(RemoteMemoryClient::sync(self, memories).await?) }
    }
    fn export_page(
        &self,
        namespaces: Option<&[String]>,
        cursor: Option<&protocol::ExportCursor>,
        limit: usize,
    ) -> impl std::future::Future<
        Output = Result<(Vec<MemoryRecord>, Option<protocol::ExportCursor>), MemoryError>,
    > + Send {
        let namespaces = namespaces.map(<[String]>::to_vec);
        let cursor = cursor.cloned();
        async move {
            let limit = limit.clamp(1, protocol::MAX_EXPORT_PAGE_RECORDS);
            let response: ExportResponse = self
                .post(
                    protocol::EXPORT_PATH,
                    &ExportRequest {
                        namespaces: namespaces.clone(),
                        cursor: cursor.clone(),
                        limit,
                    },
                    Replay::Safe,
                )
                .await?;
            if response.memories.len() > limit {
                return Err(RemoteClientError::InvalidResponse.into());
            }
            Self::validate_export_page(namespaces.as_deref(), cursor.as_ref(), 0, 0, &response)?;
            let memories = response
                .memories
                .into_iter()
                .filter(Self::valid_record)
                .collect();
            Ok((memories, response.next_cursor))
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Replay {
    Safe,
    ConnectOnly,
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn response_retry_delay(headers: &reqwest::header::HeaderMap, fallback: Duration) -> Duration {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.min(2)))
        .unwrap_or(fallback)
}

fn bookmark_from_headers(
    headers: &reqwest::header::HeaderMap,
) -> Result<Option<String>, RemoteClientError> {
    let mut values = headers.get_all(protocol::BOOKMARK_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(RemoteClientError::InvalidResponse);
    }

    let bookmark = value
        .to_str()
        .map_err(|_| RemoteClientError::InvalidResponse)?;
    if bookmark.is_empty() {
        return Ok(None);
    }
    if !protocol::is_valid_bookmark(bookmark) {
        return Err(RemoteClientError::InvalidResponse);
    }
    Ok(Some(bookmark.to_owned()))
}

async fn response_error(response: Response) -> RemoteClientError {
    let status = response.status();
    let code = decode_response::<ErrorResponse>(response)
        .await
        .ok()
        .map(|response| response.code);
    match (status, code) {
        (StatusCode::UNAUTHORIZED, _) | (_, Some(RemoteErrorCode::Unauthorized)) => {
            RemoteClientError::Unauthorized
        }
        (StatusCode::FORBIDDEN, Some(RemoteErrorCode::NamespaceMismatch)) => {
            RemoteClientError::NamespaceMismatch
        }
        (StatusCode::FORBIDDEN, _) | (_, Some(RemoteErrorCode::Forbidden)) => {
            RemoteClientError::ReadOnly
        }
        (_, Some(RemoteErrorCode::UnsupportedProtocol)) => RemoteClientError::IncompatibleProtocol,
        (_, Some(code)) => RemoteClientError::Rejected { code },
        (StatusCode::NOT_FOUND, None) => RemoteClientError::IncompatibleProtocol,
        (_, None) if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS => {
            RemoteClientError::Unavailable
        }
        _ => RemoteClientError::InvalidResponse,
    }
}

async fn decode_response<Decoded>(mut response: Response) -> Result<Decoded, RemoteClientError>
where
    Decoded: DeserializeOwned,
{
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| RemoteClientError::InvalidResponse)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(RemoteClientError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| RemoteClientError::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ExportCursor;

    fn memory(namespace: &str, id: i64, content: &str) -> MemoryRecord {
        MemoryRecord {
            key: MemoryKey::remote(namespace.to_owned(), id, 1),
            content: content.to_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
            last_scanned_at_ms: None,
            scan_count: 0,
            last_used_at_ms: None,
            use_count: 0,
            probation_until_ms: None,
        }
    }

    fn response(memories: Vec<MemoryRecord>, next: Option<(&str, i64)>) -> ExportResponse {
        ExportResponse {
            memories,
            next_cursor: next.map(|(namespace, id)| ExportCursor {
                namespace: namespace.to_owned(),
                id,
            }),
        }
    }

    fn invalid(result: Result<usize, RemoteClientError>) -> bool {
        matches!(result, Err(RemoteClientError::InvalidResponse))
    }

    #[tokio::test]
    async fn delete_rejects_a_key_without_the_authenticated_namespace() {
        let token = RemoteToken::new("test-token".to_owned()).unwrap();
        let client =
            RemoteMemoryClient::new("http://127.0.0.1:1/", "alice".to_owned(), token).unwrap();

        assert!(matches!(
            client.delete(&MemoryKey::local(1, 1)).await,
            Err(RemoteClientError::NamespaceMismatch)
        ));
    }

    #[test]
    fn client_failures_preserve_memory_error_semantics() {
        assert!(MemoryError::from(RemoteClientError::Transport).is_retryable());
        assert!(MemoryError::from(RemoteClientError::Unavailable).is_retryable());
        assert!(
            MemoryError::from(RemoteClientError::Rejected {
                code: RemoteErrorCode::Unavailable,
            })
            .is_retryable()
        );
        assert!(!MemoryError::from(RemoteClientError::InvalidResponse).is_retryable());
        assert!(matches!(
            MemoryError::from(RemoteClientError::Rejected {
                code: RemoteErrorCode::Conflict,
            }),
            MemoryError::Conflict
        ));
    }

    #[test]
    fn export_page_enforces_namespace_and_exact_cursor() {
        let selected = ["alpha".to_owned()];

        assert_eq!(
            RemoteMemoryClient::validate_export_page(
                Some(&selected),
                None,
                0,
                0,
                &response(vec![memory("alpha", 1, "one")], Some(("alpha", 1))),
            )
            .expect("valid page"),
            3
        );
        assert!(invalid(RemoteMemoryClient::validate_export_page(
            Some(&selected),
            None,
            0,
            0,
            &response(vec![memory("beta", 1, "one")], None),
        )));
        assert!(invalid(RemoteMemoryClient::validate_export_page(
            Some(&selected),
            None,
            0,
            0,
            &response(vec![memory("alpha", 1, "one")], Some(("alpha", 2))),
        )));
    }

    #[test]
    fn export_page_validates_cursor_before_suppressing_secret_like_content() {
        let page = response(
            vec![memory("alpha", 1, "password=hunter2")],
            Some(("alpha", 1)),
        );

        assert!(RemoteMemoryClient::validate_export_page(None, None, 0, 0, &page).is_ok());
        assert!(!RemoteMemoryClient::valid_record(&page.memories[0]));
    }

    #[test]
    fn export_page_rejects_duplicate_and_out_of_order_keys() {
        assert!(invalid(RemoteMemoryClient::validate_export_page(
            None,
            None,
            0,
            0,
            &response(
                vec![memory("alpha", 1, "one"), memory("alpha", 1, "two")],
                None,
            ),
        )));
        assert!(invalid(RemoteMemoryClient::validate_export_page(
            None,
            Some(&ExportCursor {
                namespace: "beta".to_owned(),
                id: 2,
            }),
            2,
            6,
            &response(vec![memory("alpha", 1, "one")], Some(("alpha", 1))),
        )));
    }

    #[test]
    fn export_page_rejects_multi_cursor_cycles() {
        let first = response(vec![memory("alpha", 1, "one")], Some(("alpha", 1)));
        let second = response(vec![memory("beta", 1, "two")], Some(("beta", 1)));
        let cycle = response(vec![memory("alpha", 1, "one")], Some(("alpha", 1)));

        assert_eq!(
            RemoteMemoryClient::validate_export_page(None, None, 0, 0, &first)
                .expect("valid first page"),
            3
        );
        assert_eq!(
            RemoteMemoryClient::validate_export_page(
                None,
                first.next_cursor.as_ref(),
                1,
                3,
                &second,
            )
            .expect("valid second page"),
            3
        );
        assert!(invalid(RemoteMemoryClient::validate_export_page(
            None,
            second.next_cursor.as_ref(),
            2,
            6,
            &cycle,
        )));
    }

    #[test]
    fn export_page_rejects_aggregate_limits_before_accumulation() {
        let page = response(vec![memory("alpha", 1, "x")], None);

        assert!(invalid(RemoteMemoryClient::validate_export_page(
            None,
            None,
            MemoryLimits::PRODUCTION.records,
            0,
            &page,
        )));
        assert!(invalid(RemoteMemoryClient::validate_export_page(
            None,
            None,
            0,
            MemoryLimits::PRODUCTION.total_content_bytes,
            &page,
        )));
    }
}
