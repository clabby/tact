//! Reusable child-agent tools and the typed runtime/UI update boundary.

use futures_util::future::join_all;
use nanocodex::{
    AgentEvent, AgentEvents, AgentHandle, Nanocodex, NanocodexError, Tool, ToolContext,
    ToolDefinition, ToolExecution, ToolInput, ToolResult, Tools, ToolsBuildError, TurnControl,
    async_trait,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

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
                "Starts a reusable clean-room subagent without inherited conversation history and immediately returns its ID."
            }
            Self::Fork => {
                "Starts a reusable subagent from the latest safe model boundary and immediately returns its ID."
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
             compact, evidence-backed report to the parent agent. Your agent ID is {id}. The \
             runtime automatically places agents you delegate beneath you in the task tree.\n\n\
             Delegated task:\n{task}"
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum AgentStatus {
    Pending,
    Running,
    Completed { report: String },
    Interrupted,
    Failed { error: String },
    Closing,
    Closed,
}

impl AgentStatus {
    pub(crate) const fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Closing)
    }

    const fn is_wait_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Interrupted | Self::Failed { .. } | Self::Closed
        )
    }

    const fn can_start_turn(&self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Completed { .. } | Self::Interrupted | Self::Failed { .. }
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
    Status { id: AgentId, status: AgentStatus },
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
    agent: Option<Nanocodex>,
    descriptor: AgentDescriptor,
    event_task: Option<JoinHandle<()>>,
    status: AgentStatus,
    active: Option<ActiveTurn>,
    next_generation: u64,
    last_report: Option<String>,
}

struct ActiveTurn {
    generation: u64,
    cancellation: CancellationToken,
    control: Option<TurnControl>,
    _capacity: TurnCapacity,
}

struct TurnCapacity {
    state: Arc<Mutex<CapacityState>>,
}

struct CapacityState {
    active: usize,
    limit: usize,
}

impl Drop for TurnCapacity {
    fn drop(&mut self) {
        self.state
            .lock()
            .expect("subagent capacity lock should not be poisoned")
            .active -= 1;
    }
}

pub(crate) struct Registry {
    id: SubagentRuntimeId,
    state: tokio::sync::Mutex<RegistryState>,
    updates: mpsc::UnboundedSender<ScopedAgentUpdate>,
    revision: watch::Sender<u64>,
    capacity: Arc<Mutex<CapacityState>>,
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
}

struct AgentReservation {
    root_session_id: String,
    id: AgentId,
    parent: Option<AgentId>,
}

struct TurnLaunch {
    root_session_id: String,
    id: AgentId,
    generation: u64,
    agent: Nanocodex,
    cancellation: CancellationToken,
}

struct CloseRequest {
    root_session_id: String,
    ids: Vec<AgentId>,
    controls: Vec<TurnControl>,
    status_updates: Vec<(AgentId, AgentStatus)>,
}

struct ClosedSessions {
    summaries: Vec<AgentSummary>,
    agents: Vec<Nanocodex>,
    event_tasks: Vec<JoinHandle<()>>,
}

#[derive(Clone, Serialize)]
struct AgentSummary {
    agent_id: AgentId,
    role: String,
    task: String,
    parent_agent_id: Option<AgentId>,
    status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_report: Option<String>,
}

const AGENT_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(300);

impl Registry {
    fn new(updates: mpsc::UnboundedSender<ScopedAgentUpdate>, max_concurrency: usize) -> Self {
        let (revision, _) = watch::channel(0);
        Self {
            id: SubagentRuntimeId::next(),
            state: tokio::sync::Mutex::new(RegistryState::default()),
            updates,
            revision,
            capacity: Arc::new(Mutex::new(CapacityState {
                active: 0,
                limit: max_concurrency,
            })),
        }
    }

    fn reserve_turn(&self) -> std::io::Result<TurnCapacity> {
        let mut capacity = self
            .capacity
            .lock()
            .expect("subagent capacity lock should not be poisoned");
        if capacity.active >= capacity.limit {
            return Err(std::io::Error::other(format!(
                "sub-agent concurrency limit of {} has been reached; try delegation again later",
                capacity.limit
            )));
        }
        capacity.active += 1;
        drop(capacity);
        Ok(TurnCapacity {
            state: Arc::clone(&self.capacity),
        })
    }

    fn set_max_concurrency(&self, limit: usize) {
        self.capacity
            .lock()
            .expect("subagent capacity lock should not be poisoned")
            .limit = limit;
    }

    async fn reserve(&self, session_id: &str) -> std::io::Result<AgentReservation> {
        self.state.lock().await.reserve_for(session_id)
    }

    async fn insert(
        &self,
        root_session_id: String,
        descriptor: AgentDescriptor,
        agent: Nanocodex,
        event_task: JoinHandle<()>,
    ) -> std::io::Result<()> {
        self.state.lock().await.insert(
            root_session_id,
            descriptor.id,
            descriptor.session_id.clone(),
            ChildSession {
                agent: Some(agent),
                descriptor,
                event_task: Some(event_task),
                status: AgentStatus::Pending,
                active: None,
                next_generation: 0,
                last_report: None,
            },
        )?;
        self.changed();
        Ok(())
    }

    async fn launch_initial_turn(
        self: &Arc<Self>,
        root_session_id: &str,
        id: AgentId,
        prompt: String,
        capacity: TurnCapacity,
    ) -> std::io::Result<()> {
        let launch = self
            .state
            .lock()
            .await
            .begin_turn_in_scope(root_session_id, id, capacity)?;
        self.turn_started(&launch.root_session_id, launch.id);
        self.drive_turn(launch, prompt);
        Ok(())
    }

