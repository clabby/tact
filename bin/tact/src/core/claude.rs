use crate::{app::config::Config, tui::session::AgentSnapshot};
use futures_util::StreamExt;
use nanocodex::{
    AgentEvents, Claude, HarnessModel as Model, Nanocodex, NanocodexError, Thinking, Tools,
    agent::{AgentHandle, Result},
    claude::{
        ClaudeClient, ClaudeToolInvocation, ClaudeToolReply, ClaudeTools, MAX_TOOL_IMAGE_DIMENSION,
        ToolDefinition as NativeDefinition, ToolResultContent,
    },
    oai::{
        __private::EventSink,
        events::{AgentEvent, AgentEventKind, AgentEventPublisher},
        tower::ResponsesServiceConfig,
    },
    tools::{
        ToolDefinition, ToolInput,
        contract::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolOutputBody, ToolOutputContent},
        embedded::{CodeModeObserver, CodeModeUpdate, OwnedToolContext},
        image::prepare_base64_image,
        runtime::{ImageGenerationConfig, ToolRuntime, WebSearchConfig},
    },
};
use serde_json::{Value, json, value::to_raw_value};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};
use tact_subagents::AgentContext;
use tokio::sync::Notify;

#[path = "claude_lifecycle.rs"]
mod lifecycle;

fn invalid(error: impl std::fmt::Display) -> NanocodexError {
    NanocodexError::InvalidRequest(error.to_string())
}

pub(super) fn tool_runtime(
    config: &Config,
    workspace: &Path,
    tools: &Tools,
) -> Result<ToolRuntime> {
    let (web_search, image_generation) = if tools.web_search_enabled()
        || tools.image_generation_enabled()
    {
        let auth = config.auth().load().map_err(|error| invalid(format!(
            "enabled web search and image generation tools require OpenAI authentication: {error}; configure OpenAI authentication or disable agent.web_search and agent.image_generation for an Anthropic-only setup"
        )))?;
        let tool_config = ResponsesServiceConfig {
            api_base_url: config
                .agent()
                .api_base_url()
                .unwrap_or_else(|| auth.mode().default_api_base_url())
                .to_owned(),
            auth,
            ..ResponsesServiceConfig::default()
        };
        (
            tools.web_search_enabled().then(|| WebSearchConfig {
                endpoint: tool_config.search_endpoint(),
                auth: tool_config.auth.clone(),
            }),
            tools
                .image_generation_enabled()
                .then(|| ImageGenerationConfig {
                    api_base_url: tool_config.api_base_url,
                    auth: tool_config.auth,
                    save_root: workspace.to_path_buf(),
                }),
        )
    } else {
        (None, None)
    };
    Ok(ToolRuntime::new_with_tools(
        workspace,
        web_search,
        image_generation,
        tools,
    ))
}

pub(super) type CleanAgentFactory =
    Arc<dyn Fn(AgentContext, bool) -> Result<(Nanocodex, AgentEvents)> + Send + Sync>;

#[derive(Default)]
pub(super) struct ClaudeSession<'a> {
    pub(super) session_id: Option<&'a str>,
    pub(super) snapshot: Option<AgentSnapshot>,
    pub(super) fast_mode: bool,
}

pub(super) fn build_client(
    client: ClaudeClient,
    context: AgentContext,
    workspace: &Path,
    instructions: Arc<str>,
    session: ClaudeSession<'_>,
    runtime: ToolRuntime,
    spawn: Option<CleanAgentFactory>,
) -> Result<(Nanocodex, AgentEvents)> {
    let ClaudeSession {
        session_id,
        snapshot,
        fast_mode,
    } = session;
    let AgentContext { model, thinking } = context;
    if let Some(snapshot) = &snapshot {
        snapshot.validate_identity(model, session_id)?;
    }
    let Model::Claude(model) = model else {
        return Err(invalid("expected a Claude model"));
    };
    let bridge = Arc::new(Bridge {
        runtime,
        events: Mutex::new(None),
        changed: Notify::new(),
        owner: OnceLock::new(),
    });
    // The native recipe retains this factory; it must not retain the bridge's owner handle.
    let callbacks = Arc::downgrade(&bridge);
    let mut builder = Nanocodex::builder(Claude::new(client, model.as_str()))
        .system(instructions.as_ref())
        .workspace(workspace.to_string_lossy())
        .tools_factory(move |agent| {
            let callbacks = callbacks.upgrade().ok_or(NanocodexError::AgentStopped)?;
            let tools = callbacks.tools(agent.session_id())?;
            callbacks
                .owner
                .set(agent)
                .map_err(|_| invalid("Claude tool owner already initialized"))?;
            Ok(tools)
        });
    if let Some(id) = session_id {
        builder = builder.session_id(id);
    }
    if let Some(snapshot) = snapshot {
        builder = builder.restore_runtime(snapshot.into_claude()?)?;
    }
    // Adaptive reasoning and generated code share the output-token budget.
    builder = builder
        .thinking(thinking)?
        .fast_mode(fast_mode)
        .cache_one_hour()
        .max_tokens(128_000);
    let (native, native_events) = builder.build()?;
    // The wrapper consumes each native turn's mirrored stream instead.
    drop(native_events);
    Ok(lifecycle::wrap(native, bridge, spawn, fast_mode))
}

struct TurnEvents {
    sink: EventSink,
    started: HashSet<String>,
    outputs: HashMap<String, ToolOutputBody>,
}

struct Bridge {
    runtime: ToolRuntime,
    // The native handle supplies live spawn defaults without copying conversation state.
    owner: OnceLock<AgentHandle>,
    events: Mutex<Option<TurnEvents>>,
    changed: Notify,
}

impl Bridge {
    fn install_events(&self, publisher: AgentEventPublisher, turn_id: String) {
        *self.events.lock().unwrap() = Some(TurnEvents {
            sink: EventSink::from_publisher(publisher.with_turn_id(turn_id)),
            started: HashSet::new(),
            outputs: HashMap::new(),
        });
    }

    fn emit(&self, kind: AgentEventKind, mut payload: Value) -> Result<()> {
        let mut events = self.events.lock().unwrap();
        let events = events
            .as_mut()
            .ok_or_else(|| invalid("Claude event stream is inactive"))?;
        if let Some(payload) = payload.as_object_mut() {
            payload.remove("turn_id");
        }
        if kind == AgentEventKind::ToolResult
            && let Some(output) = payload
                .get("call_id")
                .and_then(Value::as_str)
                .and_then(|id| events.outputs.remove(id))
        {
            payload["result"] = serde_json::to_value(output).map_err(invalid)?;
        }
        events.sink.emit(kind, payload).map_err(invalid)
    }

