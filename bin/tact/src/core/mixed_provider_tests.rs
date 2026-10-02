use super::{AgentRecipe, claude, install_agent_tools};
use crate::app::{
    config::{Config, ConfigOverrides},
    secret::SecretString,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{any, post},
};
use nanocodex::{
    AgentEvents, ClaudeModel, HarnessModel, Model as CodexModel, Nanocodex, NanocodexError, OpenAi,
    Thinking, Tools,
    agent::{ChildSnapshot, SpawnOptions},
    oai::{
        ResponseError,
        events::AgentEventKind,
        responses::{ContentItem, MessageRole, ResponseItem, WarmupResponse},
        tower::{
            CodeCall, CodeCallKind, GenerationOutput, ResponsePipelineStats, ResponsesAttempt,
            ResponsesAttemptKind, ResponsesOutput, ResponsesServiceResponse,
        },
    },
};
use serde_json::{Value, json};
use std::{
    fs,
    future::{Ready, ready},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tact_subagents::{AgentContext, AgentStatus, AgentUpdate, Subagents};
use tempfile::tempdir;
use tokio::{net::TcpListener, sync::Notify, time::timeout};
use tower::Service;

#[derive(Clone)]
struct Script {
    model: HarnessModel,
    code: Arc<str>,
    thinking: Thinking,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Script {
    fn new(model: HarnessModel, code: String) -> Self {
        Self {
            model,
            code: code.into(),
            thinking: Thinking::Medium,
            requests: Arc::default(),
        }
    }

    fn next(&self, request: Value) -> bool {
        let invoke = !contains_tool_result(&request);
        self.requests.lock().unwrap().push(request);
        invoke
    }
}

fn contains_tool_result(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            matches!(
                fields.get("type").and_then(Value::as_str),
                Some("tool_result" | "custom_tool_call_output")
            ) || fields.values().any(contains_tool_result)
        }
        Value::Array(items) => items.iter().any(contains_tool_result),
        _ => false,
    }
}

#[derive(Clone)]
struct Provider {
    script: Script,
    parent_entered: Arc<Notify>,
    parent_release: Arc<Notify>,
}

impl Service<ResponsesAttempt> for Script {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
        assert_eq!(
            request.model(),
            match self.model {
                HarnessModel::Codex(model) => model,
                HarnessModel::Claude(_) => panic!("expected Codex"),
            }
        );
        assert_eq!(request.thinking(), Thinking::Medium);
        if matches!(request.kind(), ResponsesAttemptKind::Warmup) {
            return ready(Ok(ResponsesServiceResponse::new(ResponsesOutput::Warmup(
                WarmupResponse {
                    id: "warmup-mixed".into(),
                    usage: None,
                },
            ))));
        }
        let input = request
            .input_items()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect::<Vec<_>>();
        let invoke = self.next(json!(input));
        let (items, calls) = if invoke {
            (vec![serde_json::from_value(json!({
                "type":"custom_tool_call", "call_id":"mixed-exec", "name":"exec", "input": self.code,
            })).unwrap()], vec![CodeCall {
                call_id: "mixed-exec".into(), name: "exec".into(), namespace: None,
                input: self.code.to_string(), kind: CodeCallKind::Custom,
            }])
        } else {
            (
                vec![ResponseItem::message(
                    MessageRole::Assistant,
                    [ContentItem::output_text("finished")],
                )],
                Vec::new(),
            )
        };
        ready(Ok(ResponsesServiceResponse::new(
            ResponsesOutput::Generation(GenerationOutput {
                id: if invoke { "mixed-call" } else { "mixed-final" }.into(),
                reported_model: Some(self.model.as_str().into()),
                status: "completed".into(),
                end_turn: Some(!invoke),
                final_message: (!invoke).then(|| "finished".into()),
                output_items: items,
                code_calls: calls,
                usage: None,
                time_to_first_event_ns: 0,
                time_to_first_output_ns: None,
                pipeline_stats: ResponsePipelineStats::default(),
            }),
        )))
    }
}

