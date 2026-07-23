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

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct AgentId(u64);

impl AgentId {
    #[cfg(test)]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    fn next(counter: &mut u64) -> Self {
        *counter = counter.saturating_add(1);
        Self(*counter)
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

pub(crate) struct ScopedAgentUpdate {
    pub(crate) root_session_id: String,
    pub(crate) update: AgentUpdate,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct SubagentRuntimeId(u64);

impl SubagentRuntimeId {
    fn next() -> Self {
        Self(NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

struct ChildSession {
    agent: Nanocodex,
    descriptor: AgentDescriptor,
    _event_task: JoinHandle<()>,
}

pub(crate) struct Registry {
    id: SubagentRuntimeId,
    state: tokio::sync::Mutex<RegistryState>,
    updates: mpsc::UnboundedSender<ScopedAgentUpdate>,
}

#[derive(Default)]
struct RegistryState {
    root_by_session: HashMap<String, String>,
    scopes: HashMap<String, AgentScope>,
}

#[derive(Default)]
struct AgentScope {
    next_id: u64,
    sessions: HashMap<AgentId, ChildSession>,
    active: HashMap<AgentId, TurnControl>,
}

struct AgentReservation {
    root_session_id: String,
    id: AgentId,
}

impl Registry {
    fn new(updates: mpsc::UnboundedSender<ScopedAgentUpdate>) -> Self {
        Self {
            id: SubagentRuntimeId::next(),
            state: tokio::sync::Mutex::new(RegistryState::default()),
            updates,
        }
    }

    async fn reserve(
        &self,
        session_id: &str,
        parent: Option<AgentId>,
    ) -> std::io::Result<AgentReservation> {
        self.state.lock().await.reserve(session_id, parent)
    }

    async fn insert(
        &self,
        root_session_id: String,
        descriptor: AgentDescriptor,
        agent: Nanocodex,
        event_task: JoinHandle<()>,
    ) {
        self.state.lock().await.insert(
            root_session_id,
            descriptor.id,
            descriptor.session_id.clone(),
            ChildSession {
                agent,
                descriptor,
                _event_task: event_task,
            },
        );
    }

    async fn session(
        &self,
        session_id: &str,
        id: AgentId,
    ) -> Option<(String, Nanocodex, AgentDescriptor)> {
        self.state.lock().await.session(session_id, id)
    }

    fn send(&self, root_session_id: &str, update: AgentUpdate) {
        let _ = send_update(&self.updates, root_session_id, update);
    }

    async fn run(
        &self,
        root_session_id: &str,
        id: AgentId,
        turn: Turn,
    ) -> nanocodex::Result<TurnResult> {
        self.state
            .lock()
            .await
            .scope_mut(root_session_id)
            .active
            .insert(id, turn.control());
        let result = turn.result().await;
        self.state
            .lock()
            .await
            .scope_mut(root_session_id)
            .active
            .remove(&id);
        result
    }

    async fn cancel_all(&self, session_id: &str) {
        let controls = self.state.lock().await.active_controls(session_id);
        for control in controls {
            drop(control.cancel().await);
        }
    }
}

impl RegistryState {
    fn reserve(
        &mut self,
        session_id: &str,
        parent: Option<AgentId>,
    ) -> std::io::Result<AgentReservation> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let scope = self.scope_mut(&root_session_id);
        if let Some(parent) = parent
            && !scope.sessions.contains_key(&parent)
        {
            return Err(std::io::Error::other(format!(
                "unknown parent_agent_id {parent}"
            )));
        }
        let id = AgentId::next(&mut scope.next_id);
        Ok(AgentReservation {
            root_session_id,
            id,
        })
    }

    fn insert(
        &mut self,
        root_session_id: String,
        id: AgentId,
        session_id: String,
        session: ChildSession,
    ) {
        self.root_by_session
            .insert(session_id, root_session_id.clone());
        self.scope_mut(&root_session_id)
            .sessions
            .insert(id, session);
    }

    fn session(
        &self,
        session_id: &str,
        id: AgentId,
    ) -> Option<(String, Nanocodex, AgentDescriptor)> {
        let root_session_id = self.root_session_id(session_id);
        self.scopes
            .get(root_session_id)?
            .sessions
            .get(&id)
            .map(|session| {
                (
                    root_session_id.to_owned(),
                    session.agent.clone(),
                    session.descriptor.clone(),
                )
            })
    }

    fn active_controls(&self, session_id: &str) -> Vec<TurnControl> {
        self.scopes
            .get(self.root_session_id(session_id))
            .into_iter()
            .flat_map(|scope| scope.active.values().cloned())
            .collect()
    }

    fn root_session_id<'a>(&'a self, session_id: &'a str) -> &'a str {
        self.root_by_session
            .get(session_id)
            .map_or(session_id, String::as_str)
    }

    fn scope_mut(&mut self, root_session_id: &str) -> &mut AgentScope {
        self.scopes.entry(root_session_id.to_owned()).or_default()
    }
}

#[derive(Clone)]
pub(crate) struct SubagentControl {
    registry: Arc<Registry>,
}

impl SubagentControl {
    pub(crate) async fn cancel_all(&self, root_session_id: &str) {
        self.registry.cancel_all(root_session_id).await;
    }

    pub(crate) fn runtime_id(&self) -> SubagentRuntimeId {
        self.registry.id
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

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let AgentTask {
            role,
            task,
            parent_agent_id,
        } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let reservation = registry
            .reserve(context.session_id, parent_agent_id)
            .await?;
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
            parent: parent_agent_id,
        };
        registry.send(
            &reservation.root_session_id,
            AgentUpdate::Added(descriptor.clone()),
        );
        let event_task = forward_events(
            reservation.root_session_id.clone(),
            id,
            events,
            registry.updates.clone(),
        );
        registry
            .insert(
                reservation.root_session_id.clone(),
                descriptor,
                child.clone(),
                event_task,
            )
            .await;

        let turn = child.prompt(self.origin.prompt(id, &task)).await?;
        let result = registry.run(&reservation.root_session_id, id, turn).await?;
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

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let FollowUpTask { agent_id, task } = input.decode_json()?;
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let (root_session_id, child, mut descriptor) = registry
            .session(context.session_id, agent_id)
            .await
            .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {agent_id}")))?;
        descriptor.task.clone_from(&task);
        registry.send(&root_session_id, AgentUpdate::Added(descriptor));
        let turn = child.prompt(task).await?;
        let result = registry.run(&root_session_id, agent_id, turn).await?;
        Ok(ToolExecution::json(&FollowUpReport {
            agent_id,
            report: result.final_message,
        }))
    }
}

fn forward_events(
    root_session_id: String,
    id: AgentId,
    mut events: AgentEvents,
    updates: mpsc::UnboundedSender<ScopedAgentUpdate>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if !send_update(&updates, &root_session_id, AgentUpdate::Event { id, event }) {
                return;
            }
        }
        let _ = send_update(&updates, &root_session_id, AgentUpdate::Closed { id });
    })
}

