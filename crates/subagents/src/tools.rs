use super::{
    error::SubagentError,
    message::MAX_MESSAGE_BYTES,
    model::{
        AgentDescriptor, AgentId, AgentStatus, AgentUpdate, MessageId, MessagePriority,
        MessagePurpose, agent_prompt, deserialize_reasoning_mode, serialize_reasoning_mode,
    },
    output::OutputContract,
    runtime::{AgentDirectoryEntry, AgentSummary, Registry, forward_events},
};
use nanocodex::{
    HarnessModel as Model, ReasoningMode, Thinking, Tool,
    tools::{
        ToolsBuilder,
        contract::{ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, async_trait},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::oneshot;

const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
const SPAWN_AGENT_TOOL: &str = "spawn_agent";
const SUBMIT_RESULT_TOOL: &str = "submit_result";
const SEND_AGENT_MESSAGE_TOOL: &str = "send_agent_message";
const LIST_AGENTS_TOOL: &str = "list_agents";
const WAIT_AGENT_TOOL: &str = "wait_agent";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTask {
    role: String,
    task: String,
    #[serde(deserialize_with = "crate::models::deserialize_model")]
    model: Model,
    thinking: Thinking,
    #[serde(default, deserialize_with = "deserialize_reasoning_mode")]
    reasoning_mode: ReasoningMode,
    output_schema: Value,
}

#[derive(Serialize)]
struct AgentStartReport {
    agent_id: AgentId,
    model: Model,
    #[serde(serialize_with = "serialize_reasoning_mode")]
    reasoning_mode: ReasoningMode,
    role: String,
    status: AgentStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitTask {
    agent_ids: Vec<AgentId>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetAgent {
    agent_id: AgentId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryTask {
    #[serde(default)]
    include_completed: bool,
    #[serde(default)]
    include_self: bool,
}

#[derive(Serialize)]
struct AgentDirectory {
    agents: Vec<AgentDirectoryEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendMessageTask {
    agent_id: AgentId,
    message: String,
    #[serde(default)]
    priority: MessagePriority,
    #[serde(default)]
    purpose: MessagePurpose,
    #[serde(default)]
    in_reply_to: Option<MessageId>,
}

#[derive(Serialize)]
struct WaitReport {
    agents: Vec<AgentSummary>,
    timed_out: bool,
}

#[derive(Serialize)]
struct LifecycleReport {
    agents: Vec<AgentSummary>,
}

fn json_output(value: &impl Serialize) -> ToolResult {
    Ok(ToolOutput::from_json(serde_json::to_value(value)?, true))
}

struct SpawnAgent {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for SpawnAgent {
    fn definition(&self) -> ToolDefinition {
        let claude_enabled = self
            .registry
            .upgrade()
            .is_some_and(|registry| registry.claude_enabled());
        let (models, model_description) = if claude_enabled {
            (
                vec![
                    "luna",
                    "sol",
                    "astra",
                    "haiku-5.5",
                    "sonnet-5.5",
                    "opus-5.5",
                    "fable-5.1",
                ],
                concat!(
                    "Name luna, sol, astra, haiku-5.5, sonnet-5.5, opus-5.5, or fable-5.1 explicitly. ",
                    "Choose model and effort independently for expected quality, total cost, and completion time, including rework. ",
                    "Codex children cannot exceed a Codex parent's tier: luna < sol < astra. ",
                    "Cross-provider selection and delegation between Claude models are supported, subject to effort caps.",
                ),
            )
        } else {
            (
                vec!["luna", "sol", "astra"],
                concat!(
                    "Name luna, sol, or astra explicitly. ",
                    "Choose model and effort independently for expected quality, total cost, and completion time, including rework. ",
                    "Only Codex is enabled. Choose at or below the spawning parent's tier: luna < sol < astra, ",
                    "subject to effort caps.",
                ),
            )
        };
        ToolDefinition::function(
            SPAWN_AGENT_TOOL,
            "Starts a reusable clean-room subagent without inherited conversation history and immediately returns its ID.",
            json!({
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "A short role describing the subagent's specialty."
                    },
                    "task": {
                        "type": "string",
                        "description": "A complete, focused task for the subagent."
                    },
                    "model": {
                        "type": "string",
                        "enum": models,
                        "description": model_description
                    },
                    "thinking": {
                        "type": "string",
                        "enum": ["low", "medium", "high", "xhigh", "max"],
                        "description": "Reasoning effort for this child, bounded by the live configured agent.thinking cap. Root agents follow that cap even after an update during an active turn; registered subagents are additionally bounded by their own assigned effort. Use low for mechanical work, medium for bounded coding or ordinary review, high for a bounded difficult proof, xhigh for interacting contracts or competing designs, and max for the hardest integrated architecture or proof. Higher effort upfront can avoid repeated weaker runs."
                    },
                    "reasoning_mode": {
                        "type": "string",
                        "enum": ["standard", "pro"],
                        "default": "standard",
                        "description": "Reasoning mode for this child. Pro performs additional model work for harder problems at higher latency and cost. Only a spawning agent that itself runs in Pro mode may request pro. A child whose model lacks Pro support runs in standard mode, which then bounds its own children."
                    },
                    "output_schema": {
                        "description": "The JSON Schema that every successful result from this agent must satisfy. Use an object with one string field for a free-form report."
                    }
                },
                "required": ["role", "task", "model", "thinking", "output_schema"],
                "additionalProperties": false
            }),
        )
        .with_output_schema(spawn_agent_output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let AgentTask {
            role,
            task,
            model,
            thinking,
            reasoning_mode,
            output_schema,
        } = input.decode_json()?;
        let contract = OutputContract::compile(&output_schema)?;
        let registry = self
            .registry
            .upgrade()
            .ok_or(SubagentError::RuntimeClosed)?;
        let capacity = registry.reserve_turn()?;
        let reservation = registry.reserve(context.session_id()).await?;
        reservation
            .validate_child(context.model(), model, thinking, reasoning_mode)
            .map_err(SubagentError::from)?;
        let id = reservation.id;
        let (child, events, child_context) =
            registry.spawn_agent(model, thinking, reasoning_mode)?;
        let session_id = child.session_id().to_string();
        let descriptor = AgentDescriptor {
            id,
            session_id,
            model,
            thinking,
            reasoning_mode: child_context.reasoning_mode,
            role: role.clone(),
            task: task.clone(),
            parent: reservation.parent,
        };
        let (start_events, events_ready) = oneshot::channel();
        let event_task = forward_events(
            reservation.root_session_id.clone(),
            id,
            events,
            events_ready,
            Arc::downgrade(&registry),
            registry.updates.clone(),
        );
        registry
            .insert(
                reservation.root_session_id.clone(),
                descriptor.clone(),
                child,
                event_task,
                contract,
            )
            .await?;
        registry.send(&reservation.root_session_id, AgentUpdate::Added(descriptor));
        let _ = start_events.send(());

        registry
            .launch_initial_turn(
                &reservation.root_session_id,
                id,
                agent_prompt(id, &task),
                capacity,
            )
            .await?;
        json_output(&AgentStartReport {
            agent_id: id,
            model,
            reasoning_mode: child_context.reasoning_mode,
            role,
            status: AgentStatus::Running,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitResultArgs {
    turn_token: u64,
    output: Value,
}

struct SubmitResult {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for SubmitResult {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            SUBMIT_RESULT_TOOL,
            "Submits the current subagent turn's final JSON output. Call exactly once with a value matching the output schema in the task prompt. Invalid values can be corrected and retried.",
            json!({
                "type": "object",
                "properties": {
                    "output": {
                        "description": "The final JSON value required by this agent's output schema."
                    },
                    "turn_token": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The current turn token stated in the task prompt."
                    }
                },
                "required": ["turn_token", "output"],
                "additionalProperties": false
            }),
        )
        .with_output_schema(json!({
            "type": "object",
            "properties": {
                "accepted": { "type": "boolean", "const": true }
            },
            "required": ["accepted"],
            "additionalProperties": false
        }))
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let SubmitResultArgs { turn_token, output } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or(SubagentError::RuntimeClosed)?;
        registry
            .submit_result(context.session_id(), turn_token, output)
            .await?;
        Ok(ToolOutput::from_json(json!({ "accepted": true }), true))
    }
}

struct SendAgentMessage {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for SendAgentMessage {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            SEND_AGENT_MESSAGE_TOOL,
            "Sends a bounded directed message to any other agent in the same task tree. Deferred messages start an idle agent or queue behind its active turn. If a send is queued, do not wait for it inside the current turn; finish the turn so queued messages can be delivered. Urgent messages steer a running agent at its next safe model boundary. Delegate messages replace the recipient's assigned task, retain its output schema, and require management authority.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The recipient from list_agents. Any non-closing agent in the same task tree can receive coordination messages."
                    },
                    "message": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_MESSAGE_BYTES,
                        "description": "The focused message body. The runtime enforces a 2048-byte UTF-8 limit."
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["deferred", "urgent"],
                        "default": "deferred",
                        "description": "Urgent steers an active turn; deferred preserves turn boundaries. A queued deferred send requires the current turn to finish before delivery."
                    },
                    "purpose": {
                        "type": "string",
                        "enum": ["delegate", "coordinate", "finding", "question", "reply"],
                        "default": "coordinate",
                        "description": "A typed coordination intent. Delegate is restricted to agents the sender can manage."
                    },
                    "in_reply_to": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "A message ID from the same two-party thread. Replies must reverse the original direction."
                    }
                },
                "required": ["agent_id", "message"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let SendMessageTask {
            agent_id,
            message,
            priority,
            purpose,
            in_reply_to,
        } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or(SubagentError::RuntimeClosed)?;
        let receipt = registry
            .send_message(
                context.session_id(),
                agent_id,
                priority,
                purpose,
                in_reply_to,
                message,
            )
            .await?;
        json_output(&receipt)
    }
}

struct ListAgents {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for ListAgents {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            LIST_AGENTS_TOOL,
            "Lists agents in the same task tree. Set include_completed to retrieve terminal statuses and completed results in status.output. After reuse or closure, last_output retains the latest completed result.",
            json!({
                "type": "object",
                "properties": {
                    "include_completed": {
                        "type": "boolean",
                        "default": false,
                        "description": "Includes completed, interrupted, failed, and closed agents."
                    },
                    "include_self": {
                        "type": "boolean",
                        "default": false,
                        "description": "Includes the calling agent for topology inspection. Self-messaging remains unavailable."
                    }
                },
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let DirectoryTask {
            include_completed,
            include_self,
        } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or(SubagentError::RuntimeClosed)?;
        json_output(&AgentDirectory {
            agents: registry
                .directory(context.session_id(), include_completed, include_self)
                .await,
        })
    }
}

struct WaitAgent {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for WaitAgent {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            WAIT_AGENT_TOOL,
            "Waits until any requested active subagent reaches a terminal status. Errors immediately if any selected agent is already terminal, listing remaining active IDs. Retrieve available results with list_agents({include_completed:true}), save them, and wait only on active IDs.",
            json!({
                "type": "object",
                "properties": {
                    "agent_ids": {
                        "type": "array",
                        "items": { "type": "integer", "minimum": 1 },
                        "minItems": 1,
                        "description": "Active agent IDs returned by spawn_agent. Waiting returns when any one becomes terminal; already terminal IDs cause an immediate error."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 300000,
                        "description": "Bounded wait in milliseconds. Defaults to 30000."
                    }
                },
                "required": ["agent_ids"],
                "additionalProperties": false
            }),
        )
        .with_output_schema(wait_agent_output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let WaitTask {
            agent_ids,
            timeout_ms,
        } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or(SubagentError::RuntimeClosed)?;
        let duration = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_WAIT_TIMEOUT)
            .min(MAX_WAIT_TIMEOUT);
        let (agents, timed_out) = registry
            .wait(context.session_id(), &agent_ids, duration)
            .await?;
        json_output(&WaitReport { agents, timed_out })
    }
}

#[derive(Clone, Copy)]
enum LifecycleOperation {
    Interrupt,
    Close,
}

struct ChangeAgentLifecycle {
    registry: Weak<Registry>,
    operation: LifecycleOperation,
}

impl ChangeAgentLifecycle {
    fn tool_name(&self) -> &'static str {
        match self.operation {
            LifecycleOperation::Interrupt => "interrupt_agent",
            LifecycleOperation::Close => "close_agent",
        }
    }
}

#[async_trait]
impl Tool for ChangeAgentLifecycle {
    fn definition(&self) -> ToolDefinition {
        let description = match self.operation {
            LifecycleOperation::Interrupt => {
                "Interrupts an agent's active turn and every active descendant, waits for their model and tool resources to stop, and keeps the sessions reusable."
            }
            LifecycleOperation::Close => {
                "Closes an agent and its entire descendant subtree, waiting for active model and tool resources to stop before returning. Closed agents remain inspectable but are not reusable."
            }
        };
        ToolDefinition::function(
            self.tool_name(),
            description,
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The root of the subagent subtree to stop."
                    }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let TargetAgent { agent_id } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or(SubagentError::RuntimeClosed)?;
        let agents = match self.operation {
            LifecycleOperation::Interrupt => {
                registry.interrupt(context.session_id(), agent_id).await?
            }
            LifecycleOperation::Close => registry.close(context.session_id(), agent_id).await?,
        };
        json_output(&LifecycleReport { agents })
    }
}

impl super::runtime::WeakSubagents {
    /// Adds the child-agent tool surface to a Nanocodex tool builder.
    ///
    /// The returned builder is not finalized, so applications can compose their own tools and
    /// perform duplicate-name validation once with [`ToolsBuilder::build`].
    pub fn install_tools(&self, tools: ToolsBuilder) -> ToolsBuilder {
        let registry = self.registry.clone();
        tools
            .tool(SpawnAgent {
                registry: registry.clone(),
            })
            .tool(SubmitResult {
                registry: registry.clone(),
            })
            .tool(SendAgentMessage {
                registry: registry.clone(),
            })
            .tool(ListAgents {
                registry: registry.clone(),
            })
            .tool(WaitAgent {
                registry: registry.clone(),
            })
            .tool(ChangeAgentLifecycle {
                registry: registry.clone(),
                operation: LifecycleOperation::Interrupt,
            })
            .tool(ChangeAgentLifecycle {
                registry,
                operation: LifecycleOperation::Close,
            })
    }
}

fn spawn_agent_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": { "type": "integer" },
            "model": { "type": "string" },
            "reasoning_mode": { "type": "string", "enum": ["standard", "pro"] },
            "role": { "type": "string" },
            "status": {
                "type": "object",
                "properties": { "state": { "type": "string", "const": "running" } },
                "required": ["state"],
                "additionalProperties": false
            }
        },
        "required": ["agent_id", "model", "reasoning_mode", "role", "status"],
        "additionalProperties": false
    })
}

fn wait_agent_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agents": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "integer" },
                        "model": { "type": "string" },
                        "reasoning_mode": { "type": "string", "enum": ["standard", "pro"] },
                        "role": { "type": "string" },
                        "task": { "type": "string" },
                        "parent_agent_id": { "type": ["integer", "null"] },
                        "status": agent_status_schema(),
                        "last_output": {}
                    },
                    "required": ["agent_id", "model", "reasoning_mode", "role", "task", "parent_agent_id", "status"],
                    "additionalProperties": false
                }
            },
            "timed_out": { "type": "boolean" }
        },
        "required": ["agents", "timed_out"],
        "additionalProperties": false
    })
}

