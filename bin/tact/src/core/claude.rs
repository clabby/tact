use crate::{app::config::Config, tui::session::AgentSnapshot};
use futures_util::StreamExt;
use nanocodex::{
    AgentEvents, Claude, HarnessModel as Model, Nanocodex, NanocodexError, Thinking, Tools,
    agent::{AgentHandle, Result},
    claude::{
        ClaudeClient, ClaudeToolInvocation, ClaudeToolReply, ClaudeTools,
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
    Arc<dyn Fn(AgentContext) -> Result<(Nanocodex, AgentEvents)> + Send + Sync>;

#[derive(Default)]
pub(super) struct ClaudeSession<'a> {
    pub(super) session_id: Option<&'a str>,
    pub(super) snapshot: Option<AgentSnapshot>,
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
    builder = builder.thinking(thinking)?.max_tokens(128_000);
    let (native, native_events) = builder.build()?;
    // The wrapper consumes each native turn's mirrored stream instead.
    drop(native_events);
    Ok(lifecycle::wrap(native, bridge, spawn))
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
            let mut reply = tool_reply(execution.output, execution.success, None, structured);
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
        Ok(tool_reply(
            output.output,
            output.success,
            metadata,
            structured,
        ))
    }
}

fn tool_reply(
    output: ToolOutputBody,
    success: bool,
    metadata: Option<Value>,
    structured: Value,
) -> ClaudeToolReply {
    let (content, is_error) = match native_content(output) {
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
            ToolOutputContent::InputImage { image_url, .. } => {
                let source = if let Some(data) = image_url.strip_prefix("data:") {
                    let (media_type, data) = data
                        .split_once(";base64,")
                        .ok_or("invalid image data URL")?;
                    if !matches!(
                        media_type,
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    ) {
                        return Err("unsupported Claude image type".into());
                    }
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
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, header},
        routing::post,
    };
    use nanocodex::{
        Tool,
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
    }
    struct Server {
        endpoint: String,
        requests: mpsc::UnboundedReceiver<Value>,
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
        let state = Arc::new(ServerState {
            responses: Mutex::new(responses.into()),
            seen,
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
                        let _ = state.seen.send(body.clone());
                        let next = state.responses.lock().unwrap().pop_front();
                        let content = match next {
                            Some(next) => next(body).await,
                            None => std::future::pending::<Value>().await,
                        };
                        ([(header::CONTENT_TYPE, "text/event-stream")], sse(content))
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
            task,
        }
    }

    fn sse(block: Value) -> String {
        let tool = block["type"] == "tool_use";
        let mut output = String::new();
        for event in [
            json!({"type":"message_start","message":{"id":"fixture-message","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":block}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":2}}),
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
            },
            ToolRuntime::new_with_tools(workspace, None, None, &tools),
            None,
        )
        .unwrap()
    }
    fn tools() -> nanocodex::tools::ToolsBuilder {
        Tools::builder().web_search(false).image_generation(false)
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
        let mut server = server(vec![tool("call-exec", "exec", json!({"code":"text(await tools.inspect({})); image('data:image/png;base64,iVBORw0KGgo=');"})), final_text()]).await;
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
            first["tools"][0]["input_schema"]["properties"]["code"]["type"],
            "string"
        );
        let second = server.requests.recv().await.unwrap();
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
    async fn restore_applies_requested_effort_after_native_checkpoint() {
        let mut server = server(vec![final_text(), final_text()]).await;
        let workspace = tempfile::tempdir().unwrap();
        let (original, _) = agent(&server, workspace.path(), tools().build().unwrap());
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
                session_id: Some("claude-fixture"),
                snapshot: Some(snapshot),
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
        assert_eq!(
            server.requests.recv().await.unwrap()["output_config"]["effort"],
            "medium"
        );
        let resumed = server.requests.recv().await.unwrap();
        assert_eq!(resumed["output_config"]["effort"], "high");
        assert_eq!(resumed["max_tokens"], 128_000);
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
            let mut server = server(vec![tool("blocked-exec", "exec", json!({"code":"// @exec: {\"yield_time_ms\": 1}\nconst process = await tools.exec_command({cmd: \"echo $$ > fixture.pid; exec sleep 60\", yield_time_ms: 1}); text(process); await tools.block({});"}))]).await;
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
            let pid = std::fs::read_to_string(workspace.path().join("fixture.pid")).unwrap();
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
                !std::process::Command::new("kill")
                    .args(["-0", pid.trim()])
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap()
                    .success(),
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
