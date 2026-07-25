use super::{
    model::{AgentDescriptor, AgentId, AgentOrigin, AgentStatus, AgentUpdate},
    runtime::{AgentSummary, Registry, forward_events},
};
use nanocodex::{
    AgentHandle, Tool, ToolContext, ToolDefinition, ToolExecution, ToolInput, ToolResult, Tools,
    ToolsBuildError, async_trait,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowUpTask {
    agent_id: AgentId,
    task: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SteerTask {
    agent_id: AgentId,
    message: String,
}

#[derive(Serialize)]
struct AgentStartReport {
    agent_id: AgentId,
    kind: &'static str,
    role: String,
    status: AgentStatus,
}

#[derive(Serialize)]
struct PromptAccepted {
    agent_id: AgentId,
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
struct EmptyTask {}

#[derive(Serialize)]
struct AgentList {
    agents: Vec<AgentSummary>,
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

struct StartAgent {
    parent: AgentHandle,
    registry: Weak<Registry>,
    origin: AgentOrigin,
}

#[async_trait]
impl Tool for StartAgent {
    fn name(&self) -> &'static str {
        self.origin.tool_name()
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            self.origin.description(),
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
                    }
                },
                "required": ["role", "task"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let AgentTask { role, task } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let capacity = registry.reserve_turn()?;
        let reservation = registry.reserve(context.session_id).await?;
        let id = reservation.id;
        let (child, events) = match self.origin {
            AgentOrigin::Spawn => self.parent.spawn().await,
            AgentOrigin::Fork => self.parent.fork().await,
        }?;
        let session_id = events.request_id().to_owned();
        let descriptor = AgentDescriptor {
            id,
            session_id,
            role: role.clone(),
            task: task.clone(),
            origin: self.origin,
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
                child.clone(),
                event_task,
            )
            .await?;
        registry.send(&reservation.root_session_id, AgentUpdate::Added(descriptor));
        let _ = start_events.send(());

        registry
            .launch_initial_turn(
                &reservation.root_session_id,
                id,
                self.origin.prompt(id, &task),
                capacity,
            )
            .await?;
        Ok(ToolExecution::json(&AgentStartReport {
            agent_id: id,
            kind: self.origin.result_name(),
            role,
            status: AgentStatus::Running,
        }))
    }
}

struct PromptAgent {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for PromptAgent {
    fn name(&self) -> &'static str {
        "prompt_agent"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Starts a follow-up turn on an idle reusable subagent while preserving its conversation and immediately returns.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The agent_id returned by spawn_agent or fork_agent."
                    },
                    "task": {
                        "type": "string",
                        "description": "The next focused task for that subagent."
                    }
                },
                "required": ["agent_id", "task"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let FollowUpTask { agent_id, task } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        registry
            .launch_follow_up(context.session_id, agent_id, task)
            .await?;
        Ok(ToolExecution::json(&PromptAccepted {
            agent_id,
            status: AgentStatus::Running,
        }))
    }
}

struct SteerAgent {
    registry: Weak<Registry>,
}

#[async_trait]
impl Tool for SteerAgent {
    fn name(&self) -> &'static str {
        "steer_agent"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            self.name(),
            "Urgently steers a running subagent at its next safe model boundary without interrupting or replacing its current turn.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The running agent to steer."
                    },
                    "message": {
                        "type": "string",
                        "description": "The urgent instruction to inject into the current turn."
                    }
                },
                "required": ["agent_id", "message"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let SteerTask { agent_id, message } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let agent = registry
            .steer(context.session_id, agent_id, message)
            .await?;
        Ok(ToolExecution::json(&agent))
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
            "Lists every subagent visible to the current session, including completed, interrupted, failed, and closed agents.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let EmptyTask {} = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        Ok(ToolExecution::json(&AgentList {
            agents: registry.list(context.session_id).await?,
        }))
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
                        "description": "Agent IDs returned by spawn_agent or fork_agent. Waiting returns when any one becomes terminal."
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
        Ok(ToolExecution::json(&WaitReport { agents, timed_out }))
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
        Ok(ToolExecution::json(&LifecycleReport { agents }))
    }
}

pub(crate) fn root_tools(
    tools: Tools,
    parent: AgentHandle,
    registry: Arc<Registry>,
) -> Result<Tools, ToolsBuildError> {
    tools
        .into_builder()
        .tool(StartAgent {
            parent: parent.clone(),
            registry: Arc::downgrade(&registry),
            origin: AgentOrigin::Spawn,
        })
        .tool(StartAgent {
            parent,
            registry: Arc::downgrade(&registry),
            origin: AgentOrigin::Fork,
        })
        .tool(PromptAgent {
            registry: Arc::downgrade(&registry),
        })
        .tool(SteerAgent {
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