fn agent_status_schema() -> Value {
    let state_only = ["pending", "running", "interrupted", "closing", "closed"].map(|state| {
        json!({
            "type": "object",
            "properties": { "state": { "type": "string", "const": state } },
            "required": ["state"],
            "additionalProperties": false
        })
    });
    let mut variants = state_only.into_iter().collect::<Vec<_>>();
    variants.push(json!({
        "type": "object",
        "properties": {
            "state": { "type": "string", "const": "completed" },
            "output": {}
        },
        "required": ["state", "output"],
        "additionalProperties": false
    }));
    variants.push(json!({
        "type": "object",
        "properties": {
            "state": { "type": "string", "const": "failed" },
            "error": { "type": "string" }
        },
        "required": ["state", "error"],
        "additionalProperties": false
    }));
    json!({ "oneOf": variants })
}

#[cfg(test)]
mod tests {
    use super::{SendAgentMessage, SpawnAgent, SubmitResult, WaitAgent};
    use crate::{
        AgentContext, AgentDescriptor, AgentUpdate, ScopedAgentUpdate, Speed,
        error::{SpawnError, SubagentError},
        runtime::Registry,
        test_support::recording_agent,
    };
    use nanocodex::{
        HarnessModel as Model, Model as CodexModel, NanocodexError, ReasoningMode, Thinking, Tool,
        tools::contract::{ToolContext, ToolError, ToolInput, ToolOutput},
    };
    use serde_json::{Value, json, value::to_raw_value};
    use std::{
        sync::{Arc, Weak},
        time::Duration,
    };
    use tokio::{sync::mpsc, time::timeout};

