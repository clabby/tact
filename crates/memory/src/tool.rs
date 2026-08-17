//! Explicit agent access to the global memory store.

use super::{
    MemoryAccess, MemoryCandidate, MemoryKey, MemoryRecord, MemoryStore, SelectedMemoryStore,
    server::protocol::ExportCursor,
};
use nanocodex::{
    Tool,
    tools::contract::{
        ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, async_trait,
    },
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::{
    future::Future,
    io,
    sync::atomic::{AtomicBool, Ordering},
};
use zeroize::Zeroizing;

const DEFAULT_SCAN_LIMIT: usize = 5;
const INSPECTION_PAGE_RECORDS: usize = 20;
const INSPECTION_PAGES_PER_CALL: usize = 4;

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum MemoryOperation {
    Scan {
        query: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    Read {
        keys: Vec<MemoryKey>,
    },
    Inspect {
        #[serde(default)]
        cursor: Option<InspectionCursor>,
    },
    Put {
        content: MemoryContent,
        #[serde(default)]
        replace: Option<MemoryKey>,
    },
    Delete {
        key: MemoryKey,
    },
}

/// Zeroizes Tact's typed copy even when object deserialization later rejects the call.
///
/// Nanocodex owns the raw tool arguments and conversation records outside this wrapper; those
/// dependency-owned copies do not provide a zeroization guarantee.
struct MemoryContent(Zeroizing<String>);

impl<'de> Deserialize<'de> for MemoryContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)
            .map(Zeroizing::new)
            .map(Self)
    }
}

#[derive(Serialize)]
struct ScanOutput {
    operation: &'static str,
    backend: MemoryAccess,
    ours: Vec<ToolCandidate>,
    theirs: Vec<ToolCandidate>,
}

#[derive(Serialize)]
struct ToolCandidate {
    key: MemoryKey,
    preview: String,
    score: f64,
}

impl From<MemoryCandidate> for ToolCandidate {
    fn from(candidate: MemoryCandidate) -> Self {
        Self {
            key: candidate.key,
            preview: candidate.preview,
            score: candidate.score,
        }
    }
}

#[derive(Serialize)]
struct ReadOutput {
    operation: &'static str,
    backend: MemoryAccess,
    memories: Vec<MemoryRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct InspectionCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    id: i64,
}

impl InspectionCursor {
    fn into_export(self) -> ExportCursor {
        ExportCursor {
            namespace: self.namespace.unwrap_or_default(),
            id: self.id,
        }
    }

    fn from_export(cursor: ExportCursor) -> Self {
        Self {
            namespace: (!cursor.namespace.is_empty()).then_some(cursor.namespace),
            id: cursor.id,
        }
    }
}

#[derive(Serialize)]
struct InspectOutput {
    operation: &'static str,
    backend: MemoryAccess,
    ours: Vec<InspectionRecord>,
    theirs: Vec<InspectionRecord>,
    next_cursor: Option<InspectionCursor>,
    coverage: InspectionCoverage,
}

#[derive(Serialize)]
struct InspectionCoverage {
    records: usize,
    complete: bool,
    consistency: &'static str,
}

#[derive(Serialize)]
struct InspectionRecord {
    key: MemoryKey,
    preview: String,
    content_bytes: usize,
    created_at_ms: i64,
    updated_at_ms: i64,
    last_scanned_at_ms: Option<i64>,
    scan_count: u64,
    last_used_at_ms: Option<i64>,
    use_count: u64,
    probation_until_ms: Option<i64>,
}

impl From<MemoryRecord> for InspectionRecord {
    fn from(memory: MemoryRecord) -> Self {
        Self {
            key: memory.key,
            preview: crate::retrieval::preview(&memory.content),
            content_bytes: memory.content.len(),
            created_at_ms: memory.created_at_ms,
            updated_at_ms: memory.updated_at_ms,
            last_scanned_at_ms: memory.last_scanned_at_ms,
            scan_count: memory.scan_count,
            last_used_at_ms: memory.last_used_at_ms,
            use_count: memory.use_count,
            probation_until_ms: memory.probation_until_ms,
        }
    }
}

