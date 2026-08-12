//! Bounded, read-only access to V2 session transcripts.

use crate::tui::{
    storage::{DecodedStoredRecord, SessionStorage, StorageError, StoredRecord},
    transcript::TranscriptRecord,
};
use nanocodex::{
    Tool,
    tools::contract::{
        ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, async_trait,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io, io::Write, path::PathBuf};
use thiserror::Error;

const APPROX_BYTES_PER_TOKEN: usize = 4;
const CURSOR_RESERVE_BYTES: usize = 32;
const DEFAULT_PAGE_SIZE: usize = 20;
const MAX_PAGE_SIZE: usize = 100;
const MAX_KIND_FILTERS: usize = 16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadSessionInput {
    session_id: String,
    #[serde(default)]
    cursor: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
}

#[derive(Serialize)]
struct ReadSessionOutput {
    session_id: String,
    records: Vec<OutputRecord>,
    scanned_records: usize,
    next_cursor: Option<i64>,
}

#[derive(Serialize)]
struct OutputRecord {
    event_id: i64,
    record: Value,
}

#[derive(Serialize)]
struct BorrowedOutputRecord<'a> {
    event_id: i64,
    record: &'a TranscriptRecord,
}

#[derive(Debug, Error)]
enum ReadError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Encode(#[from] serde_json::Error),
    #[error("the tool output budget is too small to read the next session record")]
    OutputBudgetTooSmall,
}

pub(crate) struct SessionTool {
    config_path: PathBuf,
}

impl SessionTool {
    pub(crate) const fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }

    async fn read(
        &self,
        session_id: String,
        cursor: Option<i64>,
        limit: Option<usize>,
        kinds: Option<Vec<String>>,
        output_token_budget: usize,
    ) -> ToolResult {
        if session_id.trim().is_empty() {
            return Err(io::Error::other("session ID is empty").into());
        }
        if cursor.is_some_and(|cursor| cursor < 0) {
            return Err(io::Error::other("session cursor cannot be negative").into());
        }
        let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(io::Error::other("session page size must be between 1 and 100").into());
        }
        if kinds
            .as_ref()
            .is_some_and(|kinds| kinds.is_empty() || kinds.len() > MAX_KIND_FILTERS)
        {
            return Err(io::Error::other("provide between 1 and 16 record kinds").into());
        }
        if kinds.as_ref().is_some_and(|kinds| {
            kinds
                .iter()
                .any(|kind| kind.trim().is_empty() || kind.len() > 64)
        }) {
            return Err(io::Error::other("record kinds must contain 1 to 64 bytes").into());
        }

        let config_path = self.config_path.clone();
        let requested_id = session_id.clone();
        let output = tokio::task::spawn_blocking(move || {
            let Some(storage) = SessionStorage::open_read_only(&config_path)? else {
                return Ok(None);
            };
            let Some(page) = storage.load_record_page(&requested_id, cursor, limit)? else {
                return Ok(None);
            };
            bounded_output(
                requested_id,
                cursor,
                page.records,
                page.next_cursor.is_some(),
                kinds.as_deref(),
                output_token_budget,
            )
            .map(Some)
        })
        .await??
        .ok_or_else(|| io::Error::other(format!("session {session_id} was not found")))?;
        Ok(ToolOutput::from_json(serde_json::to_value(output)?, true))
    }
}

#[async_trait]
impl Tool for SessionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "read_session",
            "Reads at most one bounded page of a referenced Tact V2 session; it never loads the full transcript. Use kinds to return only relevant record types, and pass next_cursor back as cursor only when another page is needed. For broad searches, call this tool from code mode, iterate pages, filter results, and stop as soon as sufficient evidence is found.",
            input_schema(),
        )
        .with_output_schema(output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let ReadSessionInput {
            session_id,
            cursor,
            limit,
            kinds,
        } = input.decode_json()?;
        self.read(
            session_id,
            cursor,
            limit,
            kinds,
            context.output_token_budget(),
        )
        .await
    }
}