    fn subagent_error(error: &ToolError) -> &SubagentError {
        error
            .downcast_ref()
            .expect("subagent tools should fail with a typed runtime error")
    }

    #[tokio::test]
    async fn claude_opt_in_controls_schema_and_runtime_admission() {
        let (runtime, _updates) = crate::Subagents::new(1);
        let (captured, mut arguments) = mpsc::unbounded_channel();
        runtime
            .set_agent_factory(
                Thinking::Max,
                ReasoningMode::Standard,
                Speed::Standard,
                move |context: AgentContext, speed| {
                    captured
                        .send((context.model, context.thinking, speed))
                        .unwrap();
                    Err(NanocodexError::InvalidRequest(
                        "stop after capture".to_owned(),
                    ))
                },
            )
            .unwrap();
        let tool = SpawnAgent {
            registry: runtime.downgrade().registry,
        };
        for enabled in [false, true, false] {
            runtime.set_claude_enabled(enabled);
            let definition = tool.definition();
            let validator =
                jsonschema::validator_for(definition.parameters().unwrap().as_value()).unwrap();
            for model in ["haiku-5.5", "sonnet-5.5", "opus-5.5", "fable-5.1"] {
                let input = json!({"role": "review", "task": "Review", "model": model,
                    "thinking": "medium", "output_schema": {"type": "object"}});
                assert_eq!(validator.is_valid(&input), enabled);
                // Execute directly to exercise admission independently of schema validation.
                let error = tool
                    .execute(
                        ToolInput::Function(to_raw_value(&input).unwrap()),
                        ToolContext::new(
                            Model::Codex(CodexModel::Luna).as_str(),
                            "root",
                            "spawn",
                            &[],
                            128,
                        ),
                    )
                    .await
                    .err()
                    .unwrap();
                if enabled {
                    assert!(matches!(subagent_error(&error), SubagentError::Agent(_)));
                    assert_eq!(
                        arguments.try_recv().unwrap().0,
                        crate::parse_model(model).unwrap()
                    );
                } else {
                    assert!(matches!(
                        subagent_error(&error),
                        SubagentError::Spawn(SpawnError::ClaudeDisabled)
                    ));
                    assert!(arguments.try_recv().is_err());
                }
            }
        }
    }

