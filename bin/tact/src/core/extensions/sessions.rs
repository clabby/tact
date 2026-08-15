//! Bounded discovery and read-only access to V2 session transcripts.

use crate::tui::{
    storage::{DecodedStoredRecord, SessionStorage, StorageError, StoredSession},
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
use std::{
    io,
    io::Write,
    path::{Path, PathBuf},
};
use thiserror::Error;

const APPROX_BYTES_PER_TOKEN: usize = 4;
const CURSOR_RESERVE_BYTES: usize = 32;
const STORAGE_PAGE_SIZE: usize = 100;
const MAX_SCANNED_RECORDS: usize = 10_000;
const MAX_SCANNED_BYTES: usize = 16 * 1024 * 1024;
const MAX_KIND_FILTERS: usize = 16;
const MAX_TEXT_FILTERS: usize = 16;
const MAX_TEXT_FILTER_BYTES: usize = 256;
const MAX_FOUND_SESSIONS: usize = 30;
const MAX_WORKSPACE_BYTES: usize = 4_096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindSessionsInput {
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    contains_any: Option<Vec<String>>,
    #[serde(default)]
    cursor: Option<SessionCursor>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SessionCursor {
    updated_at_ms: u64,
    session_id: String,
}

#[derive(Serialize)]
struct FindSessionsOutput {
    sessions: Vec<FoundSession>,
    next_cursor: Option<SessionCursor>,
}

#[derive(Serialize)]
struct FoundSession {
    session_id: String,
    parent_session_id: Option<String>,
    workspace: PathBuf,
    started_at_ms: u64,
    updated_at_ms: u64,
    preview: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadSessionInput {
    session_id: String,
    #[serde(default)]
    cursor: Option<i64>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    contains_any: Option<Vec<String>>,
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

#[derive(Clone, Copy)]
struct TextMatch {
    start: usize,
    len: usize,
}

#[derive(Debug, Error)]
enum SessionToolError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Encode(#[from] serde_json::Error),
    #[error("the tool output budget is too small to read the next session record")]
    OutputBudgetTooSmall,
}

pub(crate) struct ReadSessionTool {
    config_path: PathBuf,
}

impl ReadSessionTool {
    pub(crate) const fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }

    async fn read(
        &self,
        session_id: String,
        cursor: Option<i64>,
        kinds: Option<Vec<String>>,
        contains_any: Option<Vec<String>>,
        output_token_budget: usize,
    ) -> ToolResult {
        if session_id.trim().is_empty() {
            return Err(io::Error::other("session ID is empty").into());
        }
        if cursor.is_some_and(|cursor| cursor < 0) {
            return Err(io::Error::other("session cursor cannot be negative").into());
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
        validate_text_filters(contains_any.as_deref())?;

        let config_path = self.config_path.clone();
        let requested_id = session_id.clone();
        let output = tokio::task::spawn_blocking(move || {
            let Some(storage) = SessionStorage::open_read_only(&config_path)? else {
                return Ok(None);
            };
            bounded_output(
                &storage,
                requested_id,
                cursor,
                kinds.as_deref(),
                contains_any.as_deref(),
                output_token_budget,
            )
        })
        .await??
        .ok_or_else(|| io::Error::other(format!("session {session_id} was not found")))?;
        Ok(ToolOutput::from_json(serde_json::to_value(output)?, true))
    }
}

fn validate_text_filters(contains_any: Option<&[String]>) -> Result<(), io::Error> {
    if contains_any
        .as_ref()
        .is_some_and(|patterns| patterns.is_empty() || patterns.len() > MAX_TEXT_FILTERS)
    {
        return Err(io::Error::other(
            "provide between 1 and 16 session text patterns",
        ));
    }
    if contains_any.is_some_and(|patterns| {
        patterns
            .iter()
            .any(|pattern| pattern.is_empty() || pattern.len() > MAX_TEXT_FILTER_BYTES)
    }) {
        return Err(io::Error::other(
            "session text patterns must contain 1 to 256 bytes",
        ));
    }
    Ok(())
}

#[async_trait]
impl Tool for ReadSessionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "read_session",
            "Scans a known Tact V2 session incrementally and returns one bounded set of relevant records without loading the full transcript. The session ID may come from an explicit @@ reference or find_sessions. Use kinds and contains_any to filter during the scan. Cursor is an exclusive event-ID boundary: pass next_cursor back to continue a bounded scan, or pass a matched event_id to read following context.",
            read_input_schema(),
        )
        .with_output_schema(read_output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let ReadSessionInput {
            session_id,
            cursor,
            kinds,
            contains_any,
        } = input.decode_json()?;
        self.read(
            session_id,
            cursor,
            kinds,
            contains_any,
            context.output_token_budget(),
        )
        .await
    }
}

pub(crate) struct FindSessionsTool {
    config_path: PathBuf,
}

impl FindSessionsTool {
    pub(crate) const fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }

    async fn find(
        &self,
        current_session: String,
        input: FindSessionsInput,
        output_token_budget: usize,
    ) -> ToolResult {
        validate_text_filters(input.contains_any.as_deref())?;
        if input.workspace.as_ref().is_some_and(|workspace| {
            workspace.as_os_str().is_empty()
                || workspace.to_string_lossy().len() > MAX_WORKSPACE_BYTES
        }) {
            return Err(io::Error::other("workspace paths must contain 1 to 4096 bytes").into());
        }
        if input
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.session_id.trim().is_empty())
        {
            return Err(io::Error::other("session cursor ID is empty").into());
        }

        let config_path = self.config_path.clone();
        let output = tokio::task::spawn_blocking(move || {
            let Some(storage) = SessionStorage::open_read_only(&config_path)? else {
                return Ok(FindSessionsOutput {
                    sessions: Vec::new(),
                    next_cursor: None,
                });
            };
            bounded_session_search(
                &storage,
                &current_session,
                input.workspace.as_deref(),
                input.contains_any.as_deref(),
                input.cursor,
                output_token_budget,
            )
        })
        .await??;
        Ok(ToolOutput::from_json(serde_json::to_value(output)?, true))
    }
}