fn bounded_output(
    session_id: String,
    initial_cursor: Option<i64>,
    records: Vec<StoredRecord>,
    storage_has_more: bool,
    kinds: Option<&[String]>,
    output_token_budget: usize,
) -> Result<ReadSessionOutput, ReadError> {
    let page_records = records.len();
    let max_bytes = output_token_budget.saturating_mul(APPROX_BYTES_PER_TOKEN);
    let mut cursor = initial_cursor.unwrap_or(0);
    let mut stopped_early = false;
    let mut output = ReadSessionOutput {
        session_id,
        records: Vec::new(),
        scanned_records: 0,
        next_cursor: None,
    };
    let empty_output_bytes = serialized_len(&output)?;

    for (index, stored) in records.into_iter().enumerate() {
        let stored = stored.decode()?;
        output.scanned_records = output.scanned_records.saturating_add(1);
        if kinds.is_some_and(|kinds| !kinds.iter().any(|kind| kind == stored.record.kind())) {
            cursor = stored.event_id;
            continue;
        }

        let event_id = stored.event_id;
        let full_record_bytes = serialized_len(&BorrowedOutputRecord {
            event_id,
            record: &stored.record,
        })?
        .saturating_add(1);
        let remaining = max_bytes
            .saturating_sub(serialized_len(&output)?)
            .saturating_sub(CURSOR_RESERVE_BYTES);
        if full_record_bytes <= remaining
            && push_if_fits(
                &mut output,
                OutputRecord {
                    event_id,
                    record: serde_json::to_value(&stored.record)?,
                },
                max_bytes,
            )?
        {
            cursor = event_id;
            continue;
        }
        let empty_page_capacity = max_bytes
            .saturating_sub(empty_output_bytes)
            .saturating_sub(CURSOR_RESERVE_BYTES);
        if full_record_bytes <= empty_page_capacity {
            stopped_early = true;
            break;
        }

        let truncated = OutputRecord {
            event_id,
            record: truncated_record(&stored, 256)?,
        };
        let included = if push_if_fits(&mut output, truncated, max_bytes)? {
            true
        } else {
            push_if_fits(
                &mut output,
                OutputRecord {
                    event_id,
                    record: truncated_record(&stored, 0)?,
                },
                max_bytes,
            )?
        };
        if included {
            cursor = event_id;
            stopped_early = index.saturating_add(1) < page_records;
        } else {
            return Err(ReadError::OutputBudgetTooSmall);
        }
        break;
    }

    output.next_cursor = (stopped_early || storage_has_more).then_some(cursor);
    if serialized_len(&output)? > max_bytes {
        return Err(ReadError::OutputBudgetTooSmall);
    }
    Ok(output)
}

fn push_if_fits(
    output: &mut ReadSessionOutput,
    record: OutputRecord,
    max_bytes: usize,
) -> Result<bool, serde_json::Error> {
    output.records.push(record);
    if serialized_len(output)?.saturating_add(CURSOR_RESERVE_BYTES) <= max_bytes {
        return Ok(true);
    }
    output.records.pop();
    Ok(false)
}

fn serialized_len(value: &impl Serialize) -> Result<usize, serde_json::Error> {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn truncated_record(
    stored: &DecodedStoredRecord,
    preview_bytes: usize,
) -> Result<Value, serde_json::Error> {
    let payload = stored.record.payload_json();
    let mut preview_end = preview_bytes.min(payload.len());
    while !payload.is_char_boundary(preview_end) {
        preview_end = preview_end.saturating_sub(1);
    }

    let mut record = serde_json::to_value(&stored.record)?;
    record["payload"] = json!({
        "truncated": true,
        "original_bytes": payload.len(),
        "preview": &payload[..preview_end]
    });
    Ok(record)
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": {
                "type": "string",
                "minLength": 1,
                "description": "The exact ID following @@ in the user's prompt."
            },
            "cursor": {
                "type": "integer",
                "minimum": 0,
                "description": "The next_cursor returned by the preceding page. Omit for the first page."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_PAGE_SIZE,
                "default": DEFAULT_PAGE_SIZE,
                "description": "Maximum records read from disk in this page before applying kinds."
            },
            "kinds": {
                "type": "array",
                "items": { "type": "string", "minLength": 1, "maxLength": 64 },
                "minItems": 1,
                "maxItems": MAX_KIND_FILTERS,
                "uniqueItems": true,
                "description": "Optional exact transcript record types to return, such as user.submitted or assistant.message. Filtering is confined to this bounded page."
            }
        },
        "required": ["session_id"],
        "additionalProperties": false
    })
}

fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string" },
            "records": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "event_id": { "type": "integer" },
                        "record": { "type": "object" }
                    },
                    "required": ["event_id", "record"],
                    "additionalProperties": false
                }
            },
            "scanned_records": { "type": "integer", "minimum": 0 },
            "next_cursor": { "type": ["integer", "null"] }
        },
        "required": ["session_id", "records", "scanned_records", "next_cursor"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::{APPROX_BYTES_PER_TOKEN, MAX_PAGE_SIZE, SessionTool, truncated_record};
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode},
        tui::{
            storage::{DecodedStoredRecord, SessionStorage},
            transcript::{LocalEvent, SessionStarted, TranscriptRecord, TurnId},
        },
    };
    use nanocodex::{
        Tool,
        agent::events::{AgentEvent, AgentEventKind},
        tools::contract::{ToolContext, ToolInput, ToolOutputBody},
    };
    use serde_json::{Value, json, value::to_raw_value};
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn definition_exposes_a_closed_bounded_reader() {
        let directory = tempdir().unwrap();
        let tool = SessionTool::new(directory.path().join("config.toml"));
        let definition = tool.definition();
        let input = definition.parameters().unwrap().as_value();
        let output = definition.output_schema().unwrap().as_value();

        assert_eq!(definition.name(), "read_session");
        assert_eq!(input["additionalProperties"], json!(false));
        assert_eq!(input["properties"]["limit"]["maximum"], MAX_PAGE_SIZE);
        assert_eq!(output["additionalProperties"], json!(false));
    }

    #[test]
    fn truncated_agent_records_preserve_correlation_metadata() {
        let stored = DecodedStoredRecord {
            event_id: 7,
            record: TranscriptRecord::from_agent(
                11,
                13,
                AgentEvent {
                    protocol_version: 2,
                    request_id: Arc::from("request"),
                    seq: 17,
                    kind: AgentEventKind::ToolResult,
                    payload: to_raw_value(&json!({"output": "x".repeat(10_000)}))
                        .unwrap()
                        .into(),
                },
            ),
        };

        let truncated = truncated_record(&stored, 0).unwrap();

        assert_eq!(truncated["agent"]["protocol_version"], 2);
        assert_eq!(truncated["agent"]["request_id"], "request");
        assert_eq!(truncated["agent"]["sequence"], 17);
    }

    #[tokio::test]
    async fn reads_a_referenced_session_end_to_end() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config_path).unwrap();
        let records = [
            Arc::new(
                TranscriptRecord::from_local(
                    1,
                    1,
                    LocalEvent::SessionStarted(SessionStarted {
                        session_id: "referenced".to_owned(),
                        parent_session_id: None,
                        parent_sequence: None,
                        model: "model".to_owned(),
                        effort: ReasoningEffort::Medium,
                        reasoning_mode: ReasoningMode::Standard,
                        fast_mode: false,
                        workspace: "/work".into(),
                        application_version: "test".to_owned(),
                    }),
                )
                .unwrap(),
            ),
            Arc::new(
                TranscriptRecord::from_local(
                    2,
                    2,
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(1),
                        text: "x".repeat(10_000),
                    },
                )
                .unwrap(),
            ),
        ];
        storage.append_records("referenced", &records).unwrap();
        storage
            .append_raw_record("referenced", b"not-json")
            .unwrap();

        let tool = SessionTool::new(config_path);
        let input = ToolInput::Function(
            to_raw_value(&json!({
                "session_id": "referenced",
                "limit": 3,
                "kinds": ["user.submitted"]
            }))
            .unwrap(),
        );
        let context = ToolContext::new("test-model", "current", "test-call", &[], 128);
        let output = tool.execute(input, context).await.unwrap();
        let ToolOutputBody::Text(output) = output.output else {
            panic!("session reader returned non-text output");
        };
        assert!(output.len() <= 128 * APPROX_BYTES_PER_TOKEN);
        let output: Value = serde_json::from_str(&output).unwrap();

        assert_eq!(output["session_id"], "referenced");
        assert_eq!(output["records"].as_array().unwrap().len(), 1);
        assert_eq!(output["records"][0]["record"]["type"], "user.submitted");
        assert_eq!(output["records"][0]["record"]["payload"]["truncated"], true);
        assert_eq!(output["scanned_records"], 2);
        assert!(output["next_cursor"].is_number());

        let input = ToolInput::Function(
            to_raw_value(&json!({
                "session_id": "referenced",
                "limit": 2,
                "kinds": ["does.not.exist"]
            }))
            .unwrap(),
        );
        let context = ToolContext::new("test-model", "current", "tiny-call", &[], 1);
        let Err(error) = tool.execute(input, context).await else {
            panic!("tiny output budget unexpectedly succeeded");
        };
        assert_eq!(
            error.to_string(),
            "the tool output budget is too small to read the next session record"
        );
    }
}