    #[tokio::test]
    async fn spawn_agent_enforces_root_model_order_and_accepts_supported_efforts() {
        let (updates, _receiver) = mpsc::unbounded_channel();
        let registry = Arc::new(Registry::new(updates, 1));
        let (captured, mut arguments) = mpsc::unbounded_channel();
        registry
            .set_agent_factory(
                Thinking::Max,
                ReasoningMode::Standard,
                Speed::Standard,
                move |context: AgentContext, speed| {
                    captured
                        .send((context.model, context.thinking, speed))
                        .unwrap();
                    Err(NanocodexError::InvalidRequest(
                        "stop after capture".to_owned(),
                    ))
                },
            )
            .unwrap();
        let tool = SpawnAgent {
            registry: Arc::downgrade(&registry),
        };
        let definition = tool.definition();
        assert!(
            definition.output_schema().unwrap().as_value()["required"]
                .as_array()
                .unwrap()
                .contains(&json!("model"))
        );
        let validator =
            jsonschema::validator_for(definition.parameters().unwrap().as_value()).unwrap();

        let models = [
            ("luna", Model::Codex(CodexModel::Luna)),
            ("sol", Model::Codex(CodexModel::Sol)),
            ("astra", Model::Codex(CodexModel::Astra)),
        ];
        for (parent_rank, (_, parent_model)) in models.iter().enumerate() {
            for (child_rank, (model, expected_model)) in models.iter().copied().enumerate() {
                for (thinking, expected) in [
                    ("low", Thinking::Low),
                    ("medium", Thinking::Medium),
                    ("high", Thinking::High),
                    ("xhigh", Thinking::Xhigh),
                    ("max", Thinking::Max),
                ] {
                    let input = json!({
                        "role": "reviewer",
                        "task": "Review a focused change.",
                        "model": model,
                        "thinking": thinking,
                        "output_schema": { "type": "object" }
                    });
                    assert!(validator.is_valid(&input));
                    let error = tool
                        .execute(
                            ToolInput::Function(to_raw_value(&input).unwrap()),
                            ToolContext::new(parent_model.as_str(), "root", "spawn", &[], 128),
                        )
                        .await
                        .err()
                        .expect("factory should stop after capture");
                    if child_rank > parent_rank {
                        assert!(matches!(
                            subagent_error(&error),
                            SubagentError::Spawn(SpawnError::ModelExceedsParent { .. })
                        ));
                        assert!(arguments.try_recv().is_err());
                        continue;
                    }
                    assert!(matches!(subagent_error(&error), SubagentError::Agent(_)));
                    assert_eq!(
                        arguments.try_recv().unwrap(),
                        (expected_model, expected, Speed::Standard)
                    );
                }
            }
        }

        for model in [
            None,
            Some(json!(null)),
            Some(json!("selected")),
            Some(json!("terra")),
            Some(json!("unknown")),
            Some(json!("Astra")),
            Some(json!(1)),
        ] {
            let mut input = json!({
                "role": "reviewer", "task": "Review a focused change.",
                "thinking": "medium", "output_schema": { "type": "object" }
            });
            if let Some(model) = model {
                input["model"] = model;
            }
            assert!(!validator.is_valid(&input));
            assert!(
                tool.execute(
                    ToolInput::Function(to_raw_value(&input).unwrap()),
                    ToolContext::new("gpt-6-astra", "root", "spawn", &[], 128),
                )
                .await
                .is_err()
            );
            assert!(arguments.try_recv().is_err());
        }

        for thinking in [
            None,
            Some(json!(null)),
            Some(json!("none")),
            Some(json!("ultra")),
        ] {
            let mut input = json!({
                "role": "reviewer",
                "task": "Review a focused change.",
                "model": "astra",
                "output_schema": { "type": "object" }
            });
            if let Some(thinking) = thinking {
                input["thinking"] = thinking;
            }
            assert!(!validator.is_valid(&input));
            assert!(
                tool.execute(
                    ToolInput::Function(to_raw_value(&input).unwrap()),
                    ToolContext::new("gpt-6-astra", "root", "spawn", &[], 128),
                )
                .await
                .is_err()
            );
            assert!(arguments.try_recv().is_err());
        }
    }