    fn retain_output(&self, call_id: &str, output: &ToolOutputBody) {
        if let Some(events) = self.events.lock().unwrap().as_mut() {
            events.outputs.insert(call_id.to_owned(), output.clone());
        }
    }

    async fn wait_started(&self, call_id: &str) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .events
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|events| events.started.contains(call_id))
            {
                return;
            }
            notified.await;
        }
    }

    fn mark_started(&self, call_id: &str) {
        if let Some(events) = self.events.lock().unwrap().as_mut() {
            events.started.insert(call_id.to_owned());
        }
        self.changed.notify_waiters();
    }

    fn tools(self: &Arc<Self>, session: &str) -> Result<ClaudeTools> {
        let mut definitions = Vec::new();
        for definition in self.runtime.model_specs(session) {
            flatten(definition, None, &mut definitions)?;
        }
        let mut tools = ClaudeTools::new();
        for (definition, name, freeform) in definitions {
            let bridge = self.clone();
            tools = tools.tool_with_context(definition, move |input, invocation| {
                let bridge = bridge.clone();
                let name = name.clone();
                async move { bridge.call(&name, freeform, input, invocation).await }
            });
        }
        Ok(tools)
    }

    async fn call(
        &self,
        name: &str,
        freeform: bool,
        input: Value,
        invocation: ClaudeToolInvocation,
    ) -> std::result::Result<ClaudeToolReply, String> {
        // Nested calls must follow the native parent ToolCall in the combined stream.
        self.wait_started(&invocation.call_id).await;
        let context = OwnedToolContext::new(
            invocation.model,
            invocation.session_id,
            invocation.call_id.clone(),
            Arc::new(Vec::new()),
            DEFAULT_TOOL_OUTPUT_TOKENS,
        )
        .with_turn_id(Some(Arc::from(invocation.turn_id)))
        .with_instruction_revision(invocation.instruction_revision)
        .with_host_context(invocation.host_context);
        if matches!(name, "exec" | "wait") {
            let mut observer = Observer {
                bridge: self,
                error: None,
            };
            let execution = if name == "exec" {
                let source = input
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or("exec requires a code string")?;
                self.runtime
                    .execute_code_owned_with_updates(source, context, &mut observer)
                    .await
            } else {
                self.runtime
                    .wait_for_code_with_updates(
                        &input.to_string(),
                        context.as_context(),
                        &mut observer,
                    )
                    .await
            }
            .map_err(|error| error.to_string())?;
            if let Some(error) = observer.error {
                return Err(error);
            }
            let structured = execution.output.structured_result();
            self.retain_output(&invocation.call_id, &execution.output);
            let mut reply = tool_reply(execution.output, execution.success, None, structured).await;
            for notification in execution.notifications {
                match &mut reply.content {
                    ToolResultContent::Text(text) => {
                        text.push('\n');
                        text.push_str(&notification.text);
                    }
                    ToolResultContent::Blocks(blocks) => {
                        blocks.push(json!({"type":"text","text":notification.text}))
                    }
                }
            }
            return Ok(reply);
        }
        let input = if freeform {
            ToolInput::Freeform(
                input
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or("tool requires a code string")?
                    .to_owned(),
            )
        } else {
            ToolInput::Function(to_raw_value(&input).map_err(|error| error.to_string())?)
        };
        let mut output = self
            .runtime
            .execute_tool(name, input, context.as_context())
            .await
            .map_err(|error| error.to_string())?;
        let structured = output.take_structured_result();
        self.retain_output(&invocation.call_id, &output.output);
        let metadata = output
            .metadata
            .map(|value| serde_json::from_str(value.get()))
            .transpose()
            .map_err(|error| error.to_string())?;
        Ok(tool_reply(output.output, output.success, metadata, structured).await)
    }
}

async fn tool_reply(
    output: ToolOutputBody,
    success: bool,
    metadata: Option<Value>,
    structured: Value,
) -> ClaudeToolReply {
    let content = if matches!(&output, ToolOutputBody::Content(items) if items.iter().any(|item| matches!(item, ToolOutputContent::InputImage { .. })))
    {
        tokio::task::spawn_blocking(move || native_content(output))
            .await
            .unwrap_or_else(|_| {
                Err("image content omitted because it could not be processed".into())
            })
    } else {
        native_content(output)
    };
    let (content, is_error) = match content {
        Ok(content) => (content, !success),
        Err(reason) => (ToolResultContent::Text(reason), true),
    };
    ClaudeToolReply {
        content,
        is_error,
        metadata,
        structured_result: Some(structured),
    }
}

fn flatten(
    definition: ToolDefinition,
    namespace: Option<&str>,
    result: &mut Vec<(NativeDefinition, String, bool)>,
) -> Result<()> {
    if let ToolDefinition::Namespace { name, tools, .. } = definition {
        for tool in tools {
            flatten(tool, Some(&name), result)?;
        }
        return Ok(());
    }
    let name = namespace.map_or_else(
        || definition.name().to_owned(),
        |namespace| format!("{namespace}__{}", definition.name()),
    );
    let freeform = matches!(definition, ToolDefinition::Custom { .. });
    let description = definition.description().replace(
        "Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.",
        "Pass JavaScript source in the JSON code field.",
    );
    let schema = if freeform {
        json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false})
    } else {
        definition.parameters().map_or_else(
            || json!({"type":"object","properties":{}}),
            |schema| schema.as_value().clone(),
        )
    };
    result.push((
        NativeDefinition {
            name: name.clone(),
            description,
            input_schema: schema,
            strict: None,
            defer_loading: false,
        },
        name,
        freeform,
    ));
    Ok(())
}