#[async_trait]
impl Tool for FindSessionsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "find_sessions",
            "Finds bounded recent Tact V2 sessions before using read_session. It excludes the caller's current session, reports parent session IDs for lineage checks, optionally scopes to one exact workspace, and optionally matches ASCII-case-insensitive literal patterns against stored user prompts. Omit workspace only for cross-workspace discovery. Pass next_cursor back only when another page is needed.",
            find_input_schema(),
        )
        .with_output_schema(find_output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        self.find(
            context.session_id().to_owned(),
            input.decode_json()?,
            context.output_token_budget(),
        )
        .await
    }
}

fn bounded_session_search(
    storage: &SessionStorage,
    current_session: &str,
    workspace: Option<&Path>,
    contains_any: Option<&[String]>,
    cursor: Option<SessionCursor>,
    output_token_budget: usize,
) -> Result<FindSessionsOutput, SessionToolError> {
    let max_bytes = output_token_budget.saturating_mul(APPROX_BYTES_PER_TOKEN);
    let page = storage.find_sessions(
        current_session,
        workspace,
        contains_any,
        cursor
            .as_ref()
            .map(|cursor| (cursor.updated_at_ms, cursor.session_id.as_str())),
        MAX_FOUND_SESSIONS,
    )?;
    let next_page = page
        .next_cursor
        .map(|(updated_at_ms, session_id)| SessionCursor {
            updated_at_ms,
            session_id,
        });
    let mut output = FindSessionsOutput {
        sessions: Vec::new(),
        next_cursor: next_page.clone(),
    };
    if serialized_len(&output)? > max_bytes {
        return Err(SessionToolError::OutputBudgetTooSmall);
    }

    let returned_sessions = page.sessions.len();
    let mut last_cursor = None;
    for (index, session) in page.sessions.into_iter().enumerate() {
        let next = SessionCursor {
            updated_at_ms: session.updated_at_unix_ms,
            session_id: session.session_id.clone(),
        };
        output.sessions.push(found_session(session));
        output.next_cursor = if index.saturating_add(1) < returned_sessions {
            Some(next.clone())
        } else {
            next_page.clone()
        };
        if serialized_len(&output)? > max_bytes {
            output.sessions.pop();
            if last_cursor.is_none() {
                return Err(SessionToolError::OutputBudgetTooSmall);
            }
            output.next_cursor = last_cursor;
            return Ok(output);
        }
        last_cursor = Some(next);
    }
    Ok(output)
}