    /// Drives `spawn_agent` against real, never-finishing child harnesses so registered
    /// parents carry the context the runtime actually stored for them.
    struct ModeHarness {
        runtime: crate::Subagents,
        updates: mpsc::UnboundedReceiver<ScopedAgentUpdate>,
        built: mpsc::UnboundedReceiver<AgentContext>,
        prompts: mpsc::UnboundedReceiver<(CodexModel, Thinking, String)>,
        tool: SpawnAgent,
    }

    /// A child registered by a successful spawn.
    struct Spawned {
        session_id: String,
        report: Value,
        descriptor: AgentDescriptor,
    }

    impl ModeHarness {
        fn new(max_reasoning_mode: ReasoningMode) -> Self {
            let (runtime, updates) = crate::Subagents::new(8);
            runtime.set_claude_enabled(true);
            let (built_sender, built) = mpsc::unbounded_channel();
            let (prompt_sender, prompts) = mpsc::unbounded_channel();
            runtime
                .set_agent_factory(
                    Thinking::Max,
                    max_reasoning_mode,
                    Speed::Standard,
                    move |context, _| {
                        built_sender.send(context).unwrap();
                        Ok(recording_agent(prompt_sender.clone()))
                    },
                )
                .unwrap();
            let tool = SpawnAgent {
                registry: runtime.downgrade().registry,
            };
            Self {
                runtime,
                updates,
                built,
                prompts,
                tool,
            }
        }

