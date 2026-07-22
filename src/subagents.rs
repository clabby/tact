//! Reusable child-agent tools and the typed runtime/UI update boundary.

use nanocodex::{
    AgentEvent, AgentEvents, AgentHandle, Nanocodex, Tool, ToolContext, ToolDefinition,
    ToolExecution, ToolInput, ToolResult, Tools, ToolsBuildError, Turn, TurnControl, TurnResult,
    async_trait,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct AgentId(u64);

impl AgentId {
    #[cfg(test)]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    fn next(counter: &AtomicU64) -> Self {
        Self(counter.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentOrigin {
    Spawn,
    Fork,
}

impl AgentOrigin {
    const fn tool_name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn_agent",
            Self::Fork => "fork_agent",
        }
    }

    const fn result_name(self) -> &'static str {
        match self {
            Self::Spawn => "independent",
            Self::Fork => "fork",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Spawn => {
                "Starts a reusable clean-room subagent without inherited conversation history, runs its first task, and returns its ID and report."
            }
            Self::Fork => {
                "Starts a reusable subagent from the latest safe model boundary, runs its first task, and returns its ID and report."
            }
        }
    }

    fn prompt(self, id: AgentId, task: &str) -> String {
        let context = match self {
            Self::Spawn => "You have no inherited conversation context.",
            Self::Fork => "Use the inherited conversation only as context for this delegation.",
        };
        format!(
            "Act as a specialist subagent. {context} Work only on the delegated task and return a \
             compact, evidence-backed report to the parent agent. Your agent ID is {id}. When \
             delegating work to another subagent, pass {id} as parent_agent_id so the runtime can \
             place it beneath you in the task tree.\n\nDelegated task:\n{task}"
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentDescriptor {
    pub(crate) id: AgentId,
    pub(crate) session_id: String,
    pub(crate) role: String,
    pub(crate) task: String,
    pub(crate) origin: AgentOrigin,
    pub(crate) parent: Option<AgentId>,
}

#[derive(Debug)]
pub(crate) enum AgentUpdate {
    Added(AgentDescriptor),
    Event { id: AgentId, event: AgentEvent },
    Closed { id: AgentId },
}

struct ChildSession {
    agent: Nanocodex,
    descriptor: AgentDescriptor,
    _event_task: JoinHandle<()>,
}

pub(crate) struct Registry {
    next_id: AtomicU64,
    sessions: tokio::sync::Mutex<HashMap<AgentId, ChildSession>>,
    updates: mpsc::UnboundedSender<AgentUpdate>,
    active: tokio::sync::Mutex<HashMap<AgentId, TurnControl>>,
}

impl Registry {
    fn new(updates: mpsc::UnboundedSender<AgentUpdate>) -> Self {
        Self {
            next_id: AtomicU64::new(0),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            updates,
            active: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn insert(
        &self,
        descriptor: AgentDescriptor,
        agent: Nanocodex,
        event_task: JoinHandle<()>,
    ) {
        self.sessions.lock().await.insert(
            descriptor.id,
            ChildSession {
                agent,
                descriptor,
                _event_task: event_task,
            },
        );
    }

    async fn session(&self, id: AgentId) -> Option<(Nanocodex, AgentDescriptor)> {
        self.sessions
            .lock()
            .await
            .get(&id)
            .map(|session| (session.agent.clone(), session.descriptor.clone()))
    }

    async fn contains(&self, id: AgentId) -> bool {
        self.sessions.lock().await.contains_key(&id)
    }

    async fn run(&self, id: AgentId, turn: Turn) -> nanocodex::Result<TurnResult> {
        self.active.lock().await.insert(id, turn.control());
        let result = turn.result().await;
        self.active.lock().await.remove(&id);
        result
    }

    async fn cancel_all(&self) {
        let controls = self
            .active
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for control in controls {
            drop(control.cancel().await);
        }
    }
}

#[derive(Clone)]
pub(crate) struct SubagentControl {
    registry: Arc<Registry>,
}

impl SubagentControl {
    pub(crate) async fn cancel_all(&self) {
        self.registry.cancel_all().await;
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTask {
    role: String,
    task: String,
    #[serde(default)]
    parent_agent_id: Option<AgentId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowUpTask {
    agent_id: AgentId,
    task: String,
}

#[derive(Serialize)]
struct AgentReport {
    agent_id: AgentId,
    kind: &'static str,
    role: String,
    report: String,
}

#[derive(Serialize)]
struct FollowUpReport {
    agent_id: AgentId,
    report: String,
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
                    },
                    "parent_agent_id": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Your own agent ID when recursively delegating. Root agents omit this field."
                    }
                },
                "required": ["role", "task"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let AgentTask {
            role,
            task,
            parent_agent_id,
        } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        if let Some(parent_id) = parent_agent_id
            && !registry.contains(parent_id).await
        {
            return Err(
                std::io::Error::other(format!("unknown parent_agent_id {parent_id}")).into(),
            );
        }
        let id = AgentId::next(&registry.next_id);
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
            parent: parent_agent_id,
        };
        drop(
            registry
                .updates
                .send(AgentUpdate::Added(descriptor.clone())),
        );
        let event_task = forward_events(id, events, registry.updates.clone());
        registry.insert(descriptor, child.clone(), event_task).await;

        let turn = child.prompt(self.origin.prompt(id, &task)).await?;
        let result = registry.run(id, turn).await?;
        Ok(ToolExecution::json(&AgentReport {
            agent_id: id,
            kind: self.origin.result_name(),
            role,
            report: result.final_message,
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
            "Runs a follow-up turn on a reusable subagent while preserving its conversation and runtime.",
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

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let FollowUpTask { agent_id, task } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let (child, mut descriptor) = registry
            .session(agent_id)
            .await
            .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {agent_id}")))?;
        descriptor.task.clone_from(&task);
        drop(registry.updates.send(AgentUpdate::Added(descriptor)));
        let turn = child.prompt(task).await?;
        let result = registry.run(agent_id, turn).await?;
        Ok(ToolExecution::json(&FollowUpReport {
            agent_id,
            report: result.final_message,
        }))
    }
}

fn forward_events(
    id: AgentId,
    mut events: AgentEvents,
    updates: mpsc::UnboundedSender<AgentUpdate>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if updates.send(AgentUpdate::Event { id, event }).is_err() {
                return;
            }
        }
        drop(updates.send(AgentUpdate::Closed { id }));
    })
}

pub(crate) fn channel() -> (
    Arc<Registry>,
    SubagentControl,
    mpsc::UnboundedReceiver<AgentUpdate>,
) {
    let (updates, receiver) = mpsc::unbounded_channel();
    let registry = Arc::new(Registry::new(updates));
    let control = SubagentControl {
        registry: Arc::clone(&registry),
    };
    (registry, control, receiver)
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
        .build()
}