fn found_session(session: StoredSession) -> FoundSession {
    let mut characters = session.preview.chars();
    let mut preview = characters.by_ref().take(512).collect::<String>();
    if characters.next().is_some() {
        preview.push('…');
    }
    FoundSession {
        session_id: session.session_id,
        parent_session_id: session.parent_session_id,
        workspace: session.workspace,
        started_at_ms: session.started_at_unix_ms,
        updated_at_ms: session.updated_at_unix_ms,
        preview,
    }
}

fn bounded_output(
    storage: &SessionStorage,
    session_id: String,
    initial_cursor: Option<i64>,
    kinds: Option<&[String]>,
    contains_any: Option<&[String]>,
    output_token_budget: usize,
) -> Result<Option<ReadSessionOutput>, SessionToolError> {
    let max_bytes = output_token_budget.saturating_mul(APPROX_BYTES_PER_TOKEN);
    let mut cursor = initial_cursor.unwrap_or(0);
    let mut scanned_bytes = 0usize;
    let mut output = ReadSessionOutput {
        session_id,
        records: Vec::new(),
        scanned_records: 0,
        next_cursor: None,
    };
    let empty_output_bytes = serialized_len(&output)?;
    if empty_output_bytes.saturating_add(CURSOR_RESERVE_BYTES) > max_bytes {
        return Err(SessionToolError::OutputBudgetTooSmall);
    }

    loop {
        let remaining_records = MAX_SCANNED_RECORDS.saturating_sub(output.scanned_records);
        let page_size = STORAGE_PAGE_SIZE.min(remaining_records);
        if page_size == 0 {
            return finish_output(output, Some(cursor), max_bytes);
        }
        let remaining_bytes = MAX_SCANNED_BYTES.saturating_sub(scanned_bytes);
        let Some(page) = storage.load_record_page(
            &output.session_id,
            Some(cursor),
            page_size,
            remaining_bytes,
        )?
        else {
            return Ok(None);
        };
        let storage_has_more = page.next_cursor.is_some();
        let page_records = page.records.len();
        if page_records == 0 {
            return finish_output(output, None, max_bytes);
        }

        for (index, stored) in page.records.into_iter().enumerate() {
            let stored = stored.decode()?;
            output.scanned_records = output.scanned_records.saturating_add(1);
            scanned_bytes = scanned_bytes.saturating_add(stored.encoded_bytes);
            let records_remain = index.saturating_add(1) < page_records || storage_has_more;
            let matches_kind =
                kinds.is_none_or(|kinds| kinds.iter().any(|kind| kind == stored.record.kind()));
            let text_match = contains_any.and_then(|patterns| {
                patterns.iter().find_map(|pattern| {
                    find_ignore_ascii_case(stored.record.payload_json(), pattern)
                })
            });
            let matches_text = contains_any.is_none() || text_match.is_some();
            if !matches_kind || !matches_text {
                cursor = stored.event_id;
                if scan_budget_exhausted(&output, scanned_bytes) {
                    return finish_output(output, records_remain.then_some(cursor), max_bytes);
                }
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
                if scan_budget_exhausted(&output, scanned_bytes) {
                    return finish_output(output, records_remain.then_some(cursor), max_bytes);
                }
                continue;
            }
            let empty_page_capacity = max_bytes
                .saturating_sub(empty_output_bytes)
                .saturating_sub(CURSOR_RESERVE_BYTES);
            if full_record_bytes <= empty_page_capacity {
                return finish_output(output, Some(cursor), max_bytes);
            }

            let truncated = OutputRecord {
                event_id,
                record: truncated_record(&stored, 256, text_match)?,
            };
            let included = if push_if_fits(&mut output, truncated, max_bytes)? {
                true
            } else {
                push_if_fits(
                    &mut output,
                    OutputRecord {
                        event_id,
                        record: truncated_record(&stored, 0, text_match)?,
                    },
                    max_bytes,
                )?
            };
            if !included {
                return Err(SessionToolError::OutputBudgetTooSmall);
            }
            cursor = event_id;
            return finish_output(output, records_remain.then_some(cursor), max_bytes);
        }

        if !storage_has_more {
            return finish_output(output, None, max_bytes);
        }
    }
}