        async fn spawn(
            &self,
            caller_session: &str,
            model: &str,
            reasoning_mode: Option<Value>,
        ) -> Result<ToolOutput, ToolError> {
            let mut input = json!({
                "role": "reviewer",
                "task": "Review a focused change.",
                "model": model,
                "thinking": "medium",
                "output_schema": { "type": "object" }
            });
            if let Some(reasoning_mode) = reasoning_mode {
                input["reasoning_mode"] = reasoning_mode;
            }
            self.tool
                .execute(
                    ToolInput::Function(to_raw_value(&input).unwrap()),
                    ToolContext::new(
                        Model::Codex(CodexModel::Astra).as_str(),
                        caller_session,
                        "spawn",
                        &[],
                        128,
                    ),
                )
                .await
        }

        /// Spawns successfully and checks that the factory, report, descriptor, and injected
        /// turn context all carry `expected`.
        async fn spawn_ok(
            &mut self,
            caller_session: &str,
            model: &str,
            reasoning_mode: Option<Value>,
            expected: ReasoningMode,
        ) -> Spawned {
            let report = self
                .spawn(caller_session, model, reasoning_mode)
                .await
                .unwrap()
                .structured_result();
            let built = self.built.try_recv().unwrap();
            assert_eq!(built.model, crate::parse_model(model).unwrap());
            assert_eq!(built.reasoning_mode, expected);
            assert_eq!(report["reasoning_mode"], expected.as_str());
            let descriptor = loop {
                let update = timeout(Duration::from_secs(5), self.updates.recv())
                    .await
                    .unwrap()
                    .unwrap();
                if let AgentUpdate::Added(descriptor) = update.update {
                    break descriptor;
                }
            };
            assert_eq!(descriptor.reasoning_mode, expected);
            let (_, _, prompt) = timeout(Duration::from_secs(5), self.prompts.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                prompt.contains(&format!("in {expected} reasoning mode.")),
                "{prompt}"
            );
            Spawned {
                session_id: descriptor.session_id.clone(),
                report,
                descriptor,
            }
        }