#[derive(Serialize)]
struct PutOutput {
    operation: &'static str,
    backend: MemoryAccess,
    memory: MemoryRecord,
    replaced: bool,
}

#[derive(Serialize)]
struct DeleteOutput {
    operation: &'static str,
    backend: MemoryAccess,
    key: MemoryKey,
}

/// Authorizes mutations for one agent session.
#[async_trait]
pub trait MutationAuthorizer: Send + Sync {
    /// Returns success only when `session_id` may mutate memory.
    async fn authorize_memory_mutation(&self, session_id: &str) -> io::Result<()>;
}

/// Nanocodex tool exposing bounded memory operations.
pub struct MemoryTool<A> {
    store: SelectedMemoryStore,
    authorizer: A,
    searched: AtomicBool,
    inspection_cursor: tokio::sync::Mutex<Option<InspectionCursor>>,
}

impl<A> MemoryTool<A>
where
    A: MutationAuthorizer,
{
    /// Creates a tool over `store` with the supplied mutation authorizer.
    pub const fn new(store: SelectedMemoryStore, authorizer: A) -> Self {
        Self {
            store,
            authorizer,
            searched: AtomicBool::new(false),
            inspection_cursor: tokio::sync::Mutex::const_new(None),
        }
    }

    async fn scan(&self, query: String, limit: Option<usize>) -> ToolResult {
        if query.trim().is_empty() {
            return Err(io::Error::other("memory scan query is empty").into());
        }
        let limit = limit.unwrap_or(DEFAULT_SCAN_LIMIT);
        if !(1..=DEFAULT_SCAN_LIMIT).contains(&limit) {
            return Err(io::Error::other("memory scan limit must be between 1 and 5").into());
        }
        let backend = self.store.access().await?;
        let (ours, theirs) = self.store.scan_groups(&query, limit).await?;
        self.searched.store(true, Ordering::Release);
        json_output(&ScanOutput {
            operation: "scan",
            backend,
            ours: ours.into_iter().map(ToolCandidate::from).collect(),
            theirs: theirs.into_iter().map(ToolCandidate::from).collect(),
        })
    }

    async fn read(&self, keys: Vec<MemoryKey>) -> ToolResult {
        if keys.is_empty() {
            return Err(io::Error::other("memory read requires at least one key").into());
        }
        let backend = self.store.access().await?;
        let memories = self.store.read(&[], &keys).await?;
        json_output(&ReadOutput {
            operation: "read",
            backend,
            memories,
        })
    }

    async fn inspect(&self, cursor: Option<InspectionCursor>) -> ToolResult {
        if cursor.as_ref().is_some_and(|cursor| cursor.id <= 0) {
            return Err(io::Error::other("memory inspection cursor is invalid").into());
        }
        // Hold the guard through retrieval to serialize page transitions, but do not change the
        // issued cursor until success so an error or cancelled future leaves the page retryable.
        let mut issued_cursor = self.inspection_cursor.lock().await;
        if cursor
            .as_ref()
            .is_some_and(|cursor| issued_cursor.as_ref() != Some(cursor))
        {
            return Err(io::Error::other(
                "memory inspection cursor was not issued by the preceding page",
            )
            .into());
        }
        let backend = self.store.access().await?;
        let local = backend.namespace.is_none();
        if cursor.as_ref().is_some_and(|cursor| {
            local != cursor.namespace.is_none()
                || cursor.namespace.as_deref().is_some_and(str::is_empty)
        }) {
            return Err(
                io::Error::other("memory inspection cursor does not match the backend").into(),
            );
        }
        let export_cursor = cursor.clone().map(InspectionCursor::into_export);
        let (memories, next_cursor) = advance_inspection_page(export_cursor, |cursor| async move {
            self.store
                .export_page(None, cursor.as_ref(), INSPECTION_PAGE_RECORDS)
                .await
        })
        .await?;

        let namespace = backend.namespace.as_deref();
        let records = memories.len();
        let mut ours = Vec::new();
        let mut theirs = Vec::new();
        for memory in memories {
            if crate::secrets::contains_likely_secret(&memory.content) {
                continue;
            }
            let owned = namespace.is_none() || memory.key.namespace.as_deref() == namespace;
            if owned {
                ours.push(memory.into());
            } else {
                theirs.push(memory.into());
            }
        }
        let complete = next_cursor.is_none();
        let next_cursor = next_cursor.map(InspectionCursor::from_export);
        *issued_cursor = next_cursor.clone();
        json_output(&InspectOutput {
            operation: "inspect",
            backend,
            ours,
            theirs,
            next_cursor,
            coverage: InspectionCoverage {
                records,
                complete,
                consistency: "best_effort",
            },
        })
    }

    async fn put(
        &self,
        session_id: &str,
        content: MemoryContent,
        replace: Option<MemoryKey>,
    ) -> ToolResult {
        let content = content.0;
        self.authorizer
            .authorize_memory_mutation(session_id)
            .await?;
        if !self.searched.swap(false, Ordering::AcqRel) {
            return Err(io::Error::other("scan memory before storing a conclusion").into());
        }
        let backend = self.store.access().await?;
        let replaced = replace.is_some();
        let memory = self.store.put(content.as_str(), replace).await?;
        json_output(&PutOutput {
            operation: "put",
            backend,
            memory,
            replaced,
        })
    }

    async fn delete(&self, session_id: &str, key: MemoryKey) -> ToolResult {
        self.authorizer
            .authorize_memory_mutation(session_id)
            .await?;
        let backend = self.store.access().await?;
        self.store.delete(key.clone()).await?;
        json_output(&DeleteOutput {
            operation: "delete",
            backend,
            key,
        })
    }
}