async fn messages(
    State(provider): State<Provider>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    assert_eq!(headers["x-api-key"], "fixture-token");
    assert!(!headers.contains_key(header::AUTHORIZATION));
    if request["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .to_string()
        .contains("held-parent-task")
    {
        provider.parent_entered.notify_one();
        provider.parent_release.notified().await;
    }
    let script = &provider.script;
    let exec_name = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .find(|name| matches!(*name, "exec" | "_exec"))
        .unwrap()
        .to_owned();
    assert_eq!(request["model"], script.model.as_str());
    assert_eq!(
        request["output_config"]["effort"],
        script.thinking.to_string()
    );
    let invoke = script.next(request);
    let mut events = vec![json!({"type":"message_start", "message":{
        "id":"mixed-message", "type":"message", "role":"assistant", "model":script.model.as_str(),
        "content":[], "stop_reason":null, "usage":{"input_tokens":1,"output_tokens":0}
    }})];
    if invoke {
        events.extend([
            json!({"type":"content_block_start", "index":0, "content_block":{
                "type":"tool_use", "id":"mixed-exec", "name":exec_name, "input":{}
            }}),
            json!({"type":"content_block_delta", "index":0, "delta":{
                "type":"input_json_delta", "partial_json":json!({"code":script.code}).to_string()
            }}),
        ]);
    } else {
        events.extend([
            json!({"type":"content_block_start", "index":0, "content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta","text":"finished"}}),
        ]);
    }
    events.extend([
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"message_delta", "delta":{"stop_reason":if invoke {"tool_use"} else {"end_turn"}}, "usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]);
    let body = events
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn build_agent(
    context: AgentContext,
    recipe: &Arc<AgentRecipe>,
    codex: Script,
) -> Result<(Nanocodex, AgentEvents), NanocodexError> {
    let AgentContext { model, thinking } = context;
    let tools = install_agent_tools(
        recipe.tools.clone(),
        &recipe.subagents,
        None,
        recipe.config.subagents().enabled(),
        recipe.config.path().to_path_buf(),
    )?;
    if let HarnessModel::Codex(model) = model {
        let service = codex.clone();
        let openai = OpenAi::builder("fixture-token")
            .service(move || service.clone())
            .build()
            .map_err(|error| NanocodexError::InvalidRequest(error.to_string()))?;
        Nanocodex::builder(openai)
            .model(model)
            .thinking(thinking)
            .workspace(&recipe.workspace)
            .tools(tools)
            .build()
    } else {
        let client =
            recipe.claude_client(|| Ok(Some(SecretString::new("fixture-token".into()))))?;
        let clean_recipe = Arc::clone(recipe);
        let spawn: claude::CleanAgentFactory =
            Arc::new(move |context| build_agent(context, &clean_recipe, codex.clone()));
        claude::build_client(
            client,
            context,
            &recipe.workspace,
            Arc::from("Use the installed tools."),
            claude::ClaudeSession::default(),
            claude::tool_runtime(&recipe.config, &recipe.workspace, &tools)?,
            Some(spawn),
        )
    }
}

#[tokio::test]
async fn claude_root_runs_codex_child_through_code_mode() {
    mixed_provider_roundtrip(
        HarnessModel::Claude(ClaudeModel::Opus55),
        HarnessModel::Codex(CodexModel::Sol),
        1,
        false,
    )
    .await;
}

#[tokio::test]
async fn codex_root_runs_claude_child_through_code_mode() {
    mixed_provider_roundtrip(
        HarnessModel::Codex(CodexModel::Luna),
        HarnessModel::Claude(ClaudeModel::Fable51),
        1,
        false,
    )
    .await;
}

#[tokio::test]
async fn codex_root_runs_multiple_claude_children_through_code_mode() {
    mixed_provider_roundtrip(
        HarnessModel::Codex(CodexModel::Luna),
        HarnessModel::Claude(ClaudeModel::Fable51),
        2,
        false,
    )
    .await;
}

#[tokio::test]
async fn api_key_clean_spawn_keeps_parent_and_child_isolated() {
    mixed_provider_roundtrip(
        HarnessModel::Claude(ClaudeModel::Opus55),
        HarnessModel::Codex(CodexModel::Sol),
        2,
        true,
    )
    .await;
}

#[tokio::test]
async fn api_key_messages_reject_redirects_without_forwarding_credentials() {
    crate::install_tls_provider();
    for status in [StatusCode::FOUND, StatusCode::TEMPORARY_REDIRECT] {
        let requests = Arc::new(AtomicUsize::new(0));
        let redirected = Arc::new(AtomicUsize::new(0));
        let initial = requests.clone();
        let destination = redirected.clone();
        let router = Router::new()
            .route(
                "/messages",
                post(move |headers: HeaderMap| {
                    let requests = initial.clone();
                    async move {
                        assert_eq!(headers["x-api-key"], "fixture-token");
                        requests.fetch_add(1, Ordering::SeqCst);
                        (status, [(header::LOCATION, "/redirected")])
                    }
                }),
            )
            .route(
                "/redirected",
                any(move |headers: HeaderMap| {
                    let requests = destination.clone();
                    async move {
                        assert_eq!(headers["x-api-key"], "fixture-token");
                        requests.fetch_add(1, Ordering::SeqCst);
                        StatusCode::OK
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!("[claude]\nenabled = true\napi_base_url = {origin:?}\n"),
        )
        .unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(path),
            workspace: Some(directory.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let (subagents, _updates) = Subagents::new(1);
        let recipe = Arc::new(AgentRecipe {
            config,
            workspace: directory.path().to_owned(),
            memory: None,
            subagents: subagents.downgrade(),
            tools: Tools::builder()
                .web_search(false)
                .image_generation(false)
                .build()
                .unwrap(),
        });
        let model = HarnessModel::Claude(ClaudeModel::Opus55);
        let (agent, mut events) = build_agent(
            AgentContext {
                model,
                thinking: Thinking::Medium,
            },
            &recipe,
            Script::new(HarnessModel::Codex(CodexModel::Sol), String::new()),
        )
        .unwrap();
        let result = timeout(Duration::from_secs(5), async {
            agent
                .prompt("Do not follow provider redirects")
                .await?
                .result()
                .await
        })
        .await
        .expect("redirect request must terminate");
        let error = result.expect_err("redirect response must fail the turn");
        assert_no_credentials(&error.to_string());
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            redirected.load(Ordering::SeqCst),
            0,
            "redirect would forward x-api-key"
        );
        while let Some(event) = events.try_recv_timed() {
            assert_no_credentials(event.event.payload.get());
        }
        // The default transport follows this fixture's redirect and forwards the key.
        reqwest::Client::new()
            .post(format!("{origin}/messages"))
            .header("x-api-key", "fixture-token")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(redirected.load(Ordering::SeqCst), 1);
        agent.shutdown().await.unwrap();
        server.abort();
    }
}

fn assert_no_credentials(text: &str) {
    assert!(
        !text.contains("fixture-token"),
        "credential escaped into model or session data"
    );
}

async fn mixed_provider_roundtrip(
    root_model: HarnessModel,
    child_model: HarnessModel,
    child_count: usize,
    clean_spawn: bool,
) {
    crate::install_tls_provider();
    let directory = tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let config_path = directory.path().join("config.toml");
    fs::write(&config_path, format!("[agent]\nweb_search = false\nimage_generation = false\n[claude]\nenabled = true\napi_base_url = {origin:?}\n[subagents]\nenabled = true\n")).unwrap();
    let config = Config::load(ConfigOverrides {
        path: Some(config_path),
        auth_file: Some(directory.path().join("unused-auth.json")),
        workspace: Some(directory.path().to_owned()),
        ..ConfigOverrides::default()
    })
    .unwrap();
    let output = json!({"native_model":child_model.as_str(), "answer":42});
    let child_code = format!("text(await tools.submit_result({{turn_token:1,output:{output}}}));");
    let root_code = format!(
        "const children = await Promise.all(Array.from({{length:{child_count}}}, () => tools.spawn_agent({{role:'fixture',task:'Return the required object',model:'{}',thinking:'medium',output_schema:{{type:'object',properties:{{native_model:{{type:'string'}},answer:{{type:'integer'}}}},required:['native_model','answer'],additionalProperties:false}}}}))); text(await Promise.all(children.map(child => tools.wait_agent({{agent_ids:[child.agent_id],timeout_ms:5000}}))));",
        child_model.as_str()
    );
    let (codex, mut native_claude) = if matches!(root_model, HarnessModel::Codex(_)) {
        (
            Script::new(root_model, root_code),
            Script::new(child_model, child_code),
        )
    } else {
        (
            Script::new(child_model, child_code),
            Script::new(root_model, root_code),
        )
    };

    if clean_spawn {
        native_claude.thinking = Thinking::High;
    }
    let provider = Provider {
        script: native_claude.clone(),
        parent_entered: Arc::default(),
        parent_release: Arc::default(),
    };
    let router = Router::new()
        .route("/messages", post(messages))
        .with_state(provider.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (subagents, mut updates) = Subagents::new(child_count);
    subagents.set_claude_enabled(config.claude().enabled());
    let recipe = Arc::new(AgentRecipe {
        config: config.clone(),
        workspace: directory.path().to_owned(),
        tools: Tools::builder()
            .web_search(false)
            .image_generation(false)
            .build()
            .unwrap(),
        memory: None,
        subagents: subagents.downgrade(),
    });
    let child_recipe = recipe.clone();
    let child_codex = codex.clone();
    let children = Arc::new(Mutex::new(Vec::new()));
    let captured_children = children.clone();
    subagents
        .set_agent_factory(Thinking::Medium, false, move |model, thinking, fast| {
            assert_eq!(model, child_model);
            assert_eq!(thinking, Thinking::Medium);
            assert!(!fast);
            let (agent, events) = build_agent(
                AgentContext { model, thinking },
                &child_recipe,
                child_codex.clone(),
            )?;
            captured_children.lock().unwrap().push(agent.clone());
            Ok((agent, events))
        })
        .unwrap();
    let (root, mut events) = build_agent(
        AgentContext {
            model: root_model,
            thinking: Thinking::Medium,
        },
        &recipe,
        codex.clone(),
    )
    .unwrap();
    if clean_spawn {
        root.set_thinking(Thinking::High).await.unwrap();
        for model in [
            HarnessModel::Claude(ClaudeModel::Fable51),
            HarnessModel::Codex(CodexModel::Luna),
        ] {
            assert!(
                root.spawn_with(SpawnOptions::new().harness_model(model))
                    .await
                    .is_err()
            );
        }
    }
    let result = timeout(Duration::from_secs(15), async {
        root.prompt("Run the cross-provider task")
            .await
            .unwrap()
            .result()
            .await
            .unwrap()
    })
    .await
    .expect("mixed-provider turn timed out");
    assert_eq!(result.final_message(), "finished");
    let mut root_results = Vec::new();
    while let Some(event) = events.try_recv_timed() {
        assert_no_credentials(event.event.payload.get());
        if event.event.kind == AgentEventKind::ToolResult {
            root_results.push(serde_json::from_str::<Value>(event.event.payload.get()).unwrap());
        }
    }
    let spawn = root_results
        .iter()
        .find(|event| event["tool"] == "spawn_agent")
        .expect("nested spawn event");
    assert_eq!(spawn["structured_result"]["model"], child_model.as_str());
    let wait = root_results
        .iter()
        .find(|event| event["tool"] == "wait_agent")
        .expect("nested wait event");
    assert_eq!(
        wait["structured_result"]["agents"][0]["status"]["output"],
        output
    );
    let mut completed = 0;
    let mut submitted = 0;
    while let Ok(update) = updates.try_recv() {
        match update.update {
            AgentUpdate::Added(descriptor) => {
                assert_eq!(descriptor.model, child_model);
                assert_eq!(descriptor.thinking, Thinking::Medium);
            }
            AgentUpdate::Status {
                status: AgentStatus::Completed { output: actual },
                ..
            } => {
                assert_eq!(actual, output);
                completed += 1;
            }
            AgentUpdate::Event { event, .. } => {
                assert_no_credentials(event.payload.get());
                if event.kind != AgentEventKind::ToolResult {
                    continue;
                }
                let payload: Value = serde_json::from_str(event.payload.get()).unwrap();
                if payload["tool"] == "submit_result" {
                    assert_eq!(payload["structured_result"]["accepted"], true);
                    submitted += 1;
                }
            }
            _ => {}
        }
    }
    assert_eq!(completed, child_count);
    assert_eq!(submitted, child_count);
    for script in [&codex, &native_claude] {
        let requests = script.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            if script.model == child_model {
                2 * child_count
            } else {
                2
            }
        );
        if script.model == child_model {
            let prompt = requests[0].to_string();
            assert!(prompt.contains("turn_token: 1"));
            assert!(prompt.contains("native_model"));
        }
        assert!(requests.iter().any(contains_tool_result));
        for request in requests.iter() {
            assert_no_credentials(&request.to_string());
        }
    }
    let clean = if clean_spawn {
        let held_parent = root.prompt("held-parent-task").await.unwrap();
        timeout(Duration::from_secs(5), provider.parent_entered.notified())
            .await
            .unwrap();
        let (clean, mut clean_events) = root.spawn().await.unwrap();
        assert_ne!(root.session_id(), clean.session_id());
        assert!(matches!(
            clean.runtime_snapshot().await.unwrap(),
            ChildSnapshot::Native {
                thinking: Thinking::High,
                ..
            }
        ));
        assert_ne!(events.request_id(), clean_events.request_id());
        let result = timeout(Duration::from_secs(10), async {
            clean
                .prompt("isolated-clean-task")
                .await
                .unwrap()
                .result()
                .await
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(result.final_message(), "finished");
        let mut nested_results = 0;
        while let Some(event) = clean_events.try_recv_timed() {
            assert_no_credentials(event.event.payload.get());
            assert_eq!(event.event.request_id.as_ref(), clean.session_id());
            if event.event.kind == AgentEventKind::ToolResult {
                let payload: Value = serde_json::from_str(event.event.payload.get()).unwrap();
                if payload["tool"] == "wait_agent" {
                    assert_eq!(
                        payload["structured_result"]["agents"][0]["status"]["output"],
                        output
                    );
                    nested_results += 1;
                }
            }
        }
        assert_eq!(nested_results, child_count);
        let initial_clean = native_claude.requests.lock().unwrap()[2].to_string();
        assert!(initial_clean.contains("isolated-clean-task"));
        assert!(!initial_clean.contains("Run the cross-provider task"));
        assert!(!initial_clean.contains("held-parent-task"));
        provider.parent_release.notify_one();
        let held_result = timeout(Duration::from_secs(5), held_parent.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(held_result.final_message(), "finished");
        let result = timeout(Duration::from_secs(10), async {
            root.prompt("parent-after-clean-task")
                .await
                .unwrap()
                .result()
                .await
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(result.final_message(), "finished");
        let parent_request = native_claude
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .to_string();
        assert!(parent_request.contains("Run the cross-provider task"));
        assert!(parent_request.contains("parent-after-clean-task"));
        assert!(!parent_request.contains("isolated-clean-task"));
        while let Some(event) = events.try_recv_timed() {
            assert_eq!(event.event.request_id.as_ref(), root.session_id());
            assert_no_credentials(event.event.payload.get());
            assert!(!event.event.payload.get().contains("isolated-clean-task"));
        }
        Some(clean)
    } else {
        None
    };
    let mut snapshots = std::mem::take(&mut *children.lock().unwrap());
    snapshots.push(root.clone());
    if let Some(clean) = &clean {
        snapshots.push(clean.clone());
    }
    for agent in snapshots {
        let serialized = match agent.runtime_snapshot().await.unwrap() {
            ChildSnapshot::Codex(snapshot) => serde_json::to_string(&snapshot).unwrap(),
            ChildSnapshot::Native { payload, .. } => payload,
        };
        assert_no_credentials(&serialized);
    }
    if let Some(clean) = clean {
        subagents.close_all(clean.session_id()).await;
        clean.shutdown().await.unwrap();
    }
    subagents.close_all(root.session_id()).await;
    root.shutdown().await.unwrap();
    if clean_spawn {
        assert!(root.spawn().await.is_err());
    }
    server.abort();
}