        async fn spawn_rejected(
            &mut self,
            caller_session: &str,
            model: &str,
            reasoning_mode: Option<Value>,
        ) -> SubagentError {
            let error = self
                .spawn(caller_session, model, reasoning_mode)
                .await
                .err()
                .unwrap();
            assert!(
                self.built.try_recv().is_err(),
                "rejected requests never reach the factory"
            );
            error
                .downcast::<SubagentError>()
                .map(|error| *error)
                .expect("subagent tools should fail with a typed runtime error")
        }
    }

    #[tokio::test]
    async fn root_reasoning_mode_bounds_requests_before_model_fallback() {
        let pro = || Some(json!("pro"));
        let standard = || Some(json!("standard"));

        let mut root = ModeHarness::new(ReasoningMode::Pro);
        root.spawn_ok("root", "sol", None, ReasoningMode::Standard)
            .await;
        root.spawn_ok("root", "sol", standard(), ReasoningMode::Standard)
            .await;
        root.spawn_ok("root", "sol", pro(), ReasoningMode::Pro)
            .await;
        let fallback = root
            .spawn_ok("root", "opus-5.5", pro(), ReasoningMode::Standard)
            .await;
        assert_eq!(fallback.report["model"], json!("claude-opus-5-5"));
        root.runtime.close_all("root").await;

        let mut root = ModeHarness::new(ReasoningMode::Standard);
        root.spawn_ok("root", "sol", None, ReasoningMode::Standard)
            .await;
        for model in ["sol", "opus-5.5"] {
            assert!(matches!(
                root.spawn_rejected("root", model, pro()).await,
                SubagentError::Spawn(SpawnError::ReasoningModeExceedsMaximum {
                    requested: ReasoningMode::Pro,
                    maximum: ReasoningMode::Standard,
                })
            ));
        }
        root.runtime.close_all("root").await;
    }