fn finish_output(
    mut output: ReadSessionOutput,
    next_cursor: Option<i64>,
    max_bytes: usize,
) -> Result<Option<ReadSessionOutput>, SessionToolError> {
    output.next_cursor = next_cursor;
    if serialized_len(&output)? > max_bytes {
        return Err(SessionToolError::OutputBudgetTooSmall);
    }
    Ok(Some(output))
}

fn scan_budget_exhausted(output: &ReadSessionOutput, scanned_bytes: usize) -> bool {
    output.scanned_records >= MAX_SCANNED_RECORDS || scanned_bytes >= MAX_SCANNED_BYTES
}

fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<TextMatch> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
        .map(|start| TextMatch {
            start,
            len: needle.len(),
        })
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
    text_match: Option<TextMatch>,
) -> Result<Value, serde_json::Error> {
    let payload = stored.record.payload_json();
    let required_bytes = text_match.map_or(0, |text_match| text_match.len);
    let preview_bytes = preview_bytes.max(required_bytes).min(payload.len());
    let mut preview_start = text_match.map_or(0, |text_match| {
        text_match
            .start
            .saturating_sub(preview_bytes.saturating_sub(text_match.len) / 2)
    });
    let mut preview_end = preview_start
        .saturating_add(preview_bytes)
        .min(payload.len());
    if let Some(text_match) = text_match {
        let match_end = text_match.start.saturating_add(text_match.len);
        if preview_end < match_end {
            preview_end = match_end.min(payload.len());
            preview_start = preview_end.saturating_sub(preview_bytes);
        }
    }
    while !payload.is_char_boundary(preview_start) {
        preview_start = preview_start.saturating_sub(1);
    }
    while !payload.is_char_boundary(preview_end) {
        preview_end = preview_end.saturating_sub(1);
    }

    let mut record = serde_json::to_value(&stored.record)?;
    record["payload"] = json!({
        "truncated": true,
        "original_bytes": payload.len(),
        "preview_start": preview_start,
        "preview": &payload[preview_start..preview_end]
    });
    Ok(record)
}

fn find_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "workspace": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_WORKSPACE_BYTES,
                "description": "Optional exact workspace path. Supply the current workspace by default; omit only for intentional cross-workspace discovery."
            },
            "contains_any": {
                "type": "array",
                "items": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT_FILTER_BYTES },
                "minItems": 1,
                "maxItems": MAX_TEXT_FILTERS,
                "uniqueItems": true,
                "description": "Optional ASCII-case-insensitive literal patterns matched against stored user prompts; a session is returned when any pattern matches."
            },
            "cursor": {
                "type": "object",
                "properties": {
                    "updated_at_ms": { "type": "integer", "minimum": 0 },
                    "session_id": { "type": "string", "minLength": 1 }
                },
                "required": ["updated_at_ms", "session_id"],
                "additionalProperties": false,
                "description": "An exclusive continuation cursor returned as next_cursor by an earlier find_sessions call."
            }
        },
        "additionalProperties": false
    })
}

