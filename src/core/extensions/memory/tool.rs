//! Explicit agent access to the global memory store.

use super::{MemoryKey, MemoryRecord, MemoryStore};
use crate::core::extensions::subagents::RootAgentGuard;
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
    time::{SystemTime, UNIX_EPOCH},
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
        ids: Vec<i64>,
    },
    Put {
        content: MemoryContent,
        #[serde(default)]
        replace: Option<MemoryKey>,
    },
    Delete {
        id: i64,
        version: u64,
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
    abstained: bool,
    candidates: Vec<super::MemoryCandidate>,
}

#[derive(Serialize)]
struct ReadOutput {
    operation: &'static str,
    memories: Vec<MemoryRecord>,
}

#[derive(Serialize)]
struct PutOutput {
    operation: &'static str,
    memory: MemoryRecord,
    replaced: bool,
}

#[derive(Serialize)]
struct DeleteOutput {
    operation: &'static str,
    id: i64,
}

pub(crate) struct MemoryTool {
    store: MemoryStore,
    root: RootAgentGuard,
    searched: AtomicBool,
}

impl MemoryTool {
    pub(crate) const fn new(store: MemoryStore, root: RootAgentGuard) -> Self {
        Self {
            store,
            root,
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
        let store = self.store.clone();
        let scan = tokio::task::spawn_blocking(move || store.scan(&query, limit, unix_time_ms()))
            .await??;
        self.searched.store(true, Ordering::Release);
        json_output(&ScanOutput {
            operation: "scan",
            abstained: scan.abstained,
            candidates: scan.candidates,
        })
    }

    async fn read(&self, ids: Vec<i64>) -> ToolResult {
        let store = self.store.clone();
        let memories =
            tokio::task::spawn_blocking(move || store.read(&ids, unix_time_ms())).await??;
        json_output(&ReadOutput {
            operation: "read",
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
        self.root.require_root(session_id).await?;
        if !self.searched.swap(false, Ordering::AcqRel) {
            return Err(io::Error::other("scan memory before storing a conclusion").into());
        }

        let replaced = replace.is_some();
        let store = self.store.clone();
        let memory = tokio::task::spawn_blocking(move || {
            store.put(content.as_str(), replace, unix_time_ms())
        })
        .await??;
        json_output(&PutOutput {
            operation: "put",
            memory,
            replaced,
        })
    }

    async fn delete(&self, session_id: &str, key: MemoryKey) -> ToolResult {
        self.root.require_root(session_id).await?;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.delete(key, unix_time_ms())).await??;
        json_output(&DeleteOutput {
            operation: "delete",
            id: key.id,
        })
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "memory",
            "Explicitly searches, reads, stores, replaces, or deletes bounded global memories. Scan returns candidates without counting them as used; read marks selected memories as deliberately used. Put and delete are root-agent-only.",
            memory_input_schema(),
        )
        .with_output_schema(memory_output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        match input.decode_json::<MemoryOperation>()? {
            MemoryOperation::Scan { query, limit } => self.scan(query, limit).await,
            MemoryOperation::Read { ids } => self.read(ids).await,
            MemoryOperation::Put { content, replace } => {
                self.put(context.session_id(), content, replace).await
            }
            MemoryOperation::Delete { id, version } => {
                self.delete(context.session_id(), MemoryKey { id, version })
                    .await
            }
        }
    }
}

fn unix_time_ms() -> i64 {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(milliseconds).unwrap_or(i64::MAX)
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
                    "ids": {
                        "type": "array",
                        "items": { "type": "integer", "minimum": 1 },
                        "minItems": 1
                    }
                },
                "required": ["operation", "ids"],
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
                    "id": { "type": "integer", "minimum": 1 },
                    "version": { "type": "integer", "minimum": 1 }
                },
                "required": ["operation", "id", "version"],
                "additionalProperties": false
            }
        ]
    })
}

fn memory_output_schema() -> Value {
    let record = memory_record_schema();
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "scan" },
                    "abstained": { "type": "boolean" },
                    "candidates": {
                        "type": "array",
                        "maxItems": 5,
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": memory_key_schema(),
                                "preview": { "type": "string" },
                                "score": { "type": "number" }
                            },
                            "required": ["key", "preview", "score"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["operation", "abstained", "candidates"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "read" },
                    "memories": { "type": "array", "items": record.clone() }
                },
                "required": ["operation", "memories"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "put" },
                    "memory": record,
                    "replaced": { "type": "boolean" }
                },
                "required": ["operation", "memory", "replaced"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "const": "delete" },
                    "id": { "type": "integer" }
                },
                "required": ["operation", "id"],
                "additionalProperties": false
            }
        ]
    })
}

fn memory_key_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "integer", "minimum": 1 },
            "version": { "type": "integer", "minimum": 1 }
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
    use super::{MemoryOperation, MemoryTool};
    use crate::core::extensions::{memory::MemoryStore, subagents};
    use nanocodex::{
        Tool,
        tools::contract::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolContext, ToolInput},
    };
    use serde_json::{json, value::to_raw_value};
    use tempfile::tempdir;

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
        let (registry, _, _) = subagents::channel(1);
        let tool = MemoryTool::new(
            MemoryStore::new(tempdir().unwrap().path().join("memory.sqlite3")),
            subagents::RootAgentGuard::new(&registry),
        );
        let definition = tool.definition();
        let schema = definition.parameters().unwrap().as_value();

        assert_eq!(definition.name(), "memory");
        assert_eq!(schema["oneOf"].as_array().unwrap().len(), 4);
        assert!(
            schema["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .all(|operation| { operation["additionalProperties"] == json!(false) })
        );
    }

    #[test]
    fn serde_rejects_caller_supplied_authority() {
        let input = json!({
            "operation": "delete",
            "id": 1,
            "version": 1,
            "is_root": true
        });
        assert!(serde_json::from_value::<MemoryOperation>(input).is_err());
    }

    #[tokio::test]
    async fn root_put_requires_a_fresh_scan() {
        let directory = tempdir().unwrap();
        let store = MemoryStore::new(directory.path().join("memory.sqlite3"));
        let (registry, _, _) = subagents::channel(1);
        let tool = MemoryTool::new(store.clone(), subagents::RootAgentGuard::new(&registry));
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
        assert_eq!(store.list(0).unwrap().len(), 1);
        let Err(error) = tool.execute(put(), context("root")).await else {
            panic!("put reused an earlier scan");
        };
        assert_eq!(error.to_string(), "scan memory before storing a conclusion");
    }
}