async fn advance_inspection_page<F, Fut>(
    mut cursor: Option<ExportCursor>,
    mut page: F,
) -> Result<(Vec<MemoryRecord>, Option<ExportCursor>), super::MemoryError>
where
    F: FnMut(Option<ExportCursor>) -> Fut,
    Fut: Future<Output = Result<(Vec<MemoryRecord>, Option<ExportCursor>), super::MemoryError>>,
{
    for _ in 0..INSPECTION_PAGES_PER_CALL {
        let (memories, next_cursor) = page(cursor.clone()).await?;
        if memories.len() > INSPECTION_PAGE_RECORDS
            || next_cursor
                .as_ref()
                .is_some_and(|next| !inspection_cursor_advances(cursor.as_ref(), next))
        {
            return Err(super::MemoryError::InvalidPagination);
        }
        if !memories.is_empty() || next_cursor.is_none() {
            return Ok((memories, next_cursor));
        }
        cursor = next_cursor;
    }

    Ok((Vec::new(), cursor))
}

fn inspection_cursor_advances(current: Option<&ExportCursor>, next: &ExportCursor) -> bool {
    next.id > 0
        && current.is_none_or(|current| {
            (next.namespace.as_str(), next.id) > (current.namespace.as_str(), current.id)
        })
}

#[async_trait]
impl<A> Tool for MemoryTool<A>
where
    A: MutationAuthorizer + 'static,
{
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "memory",
            "Explicitly searches, reads, inspects, stores, replaces, or deletes bounded memories in exactly one configured local or remote backend. Use inspect only for an explicitly requested corpus audit; it returns one telemetry-neutral, best-effort page and no snapshot guarantee. Continue only with the exact next_cursor issued by the preceding inspect call, or omit it to restart, and read selected exact keys for full content. A remote scan or inspection separates the authenticated namespace (ours) from other visible namespaces (theirs). Compare scan scores only within a group. Pass keys unchanged, for example {\"operation\":\"read\",\"keys\":[{\"id\":7,\"version\":1,\"namespace\":\"alice\"}]}. Put and delete are root-agent-only; remote mutation also requires a writer credential.",
            memory_input_schema(),
        )
        .with_output_schema(memory_output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        match input.decode_json::<MemoryOperation>()? {
            MemoryOperation::Scan { query, limit } => self.scan(query, limit).await,
            MemoryOperation::Read { keys } => self.read(keys).await,
            MemoryOperation::Inspect { cursor } => self.inspect(cursor).await,
            MemoryOperation::Put { content, replace } => {
                self.put(context.session_id(), content, replace).await
            }
            MemoryOperation::Delete { key } => self.delete(context.session_id(), key).await,
        }
    }
}