    async fn launch_follow_up(
        self: &Arc<Self>,
        session_id: &str,
        id: AgentId,
        task: String,
    ) -> std::io::Result<()> {
        let capacity = self.reserve_turn()?;
        let (launch, descriptor) = self
            .state
            .lock()
            .await
            .begin_follow_up(session_id, id, &task, capacity)?;
        self.send(&launch.root_session_id, AgentUpdate::Added(descriptor));
        self.turn_started(&launch.root_session_id, launch.id);
        self.drive_turn(launch, task);
        Ok(())
    }

    fn drive_turn(self: &Arc<Self>, launch: TurnLaunch, prompt: String) {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let result = match launch.agent.prompt(prompt).await {
                Ok(turn) => {
                    let control = turn.control();
                    let should_cancel = registry
                        .attach_control(
                            &launch.root_session_id,
                            launch.id,
                            launch.generation,
                            control.clone(),
                        )
                        .await;
                    if should_cancel || launch.cancellation.is_cancelled() {
                        drop(control.cancel().await);
                    }
                    turn.result().await
                }
                Err(error) => Err(error),
            };
            registry
                .turn_finished(
                    &launch.root_session_id,
                    launch.id,
                    launch.generation,
                    result,
                )
                .await;
        });
    }

    async fn attach_control(
        &self,
        root_session_id: &str,
        id: AgentId,
        generation: u64,
        control: TurnControl,
    ) -> bool {
        let mut state = self.state.lock().await;
        let Some(session) = state
            .scopes
            .get_mut(root_session_id)
            .and_then(|scope| scope.sessions.get_mut(&id))
        else {
            return true;
        };
        let Some(active) = session
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
        else {
            return true;
        };
        active.control = Some(control);
        let cancelled = active.cancellation.is_cancelled();
        drop(state);
        self.changed();
        cancelled
    }

    async fn turn_finished(
        &self,
        root_session_id: &str,
        id: AgentId,
        generation: u64,
        result: nanocodex::Result<nanocodex::TurnResult>,
    ) {
        let status = {
            let mut state = self.state.lock().await;
            let Some(session) = state
                .scopes
                .get_mut(root_session_id)
                .and_then(|scope| scope.sessions.get_mut(&id))
            else {
                return;
            };
            if session
                .active
                .as_ref()
                .is_none_or(|active| active.generation != generation)
            {
                return;
            }
            session.active = None;
            if matches!(session.status, AgentStatus::Closing) {
                AgentStatus::Closing
            } else {
                match result {
                    Ok(result) => {
                        session.last_report = Some(result.final_message.clone());
                        AgentStatus::Completed {
                            report: result.final_message,
                        }
                    }
                    Err(NanocodexError::TurnCancelled) => AgentStatus::Interrupted,
                    Err(error) => AgentStatus::Failed {
                        error: error.to_string(),
                    },
                }
            }
            .clone_into(&mut session.status);
            session.status.clone()
        };
        self.send(root_session_id, AgentUpdate::Status { id, status });
        self.changed();
    }

    async fn runtime_closed(&self, root_session_id: &str, id: AgentId) {
        let changed = {
            let mut state = self.state.lock().await;
            let Some(session) = state
                .scopes
                .get_mut(root_session_id)
                .and_then(|scope| scope.sessions.get_mut(&id))
            else {
                return;
            };
            if matches!(session.status, AgentStatus::Closed) {
                false
            } else {
                session.agent = None;
                session.active = None;
                session.status = AgentStatus::Closed;
                true
            }
        };
        if changed {
            self.send(
                root_session_id,
                AgentUpdate::Status {
                    id,
                    status: AgentStatus::Closed,
                },
            );
            self.changed();
        }
    }

    fn send(&self, root_session_id: &str, update: AgentUpdate) {
        let _ = send_update(&self.updates, root_session_id, update);
    }

    async fn list(&self, session_id: &str) -> std::io::Result<Vec<AgentSummary>> {
        self.state.lock().await.list(session_id)
    }

    async fn steer(
        &self,
        session_id: &str,
        id: AgentId,
        message: String,
    ) -> std::io::Result<AgentSummary> {
        let mut revision = self.revision.subscribe();
        let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
        let control = loop {
            if let Some(control) = self.state.lock().await.active_control(session_id, id)? {
                break control;
            }
            timeout_at(deadline, revision.changed())
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("timed out waiting to steer agent {id}"),
                    )
                })?
                .map_err(|_| std::io::Error::other("subagent runtime is closed"))?;
        };
        control.steer(message).await.map_err(|error| {
            std::io::Error::other(format!("could not steer agent {id}: {error}"))
        })?;
        self.state
            .lock()
            .await
            .summaries(session_id, &[id])?
            .into_iter()
            .next()
            .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))
    }

    async fn wait(
        &self,
        session_id: &str,
        ids: &[AgentId],
        duration: Duration,
    ) -> std::io::Result<(Vec<AgentSummary>, bool)> {
        if ids.is_empty() {
            return Err(std::io::Error::other("agent_ids must not be empty"));
        }
        let mut revision = self.revision.subscribe();
        let deadline = Instant::now() + duration;
        loop {
            let summaries = self.state.lock().await.summaries(session_id, ids)?;
            if summaries
                .iter()
                .any(|summary| summary.status.is_wait_terminal())
            {
                return Ok((summaries, false));
            }
            if timeout_at(deadline, revision.changed()).await.is_err() {
                let summaries = self.state.lock().await.summaries(session_id, ids)?;
                return Ok((summaries, true));
            }
        }
    }

    async fn interrupt(&self, session_id: &str, id: AgentId) -> std::io::Result<Vec<AgentSummary>> {
        let (root_session_id, ids, controls) = {
            let mut state = self.state.lock().await;
            state.request_interrupt(session_id, id)?
        };
        self.changed();
        let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
        self.stop_turns(&root_session_id, &ids, controls, deadline)
            .await?;
        self.state
            .lock()
            .await
            .summaries_in_scope(&root_session_id, &ids)
    }

    async fn close(&self, session_id: &str, id: AgentId) -> std::io::Result<Vec<AgentSummary>> {
        let CloseRequest {
            root_session_id,
            ids,
            controls,
            status_updates,
        } = {
            let mut state = self.state.lock().await;
            state.request_close(session_id, id)?
        };
        for (id, status) in status_updates {
            self.send(&root_session_id, AgentUpdate::Status { id, status });
        }
        self.changed();
        self.stop_and_close(root_session_id, ids, controls).await
    }

    async fn close_all(&self, session_id: &str) -> std::io::Result<Vec<AgentSummary>> {
        let CloseRequest {
            root_session_id,
            ids,
            controls,
            status_updates,
        } = {
            let mut state = self.state.lock().await;
            state.request_close_all(session_id)?
        };
        for (id, status) in status_updates {
            self.send(&root_session_id, AgentUpdate::Status { id, status });
        }
        self.changed();
        self.stop_and_close(root_session_id, ids, controls).await
    }

    async fn stop_and_close(
        &self,
        root_session_id: String,
        ids: Vec<AgentId>,
        controls: Vec<TurnControl>,
    ) -> std::io::Result<Vec<AgentSummary>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
        self.stop_turns(&root_session_id, &ids, controls, deadline)
            .await?;
        let ClosedSessions {
            summaries,
            agents,
            event_tasks,
        } = self
            .state
            .lock()
            .await
            .finish_close(&root_session_id, &ids)?;
        drop(agents);
        for summary in &summaries {
            self.send(
                &root_session_id,
                AgentUpdate::Status {
                    id: summary.agent_id,
                    status: AgentStatus::Closed,
                },
            );
        }
        self.changed();
        self.wait_for_event_tasks(event_tasks, deadline).await?;
        Ok(summaries)
    }

    async fn cancel_all(&self, session_id: &str) {
        let (root_session_id, ids, controls) = {
            let mut state = self.state.lock().await;
            state.request_interrupt_all(session_id)
        };
        self.changed();
        let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
        drop(
            self.stop_turns(&root_session_id, &ids, controls, deadline)
                .await,
        );
    }

    async fn stop_turns(
        &self,
        root_session_id: &str,
        ids: &[AgentId],
        controls: Vec<TurnControl>,
        deadline: Instant,
    ) -> std::io::Result<()> {
        let cancellation_result = self.cancel_controls(controls, deadline).await;
        self.wait_until_inactive(root_session_id, ids, deadline)
            .await?;
        // A cancellation command can race with natural turn completion or driver
        // shutdown. Once every turn is inactive, the command error no longer
        // indicates a live resource and must not prevent lifecycle completion.
        drop(cancellation_result);
        Ok(())
    }

    async fn cancel_controls(
        &self,
        controls: Vec<TurnControl>,
        deadline: Instant,
    ) -> std::io::Result<()> {
        let cancellation = async move {
            let results = join_all(
                controls
                    .into_iter()
                    .map(|control| async move { control.cancel().await }),
            )
            .await;
            results
                .into_iter()
                .find_map(|result| match result {
                    Ok(()) | Err(NanocodexError::TurnNotCancellable) => None,
                    Err(error) => Some(error),
                })
                .map_or(Ok(()), |error| {
                    Err(std::io::Error::other(format!(
                        "could not stop subagent turn: {error}"
                    )))
                })
        };
        timeout_at(deadline, cancellation).await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out stopping subagent turns",
            )
        })?
    }

    async fn wait_for_event_tasks(
        &self,
        mut tasks: Vec<JoinHandle<()>>,
        deadline: Instant,
    ) -> std::io::Result<()> {
        if tasks.is_empty() {
            return Ok(());
        }
        let completion = join_all(tasks.iter_mut());
        match timeout_at(deadline, completion).await {
            Ok(results) => results
                .into_iter()
                .find_map(Result::err)
                .map_or(Ok(()), |error| {
                    Err(std::io::Error::other(format!(
                        "subagent event task failed during shutdown: {error}"
                    )))
                }),
            Err(_) => {
                for task in tasks {
                    task.abort();
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for subagent runtimes to close",
                ))
            }
        }
    }

    async fn wait_until_inactive(
        &self,
        root_session_id: &str,
        ids: &[AgentId],
        deadline: Instant,
    ) -> std::io::Result<()> {
        let mut revision = self.revision.subscribe();
        loop {
            if self.state.lock().await.all_inactive(root_session_id, ids)? {
                return Ok(());
            }
            timeout_at(deadline, revision.changed())
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out waiting for subagent turns to stop",
                    )
                })?
                .map_err(|_| std::io::Error::other("subagent runtime is closed"))?;
        }
    }

    fn turn_started(&self, root_session_id: &str, id: AgentId) {
        self.send(
            root_session_id,
            AgentUpdate::Status {
                id,
                status: AgentStatus::Running,
            },
        );
        self.changed();
    }

    fn changed(&self) {
        self.revision.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

impl RegistryState {
    fn reserve_for(&mut self, session_id: &str) -> std::io::Result<AgentReservation> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let parent = self
            .scopes
            .get(&root_session_id)
            .and_then(|scope| scope.agent_for_session(session_id));
        if let Some(parent) = parent {
            let parent_session = self
                .scopes
                .get(&root_session_id)
                .and_then(|scope| scope.sessions.get(&parent))
                .ok_or_else(|| std::io::Error::other("subagent parent disappeared"))?;
            if matches!(
                parent_session.status,
                AgentStatus::Closing | AgentStatus::Closed
            ) {
                return Err(std::io::Error::other(format!(
                    "agent {parent} is closing and cannot spawn children"
                )));
            }
        }
        self.reserve(&root_session_id, parent)
    }

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
            parent,
        })
    }

    fn insert(
        &mut self,
        root_session_id: String,
        id: AgentId,
        session_id: String,
        session: ChildSession,
    ) -> std::io::Result<()> {
        if let Some(parent) = session.descriptor.parent {
            let parent_session = self
                .scopes
                .get(&root_session_id)
                .and_then(|scope| scope.sessions.get(&parent))
                .ok_or_else(|| std::io::Error::other(format!("unknown parent agent {parent}")))?;
            if matches!(
                parent_session.status,
                AgentStatus::Closing | AgentStatus::Closed
            ) {
                return Err(std::io::Error::other(format!(
                    "agent {parent} stopped while spawning child {id}"
                )));
            }
        }
        self.root_by_session
            .insert(session_id, root_session_id.clone());
        self.scope_mut(&root_session_id)
            .sessions
            .insert(id, session);
        Ok(())
    }

    fn begin_follow_up(
        &mut self,
        session_id: &str,
        id: AgentId,
        task: &str,
        capacity: TurnCapacity,
    ) -> std::io::Result<(TurnLaunch, AgentDescriptor)> {
        let root_session_id = self.authorize(session_id, id)?;
        let scope = self
            .scopes
            .get_mut(&root_session_id)
            .ok_or_else(|| std::io::Error::other("subagent scope disappeared"))?;
        let session = scope
            .sessions
            .get_mut(&id)
            .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))?;
        session.descriptor.task = task.to_owned();
        let descriptor = session.descriptor.clone();
        let launch = session.begin_turn(root_session_id, id, capacity)?;
        Ok((launch, descriptor))
    }

    fn begin_turn_in_scope(
        &mut self,
        root_session_id: &str,
        id: AgentId,
        capacity: TurnCapacity,
    ) -> std::io::Result<TurnLaunch> {
        self.scopes
            .get_mut(root_session_id)
            .and_then(|scope| scope.sessions.get_mut(&id))
            .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))?
            .begin_turn(root_session_id.to_owned(), id, capacity)
    }

    fn list(&self, session_id: &str) -> std::io::Result<Vec<AgentSummary>> {
        let root_session_id = self.root_session_id(session_id);
        let Some(scope) = self.scopes.get(root_session_id) else {
            return Ok(Vec::new());
        };
        let caller = scope.agent_for_session(session_id);
        let mut summaries = scope
            .sessions
            .iter()
            .filter(|(id, _)| caller.is_none_or(|caller| scope.is_descendant(**id, caller)))
            .map(|(_, session)| session.summary())
            .collect::<Vec<_>>();
        summaries.sort_by_key(|summary| summary.agent_id);
        Ok(summaries)
    }

    fn summaries(&self, session_id: &str, ids: &[AgentId]) -> std::io::Result<Vec<AgentSummary>> {
        let root_session_id = self.root_session_id(session_id);
        for &id in ids {
            self.authorize(session_id, id)?;
        }
        self.summaries_in_scope(root_session_id, ids)
    }

    fn summaries_in_scope(
        &self,
        root_session_id: &str,
        ids: &[AgentId],
    ) -> std::io::Result<Vec<AgentSummary>> {
        let scope = self
            .scopes
            .get(root_session_id)
            .ok_or_else(|| std::io::Error::other("subagent scope disappeared"))?;
        ids.iter()
            .map(|id| {
                scope
                    .sessions
                    .get(id)
                    .map(ChildSession::summary)
                    .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))
            })
            .collect()
    }

    fn active_control(
        &self,
        session_id: &str,
        id: AgentId,
    ) -> std::io::Result<Option<TurnControl>> {
        let root_session_id = self.authorize(session_id, id)?;
        let session = self
            .scopes
            .get(&root_session_id)
            .and_then(|scope| scope.sessions.get(&id))
            .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))?;
        let Some(active) = &session.active else {
            return Err(std::io::Error::other(format!("agent {id} is not running")));
        };
        Ok(active.control.clone())
    }

    fn request_interrupt(
        &mut self,
        session_id: &str,
        id: AgentId,
    ) -> std::io::Result<(String, Vec<AgentId>, Vec<TurnControl>)> {
        let root_session_id = self.authorize(session_id, id)?;
        let ids = self.subtree_shutdown_order(&root_session_id, id)?;
        let controls = self.request_cancellation(&root_session_id, &ids, false)?;
        Ok((root_session_id, ids, controls))
    }

    fn request_close(&mut self, session_id: &str, id: AgentId) -> std::io::Result<CloseRequest> {
        let root_session_id = self.authorize(session_id, id)?;
        let ids = self.subtree_shutdown_order(&root_session_id, id)?;
        let controls = self.request_cancellation(&root_session_id, &ids, true)?;
        let status_updates = ids
            .iter()
            .copied()
            .map(|id| (id, AgentStatus::Closing))
            .collect();
        Ok(CloseRequest {
            root_session_id,
            ids,
            controls,
            status_updates,
        })
    }

    fn request_close_all(&mut self, session_id: &str) -> std::io::Result<CloseRequest> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let Some(scope) = self.scopes.get(&root_session_id) else {
            return Ok(CloseRequest {
                root_session_id,
                ids: Vec::new(),
                controls: Vec::new(),
                status_updates: Vec::new(),
            });
        };
        let ids = scope.all_shutdown_order();
        let controls = self.request_cancellation(&root_session_id, &ids, true)?;
        let status_updates = ids
            .iter()
            .copied()
            .map(|id| (id, AgentStatus::Closing))
            .collect();
        Ok(CloseRequest {
            root_session_id,
            ids,
            controls,
            status_updates,
        })
    }

    fn request_interrupt_all(
        &mut self,
        session_id: &str,
    ) -> (String, Vec<AgentId>, Vec<TurnControl>) {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let ids = self
            .scopes
            .get(&root_session_id)
            .map(|scope| scope.sessions.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let controls = self
            .request_cancellation(&root_session_id, &ids, false)
            .unwrap_or_default();
        (root_session_id, ids, controls)
    }

    fn request_cancellation(
        &mut self,
        root_session_id: &str,
        ids: &[AgentId],
        closing: bool,
    ) -> std::io::Result<Vec<TurnControl>> {
        let scope = self
            .scopes
            .get_mut(root_session_id)
            .ok_or_else(|| std::io::Error::other("subagent scope disappeared"))?;
        let mut controls = Vec::new();
        for id in ids {
            let session = scope
                .sessions
                .get_mut(id)
                .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))?;
            if closing {
                session.status = AgentStatus::Closing;
            }
            if let Some(active) = &session.active {
                active.cancellation.cancel();
                controls.extend(active.control.iter().cloned());
            }
        }
        Ok(controls)
    }

    fn finish_close(
        &mut self,
        root_session_id: &str,
        ids: &[AgentId],
    ) -> std::io::Result<ClosedSessions> {
        let scope = self
            .scopes
            .get_mut(root_session_id)
            .ok_or_else(|| std::io::Error::other("subagent scope disappeared"))?;
        let mut agents = Vec::new();
        let mut event_tasks = Vec::new();
        for id in ids {
            let session = scope
                .sessions
                .get_mut(id)
                .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))?;
            if session.active.is_some() {
                return Err(std::io::Error::other(format!(
                    "agent {id} is still running"
                )));
            }
            agents.extend(session.agent.take());
            event_tasks.extend(session.event_task.take());
            session.status = AgentStatus::Closed;
        }
        let summaries = ids
            .iter()
            .filter_map(|id| scope.sessions.get(id).map(ChildSession::summary))
            .collect();
        Ok(ClosedSessions {
            summaries,
            agents,
            event_tasks,
        })
    }

    fn all_inactive(&self, root_session_id: &str, ids: &[AgentId]) -> std::io::Result<bool> {
        let scope = self
            .scopes
            .get(root_session_id)
            .ok_or_else(|| std::io::Error::other("subagent scope disappeared"))?;
        Ok(ids.iter().all(|id| {
            scope
                .sessions
                .get(id)
                .is_some_and(|session| session.active.is_none())
        }))
    }

    fn subtree_shutdown_order(
        &self,
        root_session_id: &str,
        id: AgentId,
    ) -> std::io::Result<Vec<AgentId>> {
        let scope = self
            .scopes
            .get(root_session_id)
            .ok_or_else(|| std::io::Error::other("subagent scope disappeared"))?;
        if !scope.sessions.contains_key(&id) {
            return Err(std::io::Error::other(format!("unknown agent_id {id}")));
        }
        let mut order = Vec::new();
        scope.append_subtree_postorder(id, &mut order);
        Ok(order)
    }

    fn authorize(&self, session_id: &str, id: AgentId) -> std::io::Result<String> {
        let root_session_id = self.root_session_id(session_id);
        let scope = self
            .scopes
            .get(root_session_id)
            .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))?;
        if !scope.sessions.contains_key(&id) {
            return Err(std::io::Error::other(format!("unknown agent_id {id}")));
        }
        if let Some(caller) = scope.agent_for_session(session_id)
            && !scope.is_descendant(id, caller)
        {
            return Err(std::io::Error::other(format!(
                "agent {caller} may only manage its descendants"
            )));
        }
        Ok(root_session_id.to_owned())
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

