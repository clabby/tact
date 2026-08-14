//! Explicit agent access to the global memory store.

use super::{MemoryAccess, MemoryKey, MemoryRecord, MemoryStore, SelectedMemoryStore};
use nanocodex::{
    Tool,
    tools::contract::{
        ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, async_trait,
    },
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};
use zeroize::Zeroizing;

const DEFAULT_SCAN_LIMIT: usize = 5;

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
    abstained: bool,
    candidates: Vec<ToolCandidate>,
}

#[derive(Serialize)]
struct ToolCandidate {
    key: MemoryKey,
    preview: String,
    score: f64,
}

#[derive(Serialize)]
struct ReadOutput {
    operation: &'static str,
    backend: MemoryAccess,
    memories: Vec<MemoryRecord>,
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
        let scan = self.store.scan(&query, limit).await?;
        self.searched.store(true, Ordering::Release);
        json_output(&ScanOutput {
            operation: "scan",
            backend,
            abstained: scan.abstained,
            candidates: scan
                .candidates
                .into_iter()
                .map(|candidate| ToolCandidate {
                    key: candidate.key,
                    preview: candidate.preview,
                    score: candidate.score,
                })
                .collect(),
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

#[async_trait]
impl<A> Tool for MemoryTool<A>
where
    A: MutationAuthorizer + 'static,
{
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "memory",
            "Explicitly searches, reads, stores, replaces, or deletes bounded memories in exactly one configured local or remote backend. Pass keys returned by scan unchanged when reading, for example {\"operation\":\"read\",\"keys\":[{\"id\":7,\"version\":1,\"namespace\":\"alice\"}]}. Remote reads include the authenticated and team namespaces. Put and delete are root-agent-only; remote mutation also requires a writer credential.",
            memory_input_schema(),
        )
        .with_output_schema(memory_output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        match input.decode_json::<MemoryOperation>()? {
            MemoryOperation::Scan { query, limit } => self.scan(query, limit).await,
            MemoryOperation::Read { keys } => self.read(keys).await,
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
                    "limit": { "type": "integer", "minimum": 1, "maximum": 5, "default": 5 }
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
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "scan" },
                    "backend": backend.clone(),
                    "abstained": { "type": "boolean" },
                    "candidates": {
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
                    }
                },
                "required": ["operation", "backend", "abstained", "candidates"],
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
    use super::{MemoryOperation, MemoryTool, MutationAuthorizer};
    use crate::{MemoryStore, SelectedMemoryStore};
    use nanocodex::{
        Tool,
        tools::contract::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolContext, ToolInput, async_trait},
    };
    use serde_json::{json, value::to_raw_value};
    use std::io;
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
        assert_eq!(schema["oneOf"].as_array().unwrap().len(), 4);
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
        let candidate = &scan_output["properties"]["candidates"]["items"];
        assert_eq!(candidate["properties"]["preview"]["maxLength"], json!(64));
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