fn native_content(body: ToolOutputBody) -> std::result::Result<ToolResultContent, String> {
    let ToolOutputBody::Content(items) = body else {
        let ToolOutputBody::Text(text) = body else {
            unreachable!()
        };
        return Ok(ToolResultContent::Text(text));
    };
    let mut blocks = Vec::with_capacity(items.len());
    for item in items {
        blocks.push(match item {
            ToolOutputContent::InputText { text } => json!({"type":"text","text":text}),
            ToolOutputContent::InputImage { image_url, detail } => {
                let source = if let Some(data) = image_url.strip_prefix("data:") {
                    let (_, data) = data
                        .split_once(";base64,")
                        .ok_or("invalid image data URL")?;
                    let (data, media_type) =
                        match prepare_base64_image(data, detail, MAX_TOOL_IMAGE_DIMENSION) {
                            Ok(image) => image,
                            Err(reason) => {
                                blocks.push(json!({"type":"text","text":reason}));
                                continue;
                            }
                        };
                    json!({"type":"base64","media_type":media_type,"data":data})
                } else if image_url.starts_with("https://") {
                    json!({"type":"url","url":image_url})
                } else {
                    return Err("unsupported Claude image URL".into());
                };
                json!({"type":"image","source":source})
            }
            ToolOutputContent::InputAudio { .. } | ToolOutputContent::EncryptedContent { .. } => {
                return Err("Claude Messages cannot accept this tool media type".into());
            }
        });
    }
    Ok(ToolResultContent::Blocks(blocks))
}