fn json_output(value: &impl Serialize) -> ToolResult {
    Ok(ToolOutput::from_json(serde_json::to_value(value)?, true))
}

fn memory_input_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "scan" },
                    "query": { "type": "string", "minLength": 1, "maxLength": 512 },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 5,
                        "default": 5,
                        "description": "Maximum candidates returned by each of the ours and theirs scans."
                    }
                },
                "required": ["operation", "query"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "read" },
                    "keys": {
                        "type": "array",
                        "items": memory_key_schema(),
                        "minItems": 1,
                        "description": "Exact candidate keys returned by scan. Preserve each id, version, and namespace unchanged."
                    }
                },
                "required": ["operation", "keys"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "inspect" },
                    "cursor": inspection_cursor_schema()
                },
                "required": ["operation"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "put" },
                    "content": { "type": "string", "minLength": 1, "maxLength": 1024 },
                    "replace": memory_key_schema()
                },
                "required": ["operation", "content"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "delete" },
                    "key": memory_key_schema()
                },
                "required": ["operation", "key"],
                "additionalProperties": false
            }
        ]
    })
}

fn memory_output_schema() -> Value {
    let record = memory_record_schema();
    let backend = memory_backend_schema();
    let candidates = memory_candidates_schema();
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "scan" },
                    "backend": backend.clone(),
                    "ours": candidates.clone(),
                    "theirs": candidates
                },
                "required": ["operation", "backend", "ours", "theirs"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "read" },
                    "backend": backend.clone(),
                    "memories": { "type": "array", "items": record.clone() }
                },
                "required": ["operation", "backend", "memories"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "inspect" },
                    "backend": backend.clone(),
                    "ours": inspection_records_schema(),
                    "theirs": inspection_records_schema(),
                    "next_cursor": {
                        "oneOf": [inspection_cursor_schema(), { "type": "null" }]
                    },
                    "coverage": {
                        "type": "object",
                        "properties": {
                            "records": { "type": "integer", "minimum": 0, "maximum": 20 },
                            "complete": { "type": "boolean" },
                            "consistency": { "type": "string", "const": "best_effort" }
                        },
                        "required": ["records", "complete", "consistency"],
                        "additionalProperties": false
                    }
                },
                "required": ["operation", "backend", "ours", "theirs", "next_cursor", "coverage"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "put" },
                    "backend": backend.clone(),
                    "memory": record,
                    "replaced": { "type": "boolean" }
                },
                "required": ["operation", "backend", "memory", "replaced"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "delete" },
                    "backend": backend,
                    "key": memory_key_schema()
                },
                "required": ["operation", "backend", "key"],
                "additionalProperties": false
            }
        ]
    })
}

fn inspection_cursor_schema() -> Value {
    json!({
        "type": "object",
        "description": "Exact continuation cursor issued by the immediately preceding inspect call on this tool. Preserve every field unchanged. Omit cursor to start or restart an audit.",
        "properties": {
            "namespace": { "type": "string", "minLength": 1 },
            "id": { "type": "integer", "minimum": 1 }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

fn inspection_records_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": 20,
        "items": {
            "type": "object",
            "properties": {
                "key": memory_key_schema(),
                "preview": { "type": "string", "maxLength": 64 },
                "content_bytes": { "type": "integer", "minimum": 1, "maximum": 1024 },
                "created_at_ms": { "type": "integer" },
                "updated_at_ms": { "type": "integer" },
                "last_scanned_at_ms": { "type": ["integer", "null"] },
                "scan_count": { "type": "integer", "minimum": 0 },
                "last_used_at_ms": { "type": ["integer", "null"] },
                "use_count": { "type": "integer", "minimum": 0 },
                "probation_until_ms": { "type": ["integer", "null"] }
            },
            "required": [
                "key", "preview", "content_bytes", "created_at_ms", "updated_at_ms",
                "last_scanned_at_ms", "scan_count", "last_used_at_ms", "use_count",
                "probation_until_ms"
            ],
            "additionalProperties": false
        }
    })
}

fn memory_candidates_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": 5,
        "items": {
            "type": "object",
            "properties": {
                "key": memory_key_schema(),
                "preview": { "type": "string", "maxLength": 64 },
                "score": { "type": "number" }
            },
            "required": ["key", "preview", "score"],
            "additionalProperties": false
        }
    })
}