fn find_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessions": {
                "type": "array",
                "maxItems": MAX_FOUND_SESSIONS,
                "items": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string" },
                        "parent_session_id": {
                            "type": ["string", "null"],
                            "description": "The direct parent session ID for a fork or descendant, or null for a root session."
                        },
                        "workspace": { "type": "string" },
                        "started_at_ms": { "type": "integer", "minimum": 0 },
                        "updated_at_ms": { "type": "integer", "minimum": 0 },
                        "preview": { "type": "string" }
                    },
                    "required": ["session_id", "parent_session_id", "workspace", "started_at_ms", "updated_at_ms", "preview"],
                    "additionalProperties": false
                }
            },
            "next_cursor": {
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "updated_at_ms": { "type": "integer", "minimum": 0 },
                            "session_id": { "type": "string" }
                        },
                        "required": ["updated_at_ms", "session_id"],
                        "additionalProperties": false
                    },
                    { "type": "null" }
                ]
            }
        },
        "required": ["sessions", "next_cursor"],
        "additionalProperties": false
    })
}

fn read_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": {
                "type": "string",
                "minLength": 1,
                "description": "An exact Tact V2 session ID from an explicit @@ reference or find_sessions."
            },
            "cursor": {
                "type": "integer",
                "minimum": 0,
                "description": "An exclusive event-ID boundary. Use next_cursor to continue a bounded scan or a matched event_id to read following context; omit to start at the beginning."
            },
            "kinds": {
                "type": "array",
                "items": { "type": "string", "minLength": 1, "maxLength": 64 },
                "minItems": 1,
                "maxItems": MAX_KIND_FILTERS,
                "uniqueItems": true,
                "description": "Optional exact transcript record types to return, such as user.submitted or assistant.message."
            },
            "contains_any": {
                "type": "array",
                "items": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT_FILTER_BYTES },
                "minItems": 1,
                "maxItems": MAX_TEXT_FILTERS,
                "uniqueItems": true,
                "description": "Optional ASCII-case-insensitive literal patterns matched against each record payload; a record is returned when any pattern matches."
            }
        },
        "required": ["session_id"],
        "additionalProperties": false
    })
}