    #[tokio::test]
    async fn registered_parents_delegate_pro_only_from_their_actual_mode() {
        let pro = || Some(json!("pro"));
        let mut harness = ModeHarness::new(ReasoningMode::Pro);
        let pro_parent = harness
            .spawn_ok("root", "sol", pro(), ReasoningMode::Pro)
            .await;
        let standard_parent = harness
            .spawn_ok("root", "sol", None, ReasoningMode::Standard)
            .await;
        let claude_parent = harness
            .spawn_ok("root", "opus-5.5", pro(), ReasoningMode::Standard)
            .await;

        let pro_child = harness
            .spawn_ok(&pro_parent.session_id, "luna", pro(), ReasoningMode::Pro)
            .await;
        assert_eq!(pro_child.descriptor.parent, Some(pro_parent.descriptor.id));
        harness
            .spawn_ok(
                &pro_parent.session_id,
                "luna",
                None,
                ReasoningMode::Standard,
            )
            .await;

        // A standard parent cannot regain Pro, including through a Claude fallback parent or a
        // Claude target that would itself resolve to standard.
        for parent in [&standard_parent, &claude_parent] {
            for model in ["sol", "opus-5.5"] {
                assert!(matches!(
                    harness
                        .spawn_rejected(&parent.session_id, model, pro())
                        .await,
                    SubagentError::Spawn(SpawnError::ReasoningModeExceedsParent {
                        requested: ReasoningMode::Pro,
                        parent: ReasoningMode::Standard,
                    })
                ));
            }
        }
        let standard_grandchild = harness
            .spawn_ok(
                &claude_parent.session_id,
                "sol",
                None,
                ReasoningMode::Standard,
            )
            .await;
        assert!(matches!(
            harness
                .spawn_rejected(&standard_grandchild.session_id, "sol", pro())
                .await,
            SubagentError::Spawn(SpawnError::ReasoningModeExceedsParent { .. })
        ));
        harness.runtime.close_all("root").await;
    }

    #[tokio::test]
    async fn malformed_reasoning_modes_are_rejected_before_spawning() {
        let mut harness = ModeHarness::new(ReasoningMode::Pro);
        let definition = harness.tool.definition();
        let parameters = definition.parameters().unwrap().as_value();
        assert_eq!(
            parameters["properties"]["reasoning_mode"]["enum"],
            json!(["standard", "pro"])
        );
        assert_eq!(
            parameters["properties"]["reasoning_mode"]["default"],
            json!("standard")
        );
        assert!(
            !parameters["required"]
                .as_array()
                .unwrap()
                .contains(&json!("reasoning_mode"))
        );
        let validator = jsonschema::validator_for(parameters).unwrap();
        for mode in [
            json!(null),
            json!("Pro"),
            json!("PRO"),
            json!("ultra"),
            json!(""),
            json!(1),
            json!(true),
        ] {
            let input = json!({
                "role": "reviewer", "task": "Review", "model": "sol", "thinking": "medium",
                "output_schema": { "type": "object" }, "reasoning_mode": mode
            });
            assert!(!validator.is_valid(&input));
            assert!(harness.spawn("root", "sol", Some(mode)).await.is_err());
            assert!(harness.built.try_recv().is_err());
        }
        assert!(harness.updates.try_recv().is_err());
    }

    #[test]
    fn send_message_priority_defaults_to_deferred_delivery() {
        let definition = SendAgentMessage {
            registry: Weak::<Registry>::new(),
        }
        .definition();
        let priority = &definition.parameters().unwrap().as_value()["properties"]["priority"];

        assert_eq!(priority["enum"], json!(["deferred", "urgent"]));
        assert_eq!(priority["default"], json!("deferred"));
    }

    #[test]
    fn submit_result_requires_the_turn_token_and_one_output_value() {
        let definition = SubmitResult {
            registry: Weak::<Registry>::new(),
        }
        .definition();
        let parameters = definition.parameters().unwrap().as_value();

        assert_eq!(parameters["required"], json!(["turn_token", "output"]));
        assert_eq!(parameters["additionalProperties"], json!(false));
        assert_eq!(parameters["properties"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn wait_agent_reports_the_model_of_every_agent() {
        let definition = WaitAgent {
            registry: Weak::<Registry>::new(),
        }
        .definition();
        let output = definition.output_schema().unwrap();
        let agent = &output.as_value()["properties"]["agents"]["items"];

        assert_eq!(agent["properties"]["model"], json!({ "type": "string" }));
        assert_eq!(
            agent["properties"]["reasoning_mode"]["enum"],
            json!(["standard", "pro"])
        );
        for field in ["model", "reasoning_mode"] {
            assert!(
                agent["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(field))
            );
        }
    }
}