fn memory_backend_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "enum": ["local", "remote"] },
            "namespace": { "type": ["string", "null"] },
            "role": { "type": ["string", "null"], "enum": ["reader", "writer", null] }
        },
        "required": ["source", "namespace", "role"],
        "additionalProperties": false
    })
}

fn memory_key_schema() -> Value {
    json!({
        "type": "object",
        "description": "An exact memory key returned by scan, read, list, or put. Preserve every field unchanged.",
        "properties": {
            "id": { "type": "integer", "minimum": 1 },
            "version": { "type": "integer", "minimum": 1 }
            ,"namespace": { "type": "string", "minLength": 1 }
        },
        "required": ["id", "version"],
        "additionalProperties": false
    })
}

fn memory_record_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "key": memory_key_schema(),
            "content": { "type": "string" },
            "created_at_ms": { "type": "integer" },
            "updated_at_ms": { "type": "integer" },
            "last_scanned_at_ms": { "type": ["integer", "null"] },
            "scan_count": { "type": "integer", "minimum": 0 },
            "last_used_at_ms": { "type": ["integer", "null"] },
            "use_count": { "type": "integer", "minimum": 0 },
            "probation_until_ms": { "type": ["integer", "null"] }
        },
        "required": [
            "key", "content", "created_at_ms", "updated_at_ms", "last_scanned_at_ms",
            "scan_count", "last_used_at_ms", "use_count", "probation_until_ms"
        ],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::{MemoryOperation, MemoryTool, MutationAuthorizer, advance_inspection_page};
    use crate::{
        MemoryError, MemoryKey, MemoryRecord, MemoryStore, SelectedMemoryStore,
        server::protocol::ExportCursor,
    };
    use nanocodex::{
        Tool,
        tools::contract::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolContext, ToolInput, async_trait},
    };
    use serde_json::{Value, json, value::to_raw_value};
    use std::{cell::Cell, collections::VecDeque, fs, future::ready, io};
    use tempfile::tempdir;

    struct TestAuthorizer;

    #[async_trait]
    impl MutationAuthorizer for TestAuthorizer {
        async fn authorize_memory_mutation(&self, session_id: &str) -> io::Result<()> {
            if session_id == "root" {
                return Ok(());
            }
            Err(io::Error::other(
                "memory mutation is only available to root agents",
            ))
        }
    }

    fn input(value: serde_json::Value) -> ToolInput {
        ToolInput::Function(to_raw_value(&value).unwrap())
    }

    fn context(session_id: &str) -> ToolContext<'_> {
        ToolContext::new(
            "test-model",
            session_id,
            "test-call",
            &[],
            DEFAULT_TOOL_OUTPUT_TOKENS,
        )
    }

    #[test]
    fn definition_has_one_closed_tagged_surface() {
        let tool = MemoryTool::new(
            SelectedMemoryStore::local(tempdir().unwrap().path().join("memory.sqlite3")),
            TestAuthorizer,
        );
        let definition = tool.definition();
        let schema = definition.parameters().unwrap().as_value();
        let output_schema = definition.output_schema().unwrap().as_value();

        assert_eq!(definition.name(), "memory");
        assert_eq!(schema["oneOf"].as_array().unwrap().len(), 5);
        assert!(
            schema["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .all(|operation| { operation["additionalProperties"] == json!(false) })
        );
        let scan_output = output_schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|operation| operation["properties"]["operation"]["const"] == json!("scan"))
            .expect("scan output should be exposed");
        for group in ["ours", "theirs"] {
            let candidates = &scan_output["properties"][group];
            assert_eq!(candidates["maxItems"], json!(5));
            assert_eq!(
                candidates["items"]["properties"]["preview"]["maxLength"],
                json!(64)
            );
        }
        assert!(scan_output["properties"].get("abstained").is_none());
        assert!(scan_output["properties"].get("candidates").is_none());

        let inspect = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|operation| operation["properties"]["operation"]["const"] == json!("inspect"))
            .expect("inspect input should be exposed");
        assert!(inspect["properties"].get("limit").is_none());
        assert!(inspect["properties"].get("namespaces").is_none());
        let inspect_output = output_schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|operation| operation["properties"]["operation"]["const"] == json!("inspect"))
            .expect("inspect output should be exposed");
        assert_eq!(inspect_output["properties"]["ours"]["maxItems"], json!(20));
        assert_eq!(
            inspect_output["properties"]["coverage"]["properties"]["consistency"]["const"],
            json!("best_effort")
        );
    }

    #[test]
    fn exact_operations_share_the_memory_key_shape() {
        let tool = MemoryTool::new(
            SelectedMemoryStore::local(tempdir().unwrap().path().join("memory.sqlite3")),
            TestAuthorizer,
        );
        let definition = tool.definition();
        let operations = definition.parameters().unwrap().as_value()["oneOf"]
            .as_array()
            .unwrap();
        let operation = |name| {
            operations
                .iter()
                .find(|operation| operation["properties"]["operation"]["const"] == json!(name))
                .unwrap()
        };
        let read = operation("read");
        let delete = operation("delete");
        let delete_output = definition.output_schema().unwrap().as_value()["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|operation| operation["properties"]["operation"]["const"] == json!("delete"))
            .unwrap();

        assert_eq!(read["required"], json!(["operation", "keys"]));
        assert!(read["properties"].get("ids").is_none());
        assert!(
            read["properties"]["keys"]["description"]
                .as_str()
                .unwrap()
                .contains("scan")
        );
        assert_eq!(delete["required"], json!(["operation", "key"]));
        assert!(delete["properties"].get("id").is_none());
        assert_eq!(
            delete_output["required"],
            json!(["operation", "backend", "key"])
        );
        assert!(
            definition.description().contains(
                r#"{"operation":"read","keys":[{"id":7,"version":1,"namespace":"alice"}]}"#
            )
        );

        assert!(
            serde_json::from_value::<MemoryOperation>(json!({"operation": "read", "ids": [1]}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<MemoryOperation>(json!({
                "operation": "delete",
                "key": {"id": 1, "version": 2}
            }))
            .is_ok()
        );
    }

    #[tokio::test]
    async fn reads_and_deletes_keys_returned_by_the_store() {
        let directory = tempdir().unwrap();
        let store = SelectedMemoryStore::local(directory.path().join("memory.sqlite3"));
        let tool = MemoryTool::new(store.clone(), TestAuthorizer);
        tool.execute(
            input(json!({"operation": "scan", "query": "key contract"})),
            context("root"),
        )
        .await
        .unwrap();
        tool.execute(
            input(json!({"operation": "put", "content": "Use one exact memory key shape."})),
            context("root"),
        )
        .await
        .unwrap();
        let key = store.list().await.unwrap().remove(0).key;

        assert!(
            tool.execute(
                input(json!({"operation": "read", "keys": [key.clone()]})),
                context("root"),
            )
            .await
            .unwrap()
            .success
        );
        assert!(
            tool.execute(
                input(json!({"operation": "delete", "key": key})),
                context("root"),
            )
            .await
            .unwrap()
            .success
        );
        assert!(store.list().await.unwrap().is_empty());
    }

    #[test]
    fn serde_rejects_caller_supplied_authority() {
        let input = json!({
            "operation": "delete",
            "key": {"id": 1, "version": 1},
            "is_root": true
        });
        assert!(serde_json::from_value::<MemoryOperation>(input).is_err());
    }

    #[tokio::test]
    async fn root_put_requires_a_fresh_scan() {
        let directory = tempdir().unwrap();
        let store = SelectedMemoryStore::local(directory.path().join("memory.sqlite3"));
        let tool = MemoryTool::new(store.clone(), TestAuthorizer);
        let put = || {
            input(json!({
                "operation": "put",
                "content": "The durable test preference is concise output."
            }))
        };

        let Err(error) = tool.execute(put(), context("root")).await else {
            panic!("put without a scan unexpectedly succeeded");
        };
        assert_eq!(error.to_string(), "scan memory before storing a conclusion");
        let Err(error) = tool
            .execute(
                input(json!({
                    "operation": "scan",
                    "query": "durable preference",
                    "limit": 0
                })),
                context("root"),
            )
            .await
        else {
            panic!("zero-result scan unexpectedly succeeded");
        };
        assert_eq!(
            error.to_string(),
            "memory scan limit must be between 1 and 5"
        );
        let Err(error) = tool.execute(put(), context("root")).await else {
            panic!("invalid scan armed a put");
        };
        assert_eq!(error.to_string(), "scan memory before storing a conclusion");
        assert!(
            tool.execute(
                input(json!({ "operation": "scan", "query": "durable preference" })),
                context("root"),
            )
            .await
            .unwrap()
            .success
        );
        assert!(tool.execute(put(), context("root")).await.unwrap().success);
        assert_eq!(store.list().await.unwrap().len(), 1);
        let Err(error) = tool.execute(put(), context("root")).await else {
            panic!("put reused an earlier scan");
        };
        assert_eq!(error.to_string(), "scan memory before storing a conclusion");
    }

    #[tokio::test]
    async fn inspection_is_fixed_size_paginated_and_does_not_arm_put() {
        let directory = tempdir().unwrap();
        let store = SelectedMemoryStore::local(directory.path().join("memory.sqlite3"));
        for id in 1..=21 {
            store
                .put(&format!("Inspection record number {id}."), None)
                .await
                .unwrap();
        }
        let tool = MemoryTool::new(store, TestAuthorizer);

        let first = tool
            .execute(input(json!({"operation": "inspect"})), context("root"))
            .await
            .unwrap()
            .structured_result();
        assert_eq!(first["ours"].as_array().unwrap().len(), 20);
        assert_eq!(first["theirs"], json!([]));
        assert_eq!(first["coverage"]["records"], json!(20));
        assert_eq!(first["coverage"]["complete"], json!(false));
        assert_eq!(first["coverage"]["consistency"], json!("best_effort"));
        assert_eq!(first["next_cursor"], json!({"id": 20}));
        assert!(first["ours"][0].get("content").is_none());
        assert_eq!(first["ours"][0]["key"], json!({"id": 1, "version": 1}));

        let Err(error) = tool
            .execute(
                input(json!({"operation": "inspect", "cursor": {"id": 19}})),
                context("root"),
            )
            .await
        else {
            panic!("invented inspection cursor unexpectedly succeeded");
        };
        assert_eq!(
            error.to_string(),
            "memory inspection cursor was not issued by the preceding page"
        );

        let second = tool
            .execute(
                input(json!({"operation": "inspect", "cursor": first["next_cursor"]})),
                context("root"),
            )
            .await
            .unwrap()
            .structured_result();
        assert_eq!(second["ours"].as_array().unwrap().len(), 1);
        assert_eq!(second["ours"][0]["key"], json!({"id": 21, "version": 1}));
        assert_eq!(second["next_cursor"], Value::Null);
        assert_eq!(second["coverage"]["complete"], json!(true));

        assert!(
            tool.execute(
                input(json!({"operation": "inspect", "cursor": {"id": 20}})),
                context("root"),
            )
            .await
            .is_err(),
            "consumed inspection cursor unexpectedly succeeded again"
        );

        let restarted = tool
            .execute(input(json!({"operation": "inspect"})), context("root"))
            .await
            .unwrap()
            .structured_result();
        assert_eq!(restarted["next_cursor"], json!({"id": 20}));

        let Err(error) = tool
            .execute(
                input(json!({
                    "operation": "put",
                    "content": "Inspection must not satisfy the scan-before-put gate."
                })),
                context("root"),
            )
            .await
        else {
            panic!("inspection unexpectedly armed a put");
        };
        assert_eq!(error.to_string(), "scan memory before storing a conclusion");
    }

    #[tokio::test]
    async fn inspection_rejects_mismatched_or_non_progressing_local_cursors() {
        let tool = MemoryTool::new(
            SelectedMemoryStore::local(tempdir().unwrap().path().join("memory.sqlite3")),
            TestAuthorizer,
        );
        for cursor in [json!({"id": 0}), json!({"namespace": "alice", "id": 1})] {
            assert!(
                tool.execute(
                    input(json!({"operation": "inspect", "cursor": cursor})),
                    context("root")
                )
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn issued_inspection_cursor_can_retry_after_backend_failure() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        let backup = directory.path().join("memory.backup.sqlite3");
        let store = SelectedMemoryStore::local(&database);
        for id in 1..=21 {
            store
                .put(&format!("Retryable inspection record {id}."), None)
                .await
                .unwrap();
        }
        let tool = MemoryTool::new(store, TestAuthorizer);
        let first = tool
            .execute(input(json!({"operation": "inspect"})), context("root"))
            .await
            .unwrap()
            .structured_result();
        let cursor = first["next_cursor"].clone();
        assert_eq!(cursor, json!({"id": 20}));

        fs::rename(&database, &backup).unwrap();
        fs::create_dir(&database).unwrap();
        assert!(
            tool.execute(
                input(json!({"operation": "inspect", "cursor": cursor.clone()})),
                context("root"),
            )
            .await
            .is_err()
        );
        fs::remove_dir(&database).unwrap();
        fs::rename(&backup, &database).unwrap();

        let retried = tool
            .execute(
                input(json!({"operation": "inspect", "cursor": cursor})),
                context("root"),
            )
            .await
            .unwrap()
            .structured_result();
        assert_eq!(retried["ours"].as_array().unwrap().len(), 1);
        assert_eq!(retried["ours"][0]["key"], json!({"id": 21, "version": 1}));
        assert_eq!(retried["next_cursor"], Value::Null);
    }

    #[tokio::test]
    async fn inspection_advances_past_a_fully_suppressed_remote_page() {
        let suppressed_cursor = ExportCursor {
            namespace: "alice".to_owned(),
            id: 1,
        };
        let safe = MemoryRecord {
            key: MemoryKey::remote("alice".to_owned(), 2, 1),
            content: "Keep safe audit guidance.".to_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
            last_scanned_at_ms: None,
            scan_count: 0,
            last_used_at_ms: None,
            use_count: 0,
            probation_until_ms: None,
        };
        let mut pages = VecDeque::from([
            (Vec::new(), Some(suppressed_cursor)),
            (vec![safe.clone()], None),
        ]);

        let (memories, next_cursor) = advance_inspection_page(None, |_| {
            ready(Ok(pages.pop_front().expect("requested bounded page")))
        })
        .await
        .unwrap();

        assert_eq!(memories, vec![safe]);
        assert_eq!(next_cursor, None);
        assert!(pages.is_empty());
    }

    #[tokio::test]
    async fn inspection_rejects_a_truly_non_progressing_filtered_page() {
        let cursor = ExportCursor {
            namespace: "alice".to_owned(),
            id: 1,
        };
        let result = advance_inspection_page(Some(cursor.clone()), |_| {
            ready(Ok((Vec::new(), Some(cursor.clone()))))
        })
        .await;

        assert!(matches!(result, Err(MemoryError::InvalidPagination)));
    }

    #[tokio::test]
    async fn inspection_bounds_consecutive_fully_suppressed_pages() {
        let calls = Cell::new(0_i64);
        let (memories, next_cursor) = advance_inspection_page(None, |_| {
            calls.set(calls.get() + 1);
            ready(Ok((
                Vec::new(),
                Some(ExportCursor {
                    namespace: "alice".to_owned(),
                    id: calls.get(),
                }),
            )))
        })
        .await
        .unwrap();

        assert!(memories.is_empty());
        assert_eq!(calls.get(), 4);
        assert_eq!(next_cursor.unwrap().id, 4);
    }

    #[tokio::test]
    async fn selected_store_rejects_secret_content_before_storage() {
        let directory = tempdir().unwrap();
        let store = SelectedMemoryStore::local(directory.path().join("memory.sqlite3"));
        let tool = MemoryTool::new(store.clone(), TestAuthorizer);
        tool.execute(
            input(json!({ "operation": "scan", "query": "credentials" })),
            context("root"),
        )
        .await
        .unwrap();

        let Err(error) = tool
            .execute(
                input(json!({ "operation": "put", "content": "password=hunter2" })),
                context("root"),
            )
            .await
        else {
            panic!("secret-bearing memory unexpectedly reached storage");
        };

        assert_eq!(
            error.to_string(),
            "memory content was rejected as a likely secret"
        );
        assert!(store.list().await.unwrap().is_empty());
    }
}
