use super::{
    message::MAX_MESSAGE_BYTES,
    model::{MessageId, MessagePriority, MessagePurpose},
    runtime::AgentDirectoryEntry,
};
use super::{
    model::{AgentDescriptor, AgentId, AgentStatus, AgentUpdate, agent_prompt},
    runtime::{AgentSummary, OutputContract, Registry, forward_events},
};
use nanocodex::{
    AgentHandle, Tool, ToolContext, ToolDefinition, ToolExecution, ToolInput, ToolResult, Tools,
    ToolsBuildError, async_trait,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTask {
    role: String,
    task: String,
    output_schema: Value,
}

#[derive(Serialize)]
struct AgentStartReport {
    agent_id: AgentId,
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
    Ok(ToolExecution::from_json(serde_json::to_value(value)?, true))
}

struct SpawnAgent {
    parent: AgentHandle,
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for SpawnAgent {
    fn name(&self) -> &'static str {
        "spawn_agent"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
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
                    "output_schema": {
                        "description": "The JSON Schema that every successful result from this agent must satisfy. Use an object with one string field for a free-form report."
                    }
                },
                "required": ["role", "task", "output_schema"],
                "additionalProperties": false
            }),
        )
        .with_output_schema(spawn_agent_output_schema())
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let AgentTask {
            role,
            task,
            output_schema,
        } = input.decode_json()?;
        let contract = OutputContract::compile(&output_schema)?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let capacity = registry.reserve_turn()?;
        let reservation = registry.reserve(context.session_id).await?;
        let id = reservation.id;
        let (child, events) = self.parent.spawn().await?;
        let session_id = events.request_id().to_owned();
        let descriptor = AgentDescriptor {
            id,
            session_id,
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
    fn name(&self) -> &'static str {
        "submit_result"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
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
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        registry
            .submit_result(context.session_id, turn_token, output)
            .await?;
        Ok(ToolExecution::from_json(json!({ "accepted": true }), true))
    }
}

struct SendAgentMessage {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for SendAgentMessage {
    fn name(&self) -> &'static str {
        "send_agent_message"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
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
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let receipt = registry
            .send_message(
                context.session_id,
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
    fn name(&self) -> &'static str {
        "list_agents"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Lists a compact directory of agents in the same task tree. Active recipients are returned by default; completed agents can be included when a follow-up message is needed.",
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
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        json_output(&AgentDirectory {
            agents: registry
                .directory(context.session_id, include_completed, include_self)
                .await,
        })
    }
}

struct WaitAgent {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for WaitAgent {
    fn name(&self) -> &'static str {
        "wait_agent"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Waits until any requested subagent reaches a terminal status and returns current statuses and reports. Use one call with multiple IDs instead of polling the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "agent_ids": {
                        "type": "array",
                        "items": { "type": "integer", "minimum": 1 },
                        "minItems": 1,
                        "description": "Agent IDs returned by spawn_agent. Waiting returns when any one becomes terminal."
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
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let duration = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_WAIT_TIMEOUT)
            .min(MAX_WAIT_TIMEOUT);
        let (agents, timed_out) = registry
            .wait(context.session_id, &agent_ids, duration)
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

#[async_trait]
impl Tool for ChangeAgentLifecycle {
    fn name(&self) -> &'static str {
        match self.operation {
            LifecycleOperation::Interrupt => "interrupt_agent",
            LifecycleOperation::Close => "close_agent",
        }
    }

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
            self.name(),
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
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let agents = match self.operation {
            LifecycleOperation::Interrupt => {
                registry.interrupt(context.session_id, agent_id).await?
            }
            LifecycleOperation::Close => registry.close(context.session_id, agent_id).await?,
        };
        json_output(&LifecycleReport { agents })
    }
}

pub(crate) fn install_tools(
    tools: Tools,
    parent: AgentHandle,
    registry: Arc<Registry>,
) -> Result<Tools, ToolsBuildError> {
    tools
        .into_builder()
        .tool(SpawnAgent {
            parent,
            registry: Arc::downgrade(&registry),
        })
        .tool(SubmitResult {
            registry: Arc::downgrade(&registry),
        })
        .tool(SendAgentMessage {
            registry: Arc::downgrade(&registry),
        })
        .tool(ListAgents {
            registry: Arc::downgrade(&registry),
        })
        .tool(WaitAgent {
            registry: Arc::downgrade(&registry),
        })
        .tool(ChangeAgentLifecycle {
            registry: Arc::downgrade(&registry),
            operation: LifecycleOperation::Interrupt,
        })
        .tool(ChangeAgentLifecycle {
            registry: Arc::downgrade(&registry),
            operation: LifecycleOperation::Close,
        })
        .build()
}

fn spawn_agent_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": { "type": "integer" },
            "role": { "type": "string" },
            "status": {
                "type": "object",
                "properties": { "state": { "type": "string", "const": "running" } },
                "required": ["state"],
                "additionalProperties": false
            }
        },
        "required": ["agent_id", "role", "status"],
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
                        "role": { "type": "string" },
                        "task": { "type": "string" },
                        "parent_agent_id": { "type": ["integer", "null"] },
                        "status": agent_status_schema(),
                        "last_output": {}
                    },
                    "required": ["agent_id", "role", "task", "parent_agent_id", "status"],
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
    use super::{SendAgentMessage, SubmitResult, WaitAgent};
    use crate::core::extensions::subagents::runtime::Registry;
    use nanocodex::Tool;
    use serde_json::json;
    use std::sync::Weak;

    #[test]
    fn send_message_definition_names_deferred_delivery_and_queued_waiting() {
        let definition = SendAgentMessage {
            registry: Weak::<Registry>::new(),
        }
        .definition();
        let priority = &definition.parameters().unwrap().as_value()["properties"]["priority"];

        assert_eq!(priority["enum"], json!(["deferred", "urgent"]));
        assert_eq!(priority["default"], json!("deferred"));
        assert!(definition.description().contains("do not wait"));
        assert!(definition.description().contains("finish the turn"));
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
    fn wait_agent_only_refers_to_clean_spawns() {
        let definition = WaitAgent {
            registry: Weak::<Registry>::new(),
        }
        .definition();
        let description =
            &definition.parameters().unwrap().as_value()["properties"]["agent_ids"]["description"];

        assert!(description.as_str().unwrap().contains("spawn_agent"));
        assert!(!description.as_str().unwrap().contains("fork_agent"));
    }
}