impl AgentScope {
    fn agent_for_session(&self, session_id: &str) -> Option<AgentId> {
        self.sessions
            .iter()
            .find_map(|(&id, session)| (session.descriptor.session_id == session_id).then_some(id))
    }

    fn is_descendant(&self, candidate: AgentId, ancestor: AgentId) -> bool {
        let mut parent = self
            .sessions
            .get(&candidate)
            .and_then(|session| session.descriptor.parent);
        while let Some(id) = parent {
            if id == ancestor {
                return true;
            }
            parent = self
                .sessions
                .get(&id)
                .and_then(|session| session.descriptor.parent);
        }
        false
    }

    fn append_subtree_postorder(&self, id: AgentId, order: &mut Vec<AgentId>) {
        let mut children = self
            .sessions
            .iter()
            .filter_map(|(&child_id, child)| {
                (child.descriptor.parent == Some(id)).then_some(child_id)
            })
            .collect::<Vec<_>>();
        children.sort_unstable();
        for child in children {
            self.append_subtree_postorder(child, order);
        }
        order.push(id);
    }

    fn all_shutdown_order(&self) -> Vec<AgentId> {
        let mut roots = self
            .sessions
            .iter()
            .filter_map(|(&id, session)| session.descriptor.parent.is_none().then_some(id))
            .collect::<Vec<_>>();
        roots.sort_unstable();
        let mut order = Vec::with_capacity(self.sessions.len());
        for root in roots {
            self.append_subtree_postorder(root, &mut order);
        }
        order
    }
}