fn send_update(
    updates: &mpsc::UnboundedSender<ScopedAgentUpdate>,
    root_session_id: &str,
    update: AgentUpdate,
) -> bool {
    updates
        .send(ScopedAgentUpdate {
            root_session_id: root_session_id.to_owned(),
            update,
        })
        .is_ok()
}

pub(crate) fn channel() -> (
    Arc<Registry>,
    SubagentControl,
    mpsc::UnboundedReceiver<ScopedAgentUpdate>,
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

#[cfg(test)]
mod tests {
    use super::{AgentDescriptor, AgentId, AgentOrigin, ChildSession, RegistryState};

    #[test]
    fn root_sessions_number_subagents_independently() {
        let mut registry = RegistryState::default();

        let main = registry.reserve("main", None).unwrap();
        let fork = registry.reserve("fork", None).unwrap();

        assert_eq!(main.id, AgentId::new(1));
        assert_eq!(main.root_session_id, "main");
        assert_eq!(fork.id, AgentId::new(1));
        assert_eq!(fork.root_session_id, "fork");
    }

    #[test]
    fn descendant_sessions_use_their_root_namespace() {
        let mut registry = RegistryState::default();
        let root = registry.reserve("main", None).unwrap();
        registry
            .root_by_session
            .insert("child".to_owned(), root.root_session_id);

        let descendant = registry.reserve("child", None).unwrap();

        assert_eq!(descendant.id, AgentId::new(2));
        assert_eq!(descendant.root_session_id, "main");
    }

    #[tokio::test]
    async fn root_sessions_cannot_access_each_others_subagents() {
        let mut registry = RegistryState::default();
        let main = registry.reserve("main", None).unwrap();
        let (agent, _events) = nanocodex::Nanocodex::builder("test-key").build().unwrap();
        let descriptor = AgentDescriptor {
            id: main.id,
            session_id: "main-child".to_owned(),
            role: "reviewer".to_owned(),
            task: "review".to_owned(),
            origin: AgentOrigin::Spawn,
            parent: None,
        };
        registry.insert(
            main.root_session_id,
            main.id,
            descriptor.session_id.clone(),
            ChildSession {
                agent,
                descriptor,
                _event_task: tokio::spawn(async {}),
            },
        );

        assert!(registry.session("fork", main.id).is_none());
        assert!(registry.reserve("fork", Some(main.id)).is_err());
    }
}