struct Observer<'a> {
    bridge: &'a Bridge,
    error: Option<String>,
}
impl CodeModeObserver for Observer<'_> {
    fn update(&mut self, update: CodeModeUpdate<'_>) {
        let (kind, payload) = match update {
            CodeModeUpdate::NestedCallStarted {
                call_id,
                name,
                input,
            } => (
                AgentEventKind::ToolCall,
                json!({"call_id":call_id,"tool":name,"arguments":input,"model_call_index":0}),
            ),
            CodeModeUpdate::NestedCallCompleted(call) => (
                AgentEventKind::ToolResult,
                json!({"call_id":call.call_id,"tool":call.name,"status":if call.success {"completed"} else {"failed"},"result":call.output,"structured_result":call.structured_result,"metadata":call.metadata,"duration_ns":call.duration_ns,"started_after_ns":call.started_after_ns}),
            ),
        };
        if let Err(error) = self.bridge.emit(kind, payload) {
            self.error = Some(error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::config::Speed, core::set_speed};
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, header},
        routing::post,
    };
    use nanocodex::{
        ClaudeModel, Tool,
        tools::contract::{ToolContext, ToolOutput, ToolResult, async_trait},
    };
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    use tokio::{sync::mpsc, task::JoinHandle, time::timeout};

    type ResponseFactory = Box<
        dyn Fn(Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>
            + Send
            + Sync,
    >;
    struct ServerState {
        responses: Mutex<VecDeque<ResponseFactory>>,
        seen: mpsc::UnboundedSender<Value>,
        headers: mpsc::UnboundedSender<HeaderMap>,
    }
    struct Server {
        endpoint: String,
        requests: mpsc::UnboundedReceiver<Value>,
        headers: mpsc::UnboundedReceiver<HeaderMap>,
        task: JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server(responses: Vec<ResponseFactory>) -> Server {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (seen, requests) = mpsc::unbounded_channel();
        let (headers, received_headers) = mpsc::unbounded_channel();
        let state = Arc::new(ServerState {
            responses: Mutex::new(responses.into()),
            seen,
            headers,
        });
        let router = Router::new()
            .route(
                "/v1/messages",
                post(
                    |State(state): State<Arc<ServerState>>,
                     headers: HeaderMap,
                     Json(body): Json<Value>| async move {
                        assert_eq!(headers.get("x-api-key").unwrap(), "fixture-key");
                        assert!(!headers.contains_key("authorization"));
                        let _ = state.headers.send(headers);
                        let _ = state.seen.send(body.clone());
                        let model = body["model"].as_str().unwrap().to_owned();
                        let next = state.responses.lock().unwrap().pop_front();
                        let content = match next {
                            Some(next) => next(body).await,
                            None => std::future::pending::<Value>().await,
                        };
                        (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            sse(content, &model),
                        )
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Server {
            endpoint,
            requests,
            headers: received_headers,
            task,
        }
    }

    fn sse(block: Value, model: &str) -> String {
        let tool = block["type"] == "tool_use";
        let usage = if tool {
            json!({"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30,"output_tokens":0})
        } else {
            json!({"input_tokens":15,"cache_read_input_tokens":25,"cache_creation_input_tokens":35,"output_tokens":0})
        };
        let mut output = String::new();
        for event in [
            json!({"type":"message_start","message":{"id":"fixture-message","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":usage}}),
            json!({"type":"content_block_start","index":0,"content_block":block}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":if tool {2} else {3}}}),
            json!({"type":"message_stop"}),
        ] {
            output.push_str(&format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap(),
                event
            ));
        }
        output
    }
    fn tool(id: &str, name: &str, input: Value) -> ResponseFactory {
        let block = json!({"type":"tool_use","id":id,"name":name,"input":input});
        Box::new(move |_| {
            let block = block.clone();
            Box::pin(async move { block })
        })
    }
    fn final_text() -> ResponseFactory {
        Box::new(|_| Box::pin(async { json!({"type":"text","text":"finished"}) }))
    }
    fn agent(server: &Server, workspace: &Path, tools: Tools) -> (Nanocodex, AgentEvents) {
        build_client(
            ClaudeClient::new(reqwest::Client::new(), &server.endpoint, "fixture-key"),
            AgentContext {
                model: Model::Claude(nanocodex::ClaudeModel::Opus55),
                thinking: Thinking::Medium,
            },
            workspace,
            Arc::from("test instructions"),
            ClaudeSession {
                session_id: Some("claude-fixture"),
                snapshot: None,
                ..ClaudeSession::default()
            },
            ToolRuntime::new_with_tools(workspace, None, None, &tools),
            None,
        )
        .unwrap()
    }
    fn tools() -> nanocodex::tools::ToolsBuilder {
        Tools::builder().web_search(false).image_generation(false)
    }

    #[tokio::test]
    async fn consecutive_claude_turns_preserve_transcript_replies() {
        use crate::tui::transcript::{EntryKind, TranscriptModel, TranscriptRecord};

        let responses = ["first reply", "second reply"]
            .into_iter()
            .map(|text| {
                let response: ResponseFactory =
                    Box::new(move |_| Box::pin(async move { json!({"type":"text","text":text}) }));
                response
            })
            .collect();
        let server = server(responses).await;
        let workspace = tempfile::tempdir().unwrap();
        let (agent, mut events) = agent(&server, workspace.path(), tools().build().unwrap());
        let mut live = TranscriptModel::default();
        let mut replay = TranscriptModel::default();
        let mut sequence = 0;
        for _ in 0..2 {
            timeout(Duration::from_secs(10), agent.prompt("test").await.unwrap())
                .await
                .unwrap()
                .unwrap();
            loop {
                let event = timeout(Duration::from_secs(10), events.next())
                    .await
                    .unwrap()
                    .unwrap();
                let terminal = event.kind == AgentEventKind::RunCompleted;
                let delta = event.kind == AgentEventKind::AssistantDelta;
                sequence += 1;
                let record = TranscriptRecord::from_agent(sequence, sequence, event);
                live.apply(&record);
                if !delta {
                    let encoded = serde_json::to_string(&record).unwrap();
                    replay.apply(&serde_json::from_str(&encoded).unwrap());
                }
                if terminal {
                    break;
                }
            }
        }
        agent.shutdown().await.unwrap();
        for model in [&live, &replay] {
            let replies = model
                .entries()
                .iter()
                .filter_map(|entry| match &entry.kind {
                    EntryKind::Assistant {
                        text,
                        complete: true,
                    } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(replies, ["first reply", "second reply"]);
        }
    }

    fn fast_agent(
        endpoint: &str,
        workspace: &Path,
        model: ClaudeModel,
        session: ClaudeSession<'_>,
        spawn: Option<CleanAgentFactory>,
    ) -> (Nanocodex, AgentEvents) {
        let selected = tools().build().unwrap();
        build_client(
            ClaudeClient::new(reqwest::Client::new(), endpoint, "fixture-key"),
            AgentContext {
                model: Model::Claude(model),
                thinking: Thinking::Medium,
            },
            workspace,
            Arc::from("test instructions"),
            session,
            ToolRuntime::new_with_tools(workspace, None, None, &selected),
            spawn,
        )
        .unwrap()
    }

    async fn assert_fast_request(server: &mut Server, agent: &Nanocodex, expected: bool) -> Value {
        timeout(
            Duration::from_secs(10),
            agent.prompt("fast fixture").await.unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_fast_wire(server, expected).await
    }

    async fn assert_fast_wire(server: &mut Server, expected: bool) -> Value {
        let request = server.requests.recv().await.unwrap();
        let headers = server.headers.recv().await.unwrap();
        assert_eq!(request.get("speed"), expected.then_some(&json!("fast")));
        let beta = headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(',').any(|beta| beta == "fast-mode-2026-02-01"));
        assert_eq!(beta, expected);
        request
    }

    #[tokio::test]
    async fn fast_mode_initial_toggle_and_restore_follow_selected_model() {
        for model in [
            ClaudeModel::Sonnet55,
            ClaudeModel::Opus55,
            ClaudeModel::Fable51,
        ] {
            let mut server = server((0..5).map(|_| final_text()).collect()).await;
            let workspace = tempfile::tempdir().unwrap();
            let supported = model == ClaudeModel::Opus55;
            let (original, _) = fast_agent(
                &server.endpoint,
                workspace.path(),
                model,
                ClaudeSession {
                    fast_mode: true,
                    ..ClaudeSession::default()
                },
                None,
            );
            let request = assert_fast_request(&mut server, &original, supported).await;
            assert_eq!(request["model"], model.as_str());
            set_speed(&original, Model::Claude(model), Speed::Ultrafast)
                .await
                .unwrap();
            assert_fast_request(&mut server, &original, supported).await;
            set_speed(&original, Model::Claude(model), Speed::Standard)
                .await
                .unwrap();
            assert_fast_request(&mut server, &original, false).await;
            let snapshot =
                AgentSnapshot::from_claude(original.runtime_snapshot().await.unwrap()).unwrap();
            original.shutdown().await.unwrap();
            let (restored, _) = fast_agent(
                &server.endpoint,
                workspace.path(),
                model,
                ClaudeSession {
                    snapshot: Some(snapshot),
                    fast_mode: true,
                    ..ClaudeSession::default()
                },
                None,
            );
            let resumed = assert_fast_request(&mut server, &restored, supported).await;
            assert!(resumed["messages"].as_array().unwrap().len() > 1);
            let snapshot =
                AgentSnapshot::from_claude(restored.runtime_snapshot().await.unwrap()).unwrap();
            restored.shutdown().await.unwrap();
            let (standard, _) = fast_agent(
                &server.endpoint,
                workspace.path(),
                model,
                ClaudeSession {
                    snapshot: Some(snapshot),
                    fast_mode: false,
                    ..ClaudeSession::default()
                },
                None,
            );
            assert_fast_request(&mut server, &standard, false).await;
            standard.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn fast_mode_toggle_during_active_turn_applies_to_successor() {
        let release = Arc::new(Notify::new());
        let gate = Arc::clone(&release);
        let held: ResponseFactory = Box::new(move |_| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                gate.notified().await;
                json!({"type":"text","text":"finished"})
            })
        });
        let mut server = server(vec![held, final_text()]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (agent, _) = fast_agent(
            &server.endpoint,
            workspace.path(),
            ClaudeModel::Opus55,
            ClaudeSession {
                fast_mode: true,
                ..ClaudeSession::default()
            },
            None,
        );
        let active = agent.prompt("held fast turn").await.unwrap();
        timeout(Duration::from_secs(10), assert_fast_wire(&mut server, true))
            .await
            .unwrap();
        timeout(Duration::from_secs(10), agent.set_fast_mode(false))
            .await
            .unwrap()
            .unwrap();
        release.notify_one();
        timeout(Duration::from_secs(10), active)
            .await
            .unwrap()
            .unwrap();
        assert_fast_request(&mut server, &agent, false).await;
        agent.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn clean_spawn_inherits_live_fast_mode_without_conversation() {
        let mut server = server((0..3).map(|_| final_text()).collect()).await;
        let workspace = tempfile::tempdir().unwrap();
        let endpoint = server.endpoint.clone();
        let child_workspace = workspace.path().to_path_buf();
        let model = ClaudeModel::Opus55;
        let spawn: CleanAgentFactory = Arc::new(move |context, fast_mode| {
            let Model::Claude(model) = context.model else {
                unreachable!()
            };
            Ok(fast_agent(
                &endpoint,
                &child_workspace,
                model,
                ClaudeSession {
                    fast_mode,
                    ..ClaudeSession::default()
                },
                None,
            ))
        });
        let (parent, _) = fast_agent(
            &server.endpoint,
            workspace.path(),
            model,
            ClaudeSession::default(),
            Some(spawn),
        );
        assert_fast_request(&mut server, &parent, false).await;
        for enabled in [true, false] {
            parent.set_fast_mode(enabled).await.unwrap();
            let (child, _) = parent.spawn().await.unwrap();
            assert_ne!(child.session_id(), parent.session_id());
            let request = assert_fast_request(&mut server, &child, enabled).await;
            assert_eq!(request["messages"].as_array().unwrap().len(), 1);
            child.shutdown().await.unwrap();
        }
        parent.shutdown().await.unwrap();
    }

    struct Inspect(mpsc::UnboundedSender<(String, String, String, String, Option<u64>)>);
    #[async_trait]
    impl Tool for Inspect {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function(
                "inspect",
                "Inspect fixture",
                json!({"type":"object","properties":{}}),
            )
        }
        async fn execute(&self, _: ToolInput, context: ToolContext<'_>) -> ToolResult {
            self.0
                .send((
                    context.model().into(),
                    context.session_id().into(),
                    context.turn_id().unwrap().into(),
                    context.call_id().into(),
                    context.instruction_revision(),
                ))
                .unwrap();
            Ok(ToolOutput::text("visible fixture").with_structured_result(json!({"exact":7})))
        }
    }

    #[tokio::test]
    async fn native_model_windows_reach_tact_snapshots() {
        let server = server(vec![]).await;
        let workspace = tempfile::tempdir().unwrap();
        for model in [
            nanocodex::ClaudeModel::Sonnet55,
            nanocodex::ClaudeModel::Opus55,
            nanocodex::ClaudeModel::Fable51,
        ] {
            let selected = tools().build().unwrap();
            let (agent, _) = build_client(
                ClaudeClient::new(reqwest::Client::new(), &server.endpoint, "fixture-key"),
                AgentContext {
                    model: Model::Claude(model),
                    thinking: Thinking::Medium,
                },
                workspace.path(),
                Arc::from("test instructions"),
                ClaudeSession::default(),
                ToolRuntime::new_with_tools(workspace.path(), None, None, &selected),
                None,
            )
            .unwrap();
            let snapshot =
                AgentSnapshot::from_claude(agent.runtime_snapshot().await.unwrap()).unwrap();
            let budget = snapshot.context_budget().unwrap();
            assert_eq!(budget.window_tokens, 1_000_000, "{model:?}");
            assert_eq!(budget.active_tokens, 0);
            agent.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn automatic_compaction_preserves_code_mode_continuation_and_active_count() {
        let mut server = server(vec![
            tool(
                "before-compact",
                "exec",
                json!({"code":"text('large receipt '.repeat(1000));"}),
            ),
            Box::new(|_| {
                Box::pin(async { json!({"type":"text","text":"compacted fixture summary"}) })
            }),
            tool(
                "after-compact",
                "exec",
                json!({"code":"text(await tools.inspect({}));"}),
            ),
            final_text(),
        ])
        .await;
        let workspace = tempfile::tempdir().unwrap();
        let client = ClaudeClient::new(reqwest::Client::new(), &server.endpoint, "fixture-key");
        let (seed, _) = Nanocodex::builder(Claude::new(
            client.clone(),
            nanocodex::ClaudeModel::Opus55.as_str(),
        ))
        .session_id("compaction-fixture")
        .context_window_tokens(1_000)
        .build()
        .unwrap();
        let snapshot = AgentSnapshot::from_claude(seed.runtime_snapshot().await.unwrap()).unwrap();
        seed.shutdown().await.unwrap();
        let (tx, mut calls) = mpsc::unbounded_channel();
        let selected = tools().tool(Inspect(tx)).build().unwrap();
        let (agent, _) = build_client(
            client,
            AgentContext {
                model: Model::Claude(nanocodex::ClaudeModel::Opus55),
                thinking: Thinking::Medium,
            },
            workspace.path(),
            Arc::from("test instructions"),
            ClaudeSession {
                session_id: Some("compaction-fixture"),
                snapshot: Some(snapshot),
                ..ClaudeSession::default()
            },
            ToolRuntime::new_with_tools(workspace.path(), None, None, &selected),
            None,
        )
        .unwrap();
        let result = timeout(
            Duration::from_secs(20),
            agent.prompt("exercise compaction").await.unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.final_message(), "finished");
        let (_, session, _, call, _) = calls.recv().await.unwrap();
        assert_eq!(session, "compaction-fixture");
        assert!(call.starts_with("after-compact/"));
        let first = server.requests.recv().await.unwrap();
        assert_eq!(first["max_tokens"], 128_000);
        assert_eq!(
            first["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        let summary = server.requests.recv().await.unwrap();
        assert!(
            summary["messages"].as_array().unwrap().last().unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Summarize the conversation")
        );
        let continued = server.requests.recv().await.unwrap();
        let messages = continued["messages"].as_array().unwrap();
        assert!(
            messages[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("compacted fixture summary")
        );
        assert!(messages.iter().any(|message| {
            message["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|block| block["type"] == "tool_use" && block["id"] == "before-compact")
        }));
        assert!(
            messages.iter().any(|message| message["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|block| block["type"] == "tool_result"
                    && block["tool_use_id"] == "before-compact"))
        );
        let final_request = server.requests.recv().await.unwrap();
        assert!(
            final_request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|block| block["type"] == "tool_result"
                        && block["tool_use_id"] == "after-compact"))
        );
        let snapshot = AgentSnapshot::from_claude(agent.runtime_snapshot().await.unwrap()).unwrap();
        let budget = snapshot.context_budget().unwrap();
        assert_eq!(budget.window_tokens, 1_000);
        assert_eq!(budget.active_tokens, 15 + 25 + 35 + 3);
        assert_eq!(
            result.usage().unwrap().total_tokens(),
            2 * (10 + 20 + 30 + 2) + 2 * budget.active_tokens
        );
        agent.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_releases_code_mode_tools() {
        let server = server(vec![]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (sender, mut calls) = mpsc::unbounded_channel();
        let (agent, events) = agent(
            &server,
            workspace.path(),
            tools().tool(Inspect(sender)).build().unwrap(),
        );
        agent.shutdown().await.unwrap();
        drop(agent);
        drop(events);
        assert!(
            timeout(Duration::from_secs(1), calls.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn code_mode_roundtrip_preserves_identity_events_and_media() {
        let mut server = server(vec![tool("call-exec", "exec", json!({"code":"text(await tools.inspect({})); image('data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGMQjD0JAAG6ATiGpB8nAAAAAElFTkSuQmCC');"})), final_text()]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (tx, mut calls) = mpsc::unbounded_channel();
        let (agent, mut events) = agent(
            &server,
            workspace.path(),
            tools().tool(Inspect(tx)).build().unwrap(),
        );
        let turn = agent
            .prompt(nanocodex::oai::Prompt::new("exercise tools").with_instruction_revision(17))
            .await
            .unwrap();
        let id = turn.id().to_owned();
        let result = timeout(Duration::from_secs(20), turn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.final_message(), "finished");
        let (model, session, turn, call, revision) = calls.recv().await.unwrap();
        assert_eq!(model, nanocodex::ClaudeModel::Opus55.as_str());
        assert_eq!(session, agent.session_id());
        assert_eq!(turn, id);
        assert!(call.starts_with("call-exec/"));
        assert_eq!(revision, Some(17));
        let first = server.requests.recv().await.unwrap();
        assert_eq!(first["max_tokens"], 128_000);
        assert_eq!(
            first["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        assert_eq!(
            first["tools"][0]["input_schema"]["properties"]["code"]["type"],
            "string"
        );
        let second = server.requests.recv().await.unwrap();
        assert_eq!(second["cache_control"], first["cache_control"]);
        assert_eq!(second["system"], first["system"]);
        let content =
            &second["messages"].as_array().unwrap().last().unwrap()["content"][0]["content"];
        assert!(
            content
                .as_array()
                .unwrap()
                .iter()
                .any(|block| block["type"] == "image")
        );
        let mut kinds = Vec::new();
        while let Some(event) = events.recv().await {
            let value: Value = serde_json::from_str(event.payload.get()).unwrap();
            assert_eq!(value["turn_id"], id);
            if value["tool"] == "inspect" && event.kind == AgentEventKind::ToolResult {
                assert_eq!(value["structured_result"], json!({"exact":7}));
            }
            kinds.push((event.kind, value["call_id"].clone()));
            if event.kind.is_terminal() {
                break;
            }
        }
        let parent = kinds
            .iter()
            .position(|(kind, id)| *kind == AgentEventKind::ToolCall && *id == "call-exec")
            .unwrap();
        let nested = kinds
            .iter()
            .position(|(kind, id)| *kind == AgentEventKind::ToolCall && *id == call)
            .unwrap();
        assert!(parent < nested);
        agent.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restore_applies_effort_and_caching_after_native_checkpoint() {
        let mut server = server(vec![final_text(), final_text()]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (original, _) = Nanocodex::builder(Claude::new(
            ClaudeClient::new(reqwest::Client::new(), &server.endpoint, "fixture-key"),
            nanocodex::ClaudeModel::Opus55.as_str(),
        ))
        .system("test instructions")
        .thinking(Thinking::Medium)
        .unwrap()
        .build()
        .unwrap();
        timeout(
            Duration::from_secs(20),
            original.prompt("first").await.unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        let snapshot =
            AgentSnapshot::from_claude(original.runtime_snapshot().await.unwrap()).unwrap();
        original.shutdown().await.unwrap();
        let selected = tools().build().unwrap();
        let (restored, _) = build_client(
            ClaudeClient::new(reqwest::Client::new(), &server.endpoint, "fixture-key"),
            AgentContext {
                model: Model::Claude(nanocodex::ClaudeModel::Opus55),
                thinking: Thinking::High,
            },
            workspace.path(),
            Arc::from("test instructions"),
            ClaudeSession {
                session_id: Some(original.session_id()),
                snapshot: Some(snapshot),
                ..ClaudeSession::default()
            },
            ToolRuntime::new_with_tools(workspace.path(), None, None, &selected),
            None,
        )
        .unwrap();
        timeout(
            Duration::from_secs(20),
            restored.prompt("second").await.unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        let first = server.requests.recv().await.unwrap();
        assert_eq!(first["output_config"]["effort"], "medium");
        assert!(first.get("cache_control").is_none());
        let resumed = server.requests.recv().await.unwrap();
        assert_eq!(resumed["output_config"]["effort"], "high");
        assert_eq!(resumed["max_tokens"], 128_000);
        assert_eq!(
            resumed["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        assert_eq!(resumed["system"][0]["text"], first["system"]);
        assert_eq!(
            resumed["system"][0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        assert!(resumed["messages"].as_array().unwrap().len() > 1);
        restored.shutdown().await.unwrap();
    }

    struct BufferedReceipt(Arc<Notify>);

    #[async_trait]
    impl Tool for BufferedReceipt {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function(
                "receipt",
                "Return a gated receipt",
                json!({"type":"object","properties":{}}),
            )
        }
        async fn execute(&self, _: ToolInput, _: ToolContext<'_>) -> ToolResult {
            self.0.notified().await;
            Ok(ToolOutput::text("known completion").with_structured_result(json!({"exact":7})))
        }
    }

    #[tokio::test]
    async fn context_usage_updates_before_code_mode_turn_finishes() {
        use crate::tui::transcript::TranscriptRecord;
        use nanocodex::oai::events::ModelCallCompleted;

        let server = server(vec![
            tool(
                "context-round",
                "exec",
                json!({"code":"text(await tools.receipt({}));"}),
            ),
            final_text(),
        ])
        .await;
        let workspace = tempfile::tempdir().unwrap();
        let release = Arc::new(Notify::new());
        let (agent, mut events) = agent(
            &server,
            workspace.path(),
            tools()
                .tool(BufferedReceipt(release.clone()))
                .build()
                .unwrap(),
        );
        let turn = agent.prompt("update usage while tools run").await.unwrap();
        let first_usage = timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                assert!(!event.kind.is_terminal());
                if event.kind == AgentEventKind::ModelCallCompleted {
                    let record = TranscriptRecord::from_agent(event.seq, 0, event);
                    return record
                        .decode_payload::<ModelCallCompleted>()
                        .unwrap()
                        .usage
                        .unwrap();
                }
            }
            panic!("event stream closed before context update");
        })
        .await;
        release.notify_one();
        let result = timeout(Duration::from_secs(20), turn)
            .await
            .unwrap()
            .unwrap();
        let mut usage = first_usage.expect("context must update before the tool completes");
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (60, 2, 62)
        );
        assert_eq!(
            usage.input_tokens_details.as_ref().unwrap().cached_tokens,
            20
        );
        while let Some(event) = events.recv().await {
            let terminal = event.kind.is_terminal();
            if event.kind == AgentEventKind::ModelCallCompleted {
                let record = TranscriptRecord::from_agent(event.seq, 0, event);
                usage = record
                    .decode_payload::<ModelCallCompleted>()
                    .unwrap()
                    .usage
                    .unwrap();
            }
            if terminal {
                break;
            }
        }
        assert_eq!(usage.total_tokens, 78);
        assert_eq!(result.usage().unwrap().total_tokens(), 62 + 78);
        agent.shutdown().await.unwrap();
    }

    async fn steering_acknowledgments(terminal_response: bool) {
        let release = Arc::new(Notify::new());
        let first_response: ResponseFactory = if terminal_response {
            let release = release.clone();
            Box::new(move |_| {
                let release = release.clone();
                Box::pin(async move {
                    release.notified().await;
                    json!({"type":"text","text":"initial answer"})
                })
            })
        } else {
            tool(
                "steering-exec",
                "exec",
                json!({"code":"text(await tools.receipt({}));"}),
            )
        };
        let mut server = server(vec![first_response, final_text()]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (agent, mut events) = agent(
            &server,
            workspace.path(),
            tools()
                .tool(BufferedReceipt(release.clone()))
                .build()
                .unwrap(),
        );
        let turn = agent.prompt("start the steering fixture").await.unwrap();
        timeout(Duration::from_secs(10), server.requests.recv())
            .await
            .unwrap()
            .unwrap();
        let mut observed = Vec::new();
        if !terminal_response {
            timeout(Duration::from_secs(10), async {
                loop {
                    let event = events.recv().await.unwrap();
                    let payload: Value = serde_json::from_str(event.payload.get()).unwrap();
                    let entered =
                        event.kind == AgentEventKind::ToolCall && payload["tool"] == "receipt";
                    observed.push(event);
                    if entered {
                        break;
                    }
                }
            })
            .await
            .unwrap();
        }
        let instructions = if terminal_response {
            vec!["correction during terminal response"]
        } else {
            vec![
                "first correction during nested tool",
                "second correction during nested tool",
            ]
        };
        for instruction in &instructions {
            turn.control().steer(*instruction).await.unwrap();
        }
        release.notify_one();
        timeout(Duration::from_secs(20), turn)
            .await
            .unwrap()
            .unwrap();
        let continued = timeout(Duration::from_secs(10), server.requests.recv())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                let terminal = event.kind.is_terminal();
                observed.push(event);
                if terminal {
                    break;
                }
            }
        })
        .await
        .unwrap();
        agent.shutdown().await.unwrap();

        let delivered = continued["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .flat_map(|message| message["content"].as_array().unwrap())
            .filter_map(|block| block["text"].as_str())
            .filter(|text| instructions.contains(text))
            .collect::<Vec<_>>();
        assert_eq!(
            delivered, instructions,
            "every steer must reach the next request once, in order"
        );
        let acknowledgments = observed
            .iter()
            .enumerate()
            .filter(|(_, event)| event.kind == AgentEventKind::RunSteered)
            .collect::<Vec<_>>();
        assert_eq!(
            acknowledgments.len(),
            instructions.len(),
            "delivered steers must acknowledge the UI queue"
        );
        let next_response = observed
            .iter()
            .enumerate()
            .filter(|(_, event)| event.kind == AgentEventKind::ModelCallCompleted)
            .nth(1)
            .unwrap()
            .0;
        for (index, ((position, event), instruction)) in
            acknowledgments.iter().zip(&instructions).enumerate()
        {
            let payload: Value = serde_json::from_str(event.payload.get()).unwrap();
            assert_eq!(payload["steer_index"], index + 1);
            assert_eq!(payload["instruction_bytes"], instruction.len());
            assert!(*position < next_response);
            if !terminal_response {
                let callback_completed = observed
                    .iter()
                    .position(|event| {
                        let payload: Value = serde_json::from_str(event.payload.get()).unwrap();
                        event.kind == AgentEventKind::ToolResult
                            && payload["call_id"] == "steering-exec"
                    })
                    .unwrap();
                assert!(
                    *position > callback_completed,
                    "nested tools are not model boundaries"
                );
            }
        }
    }

    #[tokio::test]
    async fn steering_acknowledges_each_prompt_after_code_mode_callback() {
        steering_acknowledgments(false).await;
    }

    #[tokio::test]
    async fn steering_acknowledges_prompt_admitted_during_terminal_response() {
        steering_acknowledgments(true).await;
    }

    async fn buffered_completion_receipt(cancel: bool) {
        let release = Arc::new(Notify::new());
        let response_release = release.clone();
        let (entered, entered_rx) = mpsc::unbounded_channel();
        let (buffered, mut buffered_rx) = mpsc::unbounded_channel();
        let observed = Arc::new(tokio::sync::Mutex::new(entered_rx));
        let server = server(vec![
            tool("buffered-exec", "exec", json!({"code":"// @exec: {\"yield_time_ms\": 1}\nawait tools.receipt({}); await tools.block({});"})),
            Box::new(move |_| {
                let release = response_release.clone();
                let observed = observed.clone();
                let buffered = buffered.clone();
                Box::pin(async move {
                    // The second request proves exec yielded before the receipt finishes.
                    release.notify_one();
                    observed.lock().await.recv().await.unwrap();
                    // block starts only after JavaScript consumed the completed receipt.
                    buffered.send(()).unwrap();
                    if cancel { std::future::pending::<()>().await; }
                    json!({"type":"text","text":"finished"})
                })
            }),
        ]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (agent, mut events) = agent(
            &server,
            workspace.path(),
            tools()
                .tool(BufferedReceipt(release))
                .tool(Block {
                    entered,
                    dropped: Arc::new(AtomicBool::new(false)),
                })
                .build()
                .unwrap(),
        );
        let turn = agent.prompt("leave a buffered completion").await.unwrap();
        timeout(Duration::from_secs(10), buffered_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if cancel {
            turn.control().cancel().await.unwrap();
        }
        let result = turn.await;
        assert_eq!(result.is_err(), cancel);
        let mut receipt = None;
        while let Some(event) = events.recv().await {
            let payload: Value = serde_json::from_str(event.payload.get()).unwrap();
            if event.kind == AgentEventKind::ToolResult && payload["tool"] == "receipt" {
                receipt = Some(payload["structured_result"].clone());
            }
            if event.kind.is_terminal() {
                break;
            }
        }
        agent.shutdown().await.unwrap();
        assert_eq!(
            receipt,
            Some(json!({"exact":7})),
            "completed nested receipt was dropped at terminal cleanup"
        );
    }

    #[tokio::test]
    async fn buffered_completion_survives_success_cleanup() {
        buffered_completion_receipt(false).await;
    }

    #[tokio::test]
    async fn buffered_completion_survives_cancellation() {
        buffered_completion_receipt(true).await;
    }

    #[tokio::test]
    async fn shutdown_cancels_stalled_compaction() {
        let mut server = server(vec![final_text()]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (agent, _) = agent(&server, workspace.path(), tools().build().unwrap());
        agent
            .prompt("establish history")
            .await
            .unwrap()
            .await
            .unwrap();
        server.requests.recv().await.unwrap();
        let compact_agent = agent.clone();
        let compact = tokio::spawn(async move { compact_agent.compact().await });
        timeout(Duration::from_secs(10), server.requests.recv())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(3), agent.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(
            timeout(Duration::from_secs(3), compact)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_event_allows_immediate_successor() {
        let server = server((0..32).map(|_| final_text()).collect()).await;
        let workspace = tempfile::tempdir().unwrap();
        let (agent, mut events) = agent(&server, workspace.path(), tools().build().unwrap());
        let mut turns = Vec::new();
        for _ in 0..32 {
            turns.push(agent.prompt("next").await.unwrap());
            timeout(Duration::from_secs(10), async {
                while let Some(event) = events.next().await {
                    if event.kind.is_terminal() {
                        return;
                    }
                }
                panic!("missing terminal event");
            })
            .await
            .unwrap();
        }
        for turn in turns {
            turn.await.unwrap();
        }
        agent.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn yielded_code_resumes_through_wait() {
        let mut server = server(vec![
            tool("yield-exec", "exec", json!({"code":"// @exec: {\"yield_time_ms\": 1}\nawait new Promise(resolve => setTimeout(resolve, 100)); text('resumed');"})),
            tool("wait-exec", "wait", json!({"cell_id":"1","yield_time_ms":1000})), final_text(),
        ]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (agent, _) = agent(&server, workspace.path(), tools().build().unwrap());
        timeout(
            Duration::from_secs(20),
            agent.prompt("yield then wait").await.unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        let _ = server.requests.recv().await.unwrap();
        let yielded = server.requests.recv().await.unwrap();
        assert!(yielded.to_string().contains("Script running with cell ID"));
        let resumed = server.requests.recv().await.unwrap();
        assert!(resumed.to_string().contains("resumed"));
        agent.shutdown().await.unwrap();
    }

    struct Block {
        entered: mpsc::UnboundedSender<()>,
        dropped: Arc<AtomicBool>,
    }
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    #[async_trait]
    impl Tool for Block {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function(
                "block",
                "Block fixture",
                json!({"type":"object","properties":{}}),
            )
        }
        async fn execute(&self, _: ToolInput, _: ToolContext<'_>) -> ToolResult {
            let _dropped = Dropped(self.dropped.clone());
            self.entered.send(()).unwrap();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn successful_final_drains_yielded_producers_before_successor() {
        let (entered, entered_rx) = mpsc::unbounded_channel();
        let entered_rx = Arc::new(tokio::sync::Mutex::new(entered_rx));
        let server = server(vec![
            tool(
                "yielded-success",
                "exec",
                json!({"code":"// @exec: {\"yield_time_ms\": 1}\nawait tools.block({});"}),
            ),
            Box::new(move |_| {
                let entered = entered_rx.clone();
                Box::pin(async move {
                    entered.lock().await.recv().await.unwrap();
                    json!({"type":"text","text":"finished"})
                })
            }),
            final_text(),
        ])
        .await;
        let workspace = tempfile::tempdir().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let (agent, _) = agent(
            &server,
            workspace.path(),
            tools()
                .tool(Block {
                    entered,
                    dropped: dropped.clone(),
                })
                .build()
                .unwrap(),
        );
        let turn = agent.prompt("leave a yielded producer").await.unwrap();
        timeout(Duration::from_secs(20), turn)
            .await
            .unwrap()
            .unwrap();
        assert!(dropped.load(Ordering::Acquire));
        timeout(
            Duration::from_secs(20),
            agent.prompt("successor").await.unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        agent.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn interrupt_and_shutdown_drain_yielded_cells_and_shells() {
        for shutdown in [false, true] {
            let mut server = server(vec![tool("blocked-exec", "exec", json!({"code":"// @exec: {\"yield_time_ms\": 1}\nconst process = await tools.exec_command({cmd: \"sleep 1; echo $$ > fixture.pid; exec sleep 60\", yield_time_ms: 1}); text(process); await tools.block({});"}))]).await;
            let workspace = tempfile::tempdir().unwrap();
            let (entered, mut entered_rx) = mpsc::unbounded_channel();
            let dropped = Arc::new(AtomicBool::new(false));
            let (agent, mut events) = agent(
                &server,
                workspace.path(),
                tools()
                    .tool(Block {
                        entered,
                        dropped: dropped.clone(),
                    })
                    .build()
                    .unwrap(),
            );
            let turn = agent.prompt("block tools").await.unwrap();
            let control = turn.control();
            assert!(agent.prompt("overlap").await.is_err());
            timeout(Duration::from_secs(20), entered_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let pid = timeout(Duration::from_secs(20), async {
                loop {
                    match tokio::fs::read_to_string(workspace.path().join("fixture.pid")).await {
                        Ok(pid) if pid.ends_with('\n') => break pid,
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => panic!("could not read shell readiness marker: {error}"),
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("shell did not publish its PID");
            let mut probe = std::process::Command::new("kill");
            probe
                .args(["-0", pid.trim()])
                .stderr(std::process::Stdio::null());
            assert!(
                probe.status().unwrap().success(),
                "fixture shell exited early"
            );
            let _ = server.requests.recv().await.unwrap();
            timeout(Duration::from_secs(20), server.requests.recv())
                .await
                .unwrap()
                .unwrap();
            if shutdown {
                timeout(Duration::from_secs(20), agent.shutdown())
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                timeout(Duration::from_secs(20), control.cancel())
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert!(
                dropped.load(Ordering::Acquire),
                "cancel acknowledged before nested tool dropped"
            );
            assert!(
                !probe.status().unwrap().success(),
                "cancel acknowledged with shell still alive"
            );
            assert!(
                timeout(Duration::from_secs(20), turn)
                    .await
                    .unwrap()
                    .is_err()
            );
            while let Some(event) = events.recv().await {
                if event.kind.is_terminal() {
                    break;
                }
            }
            agent.shutdown().await.unwrap();
        }
    }
}