impl ChildSession {
    fn begin_turn(
        &mut self,
        root_session_id: String,
        id: AgentId,
        capacity: TurnCapacity,
    ) -> std::io::Result<TurnLaunch> {
        if !self.status.can_start_turn() || self.active.is_some() {
            return Err(std::io::Error::other(format!(
                "agent {id} is not idle ({:?})",
                self.status
            )));
        }
        let agent = self
            .agent
            .clone()
            .ok_or_else(|| std::io::Error::other(format!("agent {id} is closed")))?;
        self.next_generation = self.next_generation.saturating_add(1);
        let generation = self.next_generation;
        let cancellation = CancellationToken::new();
        self.active = Some(ActiveTurn {
            generation,
            cancellation: cancellation.clone(),
            control: None,
            _capacity: capacity,
        });
        self.status = AgentStatus::Running;
        Ok(TurnLaunch {
            root_session_id,
            id,
            generation,
            agent,
            cancellation,
        })
    }

    fn summary(&self) -> AgentSummary {
        let last_report = if matches!(self.status, AgentStatus::Completed { .. }) {
            None
        } else {
            self.last_report.clone()
        };
        AgentSummary {
            agent_id: self.descriptor.id,
            role: self.descriptor.role.clone(),
            task: self.descriptor.task.clone(),
            parent_agent_id: self.descriptor.parent,
            status: self.status.clone(),
            last_report,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SubagentControl {
    registry: Arc<Registry>,
}

impl SubagentControl {
    pub(crate) fn set_max_concurrency(&self, limit: usize) {
        self.registry.set_max_concurrency(limit);
    }

    pub(crate) async fn cancel_all(&self, root_session_id: &str) {
        self.registry.cancel_all(root_session_id).await;
    }

    pub(crate) async fn close_all(&self, root_session_id: &str) {
        drop(self.registry.close_all(root_session_id).await);
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

fn forward_events(
    root_session_id: String,
    id: AgentId,
    mut events: AgentEvents,
    start: oneshot::Receiver<()>,
    registry: Weak<Registry>,
    updates: mpsc::UnboundedSender<ScopedAgentUpdate>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if start.await.is_err() {
            return;
        }
        while let Some(event) = events.recv().await {
            if !send_update(&updates, &root_session_id, AgentUpdate::Event { id, event }) {
                return;
            }
        }
        if let Some(registry) = registry.upgrade() {
            registry.runtime_closed(&root_session_id, id).await;
        }
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

pub(crate) fn channel(
    max_concurrency: usize,
) -> (
    Arc<Registry>,
    SubagentControl,
    mpsc::UnboundedReceiver<ScopedAgentUpdate>,
) {
    let (updates, receiver) = mpsc::unbounded_channel();
    let registry = Arc::new(Registry::new(updates, max_concurrency));
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

#[cfg(test)]
mod tests {
    use super::{
        AgentDescriptor, AgentId, AgentOrigin, AgentStatus, ChildSession, Registry, RegistryState,
        forward_events,
    };
    use nanocodex::{
        Nanocodex, NanocodexError, Responses, ResponsesAttempt, ResponsesServiceResponse,
    };
    use std::{
        future::{Pending, pending},
        result::Result as StdResult,
        sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::{
        sync::{Notify, oneshot},
        time::timeout,
    };
    use tower::Service;

    #[derive(Clone)]
    struct PendingService {
        called: Arc<Notify>,
    }

    impl Service<ResponsesAttempt> for PendingService {
        type Response = ResponsesServiceResponse;
        type Error = NanocodexError;
        type Future = Pending<StdResult<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
            self.called.notify_one();
            pending()
        }
    }

    fn pending_agent(called: Arc<Notify>) -> (Nanocodex, nanocodex::AgentEvents) {
        let responses = Responses::builder()
            .service(move || PendingService {
                called: Arc::clone(&called),
            })
            .build();
        Nanocodex::builder("test-key")
            .responses(responses)
            .build()
            .unwrap()
    }

    async fn insert_runtime_session(
        registry: &Arc<Registry>,
        reservation: &super::AgentReservation,
        parent: Option<AgentId>,
        agent: Nanocodex,
        events: nanocodex::AgentEvents,
    ) -> String {
        let session_id = events.request_id().to_owned();
        let descriptor = AgentDescriptor {
            id: reservation.id,
            session_id: session_id.clone(),
            role: format!("agent-{}", reservation.id),
            task: "wait forever".to_owned(),
            origin: AgentOrigin::Spawn,
            parent,
        };
        let (start_events, events_ready) = oneshot::channel();
        let event_task = forward_events(
            reservation.root_session_id.clone(),
            reservation.id,
            events,
            events_ready,
            Arc::downgrade(registry),
            registry.updates.clone(),
        );
        registry
            .insert(
                reservation.root_session_id.clone(),
                descriptor,
                agent,
                event_task,
            )
            .await
            .unwrap();
        start_events.send(()).unwrap();
        session_id
    }

    fn insert_session(
        registry: &mut RegistryState,
        root_session_id: &str,
        id: AgentId,
        session_id: &str,
        parent: Option<AgentId>,
    ) {
        let session = test_session(id, session_id, parent);
        registry
            .insert(
                root_session_id.to_owned(),
                id,
                session.descriptor.session_id.clone(),
                session,
            )
            .unwrap();
    }

    fn test_session(id: AgentId, session_id: &str, parent: Option<AgentId>) -> ChildSession {
        let (agent, _events) = nanocodex::Nanocodex::builder("test-key").build().unwrap();
        let descriptor = AgentDescriptor {
            id,
            session_id: session_id.to_owned(),
            role: format!("agent-{id}"),
            task: "test lifecycle".to_owned(),
            origin: AgentOrigin::Spawn,
            parent,
        };
        ChildSession {
            agent: Some(agent),
            descriptor,
            event_task: Some(tokio::spawn(async {})),
            status: AgentStatus::Pending,
            active: None,
            next_generation: 0,
            last_report: None,
        }
    }

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
    async fn child_sessions_automatically_own_new_subagents() {
        let mut registry = RegistryState::default();
        let parent = registry.reserve("main", None).unwrap();
        insert_session(
            &mut registry,
            &parent.root_session_id,
            parent.id,
            "parent-session",
            None,
        );

        let child = registry.reserve_for("parent-session").unwrap();

        assert_eq!(child.root_session_id, "main");
        assert_eq!(child.parent, Some(parent.id));
    }

    #[test]
    fn concurrency_capacity_is_released_for_later_delegation() {
        let (registry, _control, _updates) = super::channel(1);
        let capacity = registry.reserve_turn().unwrap();

        let error = registry.reserve_turn().err().unwrap();
        assert_eq!(
            error.to_string(),
            "sub-agent concurrency limit of 1 has been reached; try delegation again later"
        );

        drop(capacity);
        assert!(registry.reserve_turn().is_ok());
    }

    #[test]
    fn concurrency_limit_can_change_while_turns_are_active() {
        let (registry, control, _updates) = super::channel(1);
        let _first = registry.reserve_turn().unwrap();

        control.set_max_concurrency(2);
        let _second = registry.reserve_turn().unwrap();
        control.set_max_concurrency(1);

        assert!(registry.reserve_turn().is_err());
    }

    #[tokio::test]
    async fn subagents_can_manage_descendants_but_not_siblings_or_ancestors() {
        let mut registry = RegistryState::default();
        let first = registry.reserve("main", None).unwrap();
        insert_session(
            &mut registry,
            &first.root_session_id,
            first.id,
            "first-session",
            None,
        );
        let second = registry.reserve("main", None).unwrap();
        insert_session(
            &mut registry,
            &second.root_session_id,
            second.id,
            "second-session",
            None,
        );
        let child = registry.reserve_for("first-session").unwrap();
        insert_session(
            &mut registry,
            &child.root_session_id,
            child.id,
            "child-session",
            Some(first.id),
        );

        assert!(registry.summaries("first-session", &[child.id]).is_ok());
        assert!(registry.summaries("first-session", &[second.id]).is_err());
        assert!(registry.summaries("second-session", &[child.id]).is_err());
        assert!(registry.summaries("child-session", &[first.id]).is_err());
        assert_eq!(registry.summaries("main", &[child.id]).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn child_spawn_is_rejected_when_parent_closes_after_reservation() {
        let mut registry = RegistryState::default();
        let parent = registry.reserve("main", None).unwrap();
        insert_session(
            &mut registry,
            &parent.root_session_id,
            parent.id,
            "parent-session",
            None,
        );
        let child = registry.reserve_for("parent-session").unwrap();
        registry
            .scopes
            .get_mut("main")
            .unwrap()
            .sessions
            .get_mut(&parent.id)
            .unwrap()
            .status = AgentStatus::Closed;
        let session = test_session(child.id, "child-session", Some(parent.id));

        let result = registry.insert(
            child.root_session_id,
            child.id,
            session.descriptor.session_id.clone(),
            session,
        );

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn closed_agent_summaries_keep_the_last_completed_report() {
        let (registry, _control, _updates) = super::channel(32);
        let reservation = registry.reserve("main").await.unwrap();
        let mut session = test_session(reservation.id, "child-session", None);
        session.status = AgentStatus::Completed {
            report: "completed work".to_owned(),
        };
        session.last_report = Some("completed work".to_owned());
        registry
            .state
            .lock()
            .await
            .insert(
                reservation.root_session_id.clone(),
                reservation.id,
                session.descriptor.session_id.clone(),
                session,
            )
            .unwrap();

        let summaries = registry.close("main", reservation.id).await.unwrap();

        assert_eq!(summaries[0].status, AgentStatus::Closed);
        assert_eq!(summaries[0].last_report.as_deref(), Some("completed work"));
    }

    #[tokio::test]
    async fn subtree_shutdown_order_includes_every_descendant_before_its_parent() {
        let mut registry = RegistryState::default();
        let parent = registry.reserve("main", None).unwrap();
        insert_session(
            &mut registry,
            &parent.root_session_id,
            parent.id,
            "parent-session",
            None,
        );
        let child = registry.reserve("parent-session", Some(parent.id)).unwrap();
        insert_session(
            &mut registry,
            &child.root_session_id,
            child.id,
            "child-session",
            Some(parent.id),
        );
        let grandchild = registry.reserve("child-session", Some(child.id)).unwrap();
        insert_session(
            &mut registry,
            &grandchild.root_session_id,
            grandchild.id,
            "grandchild-session",
            Some(child.id),
        );

        assert_eq!(
            registry.subtree_shutdown_order("main", parent.id).unwrap(),
            [grandchild.id, child.id, parent.id]
        );
    }

    #[tokio::test]
    async fn interrupt_and_close_stop_recursive_turns_and_preserve_continuation() {
        let (registry, _control, _updates) = super::channel(32);
        let parent_called = Arc::new(Notify::new());
        let child_called = Arc::new(Notify::new());
        let sibling_called = Arc::new(Notify::new());

        let parent = registry.reserve("main").await.unwrap();
        let (parent_agent, parent_events) = pending_agent(Arc::clone(&parent_called));
        let parent_session =
            insert_runtime_session(&registry, &parent, None, parent_agent, parent_events).await;
        registry
            .launch_initial_turn(
                &parent.root_session_id,
                parent.id,
                "parent work".to_owned(),
                registry.reserve_turn().unwrap(),
            )
            .await
            .unwrap();

        let child = registry.reserve(&parent_session).await.unwrap();
        let (child_agent, child_events) = pending_agent(Arc::clone(&child_called));
        insert_runtime_session(
            &registry,
            &child,
            Some(parent.id),
            child_agent,
            child_events,
        )
        .await;
        registry
            .launch_initial_turn(
                &child.root_session_id,
                child.id,
                "child work".to_owned(),
                registry.reserve_turn().unwrap(),
            )
            .await
            .unwrap();

        let sibling = registry.reserve("main").await.unwrap();
        let (sibling_agent, sibling_events) = pending_agent(Arc::clone(&sibling_called));
        insert_runtime_session(&registry, &sibling, None, sibling_agent, sibling_events).await;
        registry
            .launch_initial_turn(
                &sibling.root_session_id,
                sibling.id,
                "sibling work".to_owned(),
                registry.reserve_turn().unwrap(),
            )
            .await
            .unwrap();

        timeout(Duration::from_secs(5), parent_called.notified())
            .await
            .unwrap();
        timeout(Duration::from_secs(5), child_called.notified())
            .await
            .unwrap();
        timeout(Duration::from_secs(5), sibling_called.notified())
            .await
            .unwrap();

        let (running, timed_out) = registry
            .wait("main", &[parent.id, child.id], Duration::from_millis(1))
            .await
            .unwrap();
        assert!(timed_out);
        assert!(
            running
                .iter()
                .all(|summary| summary.status == AgentStatus::Running)
        );

        registry
            .steer("main", child.id, "report sooner".to_owned())
            .await
            .unwrap();
        let interrupted = registry.interrupt("main", parent.id).await.unwrap();
        assert_eq!(
            interrupted
                .iter()
                .map(|summary| (&summary.agent_id, &summary.status))
                .collect::<Vec<_>>(),
            [
                (&child.id, &AgentStatus::Interrupted),
                (&parent.id, &AgentStatus::Interrupted),
            ]
        );
        let (finished, timed_out) = registry
            .wait("main", &[parent.id, child.id], Duration::from_secs(1))
            .await
            .unwrap();
        assert!(!timed_out);
        assert_eq!(finished.len(), 2);
        assert_eq!(
            registry
                .state
                .lock()
                .await
                .summaries("main", &[sibling.id])
                .unwrap()[0]
                .status,
            AgentStatus::Running
        );

        registry
            .launch_follow_up("main", parent.id, "continue".to_owned())
            .await
            .unwrap();
        timeout(Duration::from_secs(5), parent_called.notified())
            .await
            .unwrap();

        let closed = registry.close("main", parent.id).await.unwrap();
        assert_eq!(
            closed
                .iter()
                .map(|summary| (&summary.agent_id, &summary.status))
                .collect::<Vec<_>>(),
            [
                (&child.id, &AgentStatus::Closed),
                (&parent.id, &AgentStatus::Closed),
            ]
        );
        assert_eq!(registry.list("main").await.unwrap().len(), 3);

        let all_closed = registry.close_all("main").await.unwrap();
        assert_eq!(all_closed.len(), 3);
        assert!(
            all_closed
                .iter()
                .all(|summary| summary.status == AgentStatus::Closed)
        );
        let state = registry.state.lock().await;
        assert!(
            state.scopes["main"]
                .sessions
                .values()
                .all(|session| session.agent.is_none() && session.event_task.is_none())
        );
    }

    #[tokio::test]
    async fn root_sessions_cannot_access_each_others_subagents() {
        let mut registry = RegistryState::default();
        let main = registry.reserve("main", None).unwrap();
        let session = test_session(main.id, "main-child", None);
        registry
            .insert(
                main.root_session_id,
                main.id,
                session.descriptor.session_id.clone(),
                session,
            )
            .unwrap();

        assert!(registry.summaries("fork", &[main.id]).is_err());
        assert!(registry.reserve("fork", Some(main.id)).is_err());
    }
}