fn read_output_schema() -> Value {
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
    use super::{
        APPROX_BYTES_PER_TOKEN, FindSessionsTool, MAX_FOUND_SESSIONS, MAX_TEXT_FILTERS,
        ReadSessionTool, STORAGE_PAGE_SIZE, find_ignore_ascii_case, truncated_record,
    };
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
    fn read_definition_exposes_a_closed_bounded_reader() {
        let directory = tempdir().unwrap();
        let tool = ReadSessionTool::new(directory.path().join("config.toml"));
        let definition = tool.definition();
        let input = definition.parameters().unwrap().as_value();
        let output = definition.output_schema().unwrap().as_value();

        assert_eq!(definition.name(), "read_session");
        assert!(definition.description().contains("find_sessions"));
        assert!(
            definition
                .description()
                .contains("exclusive event-ID boundary")
        );
        assert_eq!(input["additionalProperties"], json!(false));
        assert!(
            input["properties"]["session_id"]["description"]
                .as_str()
                .unwrap()
                .contains("find_sessions")
        );
        assert!(
            input["properties"]["cursor"]["description"]
                .as_str()
                .unwrap()
                .contains("matched event_id")
        );
        assert!(input["properties"].get("limit").is_none());
        assert_eq!(
            input["properties"]["contains_any"]["maxItems"],
            MAX_TEXT_FILTERS
        );
        assert_eq!(output["additionalProperties"], json!(false));
    }

    #[test]
    fn find_definition_exposes_bounded_discovery() {
        let directory = tempdir().unwrap();
        let tool = FindSessionsTool::new(directory.path().join("config.toml"));
        let definition = tool.definition();
        let input = definition.parameters().unwrap().as_value();
        let output = definition.output_schema().unwrap().as_value();

        assert_eq!(definition.name(), "find_sessions");
        assert!(definition.description().contains("current session"));
        assert!(definition.description().contains("read_session"));
        assert_eq!(input["additionalProperties"], json!(false));
        assert_eq!(input["properties"]["contains_any"]["maxItems"], 16);
        assert_eq!(input["properties"]["cursor"]["additionalProperties"], false);
        assert_eq!(output["properties"]["sessions"]["maxItems"], 30);
        assert_eq!(
            output["properties"]["sessions"]["items"]["properties"]["parent_session_id"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(output["additionalProperties"], json!(false));
    }

    #[test]
    fn truncated_agent_records_preserve_correlation_metadata() {
        let stored = DecodedStoredRecord {
            event_id: 7,
            encoded_bytes: 10_000,
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

        let truncated = truncated_record(&stored, 0, None).unwrap();

        assert_eq!(truncated["agent"]["protocol_version"], 2);
        assert_eq!(truncated["agent"]["request_id"], "request");
        assert_eq!(truncated["agent"]["sequence"], 17);
    }

    #[test]
    fn truncated_search_results_include_a_late_match() {
        let text = format!("{}Session Needle{}", "x".repeat(5_000), "y".repeat(5_000));
        let stored = DecodedStoredRecord {
            event_id: 7,
            encoded_bytes: text.len(),
            record: TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::UserSubmitted {
                    id: TurnId::new(1),
                    text,
                },
            )
            .unwrap(),
        };
        let text_match = find_ignore_ascii_case(stored.record.payload_json(), "session needle");

        let truncated = truncated_record(&stored, 256, text_match).unwrap();

        assert!(truncated["payload"]["preview_start"].as_u64().unwrap() > 0);
        assert!(
            truncated["payload"]["preview"]
                .as_str()
                .unwrap()
                .contains("Session Needle")
        );
    }

    #[tokio::test]
    async fn finds_stored_sessions_without_returning_the_current_session() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config_path).unwrap();
        append_session(
            &mut storage,
            "current",
            None,
            "/work",
            400,
            "validation needle",
        );
        append_session(
            &mut storage,
            "matching",
            Some("root-session"),
            "/work",
            300,
            "Validation Needle",
        );
        append_session(
            &mut storage,
            "unrelated",
            None,
            "/work",
            200,
            "different topic",
        );
        append_session(
            &mut storage,
            "other",
            None,
            "/other",
            100,
            "validation needle",
        );

        let tool = FindSessionsTool::new(config_path);
        let output = execute_find(
            &tool,
            "current",
            json!({
                "workspace": "/work",
                "contains_any": ["validation needle"]
            }),
            1_000,
        )
        .await;

        assert_eq!(output["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(output["sessions"][0]["session_id"], "matching");
        assert_eq!(output["sessions"][0]["parent_session_id"], "root-session");
        assert_eq!(output["sessions"][0]["workspace"], "/work");
        assert_eq!(output["sessions"][0]["updated_at_ms"], 300);
        assert!(output["next_cursor"].is_null());

        let cross_workspace = execute_find(&tool, "current", json!({}), 1_000).await;
        assert_eq!(
            cross_workspace["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|session| session["session_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["matching", "unrelated", "other"]
        );
    }

    #[tokio::test]
    async fn session_find_continuation_starts_after_the_last_returned_session() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config_path).unwrap();
        for index in 0..=MAX_FOUND_SESSIONS {
            append_session(
                &mut storage,
                &format!("session-{index:02}"),
                None,
                "/work",
                1_000_u64.saturating_sub(u64::try_from(index).unwrap()),
                "prompt",
            );
        }

        let tool = FindSessionsTool::new(config_path);
        let first = execute_find(&tool, "current", json!({"workspace": "/work"}), 10_000).await;
        assert_eq!(
            first["sessions"].as_array().unwrap().len(),
            MAX_FOUND_SESSIONS
        );
        let cursor = first["next_cursor"].clone();
        assert!(cursor.is_object());

        let second = execute_find(
            &tool,
            "current",
            json!({"workspace": "/work", "cursor": cursor}),
            10_000,
        )
        .await;
        assert_eq!(second["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(second["sessions"][0]["session_id"], "session-30");
        assert!(second["next_cursor"].is_null());

        let sparse = execute_find(
            &tool,
            "current",
            json!({"workspace": "/work", "contains_any": ["absent"]}),
            10_000,
        )
        .await;
        assert!(sparse["sessions"].as_array().unwrap().is_empty());
        assert!(sparse["next_cursor"].is_object());
    }

    #[tokio::test]
    async fn reads_a_referenced_session_end_to_end() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config_path).unwrap();
        let records = [
            session_started("referenced"),
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

        let tool = ReadSessionTool::new(config_path);
        let input = ToolInput::Function(
            to_raw_value(&json!({
                "session_id": "referenced",
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

    #[tokio::test]
    async fn one_call_searches_across_storage_pages() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config_path).unwrap();
        let final_sequence = u64::try_from(STORAGE_PAGE_SIZE * 2 + 2).unwrap();
        let mut records = vec![session_started("searched")];
        for sequence in 2..=final_sequence {
            let text = if sequence == final_sequence {
                "A CommonWare session needle"
            } else {
                "irrelevant"
            };
            records.push(Arc::new(
                TranscriptRecord::from_local(
                    sequence,
                    sequence,
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(sequence),
                        text: text.to_owned(),
                    },
                )
                .unwrap(),
            ));
        }
        storage.append_records("searched", &records).unwrap();

        let tool = ReadSessionTool::new(config_path);
        let input = ToolInput::Function(
            to_raw_value(&json!({
                "session_id": "searched",
                "kinds": ["user.submitted"],
                "contains_any": ["commonware", "another needle"]
            }))
            .unwrap(),
        );
        let context = ToolContext::new("test-model", "current", "search-call", &[], 1_000);
        let output = tool.execute(input, context).await.unwrap();
        let ToolOutputBody::Text(output) = output.output else {
            panic!("session reader returned non-text output");
        };
        let output: Value = serde_json::from_str(&output).unwrap();

        assert_eq!(output["records"].as_array().unwrap().len(), 1);
        assert_eq!(output["records"][0]["record"]["type"], "user.submitted");
        assert_eq!(output["scanned_records"], final_sequence);
        assert!(output["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn matched_event_id_reads_the_following_context() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config_path).unwrap();
        let records = [
            session_started("adjacent"),
            Arc::new(
                TranscriptRecord::from_local(
                    2,
                    2,
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(1),
                        text: "validation needle".to_owned(),
                    },
                )
                .unwrap(),
            ),
            Arc::new(TranscriptRecord::from_agent(
                3,
                3,
                AgentEvent {
                    protocol_version: 2,
                    request_id: Arc::from("request"),
                    seq: 1,
                    kind: AgentEventKind::AssistantMessage,
                    payload: to_raw_value(&json!({
                        "model_call_index": 1,
                        "item_id": "answer",
                        "phase": "final_answer",
                        "text": "following response"
                    }))
                    .unwrap()
                    .into(),
                },
            )),
        ];
        storage.append_records("adjacent", &records).unwrap();
        let tool = ReadSessionTool::new(config_path);

        let matched = execute(
            &tool,
            json!({
                "session_id": "adjacent",
                "kinds": ["user.submitted"],
                "contains_any": ["validation needle"]
            }),
            1_000,
        )
        .await;
        let event_id = matched["records"][0]["event_id"].as_i64().unwrap();
        let following = execute(
            &tool,
            json!({
                "session_id": "adjacent",
                "cursor": event_id,
                "kinds": ["assistant.message"]
            }),
            1_000,
        )
        .await;

        assert_eq!(following["records"].as_array().unwrap().len(), 1);
        assert_eq!(following["records"][0]["event_id"], 3);
        assert_eq!(
            following["records"][0]["record"]["payload"]["text"],
            "following response"
        );
    }

    #[tokio::test]
    async fn output_continuation_resumes_before_the_first_omitted_match() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config_path).unwrap();
        let records = [
            session_started("continued"),
            Arc::new(
                TranscriptRecord::from_local(
                    2,
                    2,
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(2),
                        text: format!("match first {}", "x".repeat(600)),
                    },
                )
                .unwrap(),
            ),
            Arc::new(
                TranscriptRecord::from_local(
                    3,
                    3,
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(3),
                        text: format!("match second {}", "x".repeat(600)),
                    },
                )
                .unwrap(),
            ),
        ];
        storage.append_records("continued", &records).unwrap();

        let tool = ReadSessionTool::new(config_path);
        let first = execute_search(&tool, None, 300).await;
        assert_eq!(first["records"].as_array().unwrap().len(), 1);
        assert_eq!(
            first["records"][0]["record"]["payload"]["text"]
                .as_str()
                .unwrap()
                .len(),
            612
        );
        let cursor = first["next_cursor"].as_i64().unwrap();

        let second = execute_search(&tool, Some(cursor), 300).await;
        assert_eq!(second["records"].as_array().unwrap().len(), 1);
        assert!(
            second["records"][0]["record"]["payload"]["text"]
                .as_str()
                .unwrap()
                .starts_with("match second")
        );
        assert!(second["next_cursor"].is_null());
    }

    async fn execute_search(tool: &ReadSessionTool, cursor: Option<i64>, budget: usize) -> Value {
        execute(
            tool,
            json!({
                "session_id": "continued",
                "cursor": cursor,
                "kinds": ["user.submitted"],
                "contains_any": ["match"]
            }),
            budget,
        )
        .await
    }

    async fn execute(tool: &ReadSessionTool, input: Value, budget: usize) -> Value {
        let input = ToolInput::Function(to_raw_value(&input).unwrap());
        let context = ToolContext::new("test-model", "current", "continuation-call", &[], budget);
        let output = tool.execute(input, context).await.unwrap();
        let ToolOutputBody::Text(output) = output.output else {
            panic!("session reader returned non-text output");
        };
        serde_json::from_str(&output).unwrap()
    }

    async fn execute_find(
        tool: &FindSessionsTool,
        current_session: &str,
        input: Value,
        budget: usize,
    ) -> Value {
        let input = ToolInput::Function(to_raw_value(&input).unwrap());
        let context = ToolContext::new(
            "test-model",
            current_session,
            "find-sessions-call",
            &[],
            budget,
        );
        let output = tool.execute(input, context).await.unwrap();
        let ToolOutputBody::Text(output) = output.output else {
            panic!("session finder returned non-text output");
        };
        assert!(output.len() <= budget.saturating_mul(APPROX_BYTES_PER_TOKEN));
        serde_json::from_str(&output).unwrap()
    }

    fn append_session(
        storage: &mut SessionStorage,
        session_id: &str,
        parent_session_id: Option<&str>,
        workspace: &str,
        at: u64,
        prompt: &str,
    ) {
        let records = [
            Arc::new(
                TranscriptRecord::from_local(
                    1,
                    at.saturating_sub(1),
                    LocalEvent::SessionStarted(SessionStarted {
                        session_id: session_id.to_owned(),
                        parent_session_id: parent_session_id.map(str::to_owned),
                        parent_sequence: None,
                        model: "model".to_owned(),
                        effort: ReasoningEffort::Medium,
                        reasoning_mode: ReasoningMode::Standard,
                        fast_mode: false,
                        workspace: workspace.into(),
                        application_version: "test".to_owned(),
                    }),
                )
                .unwrap(),
            ),
            Arc::new(
                TranscriptRecord::from_local(
                    2,
                    at,
                    LocalEvent::UserSubmitted {
                        id: TurnId::new(1),
                        text: prompt.to_owned(),
                    },
                )
                .unwrap(),
            ),
        ];
        storage.append_records(session_id, &records).unwrap();
    }

    fn session_started(session_id: &str) -> Arc<TranscriptRecord> {
        Arc::new(
            TranscriptRecord::from_local(
                1,
                1,
                LocalEvent::SessionStarted(SessionStarted {
                    session_id: session_id.to_owned(),
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
        )
    }
}
