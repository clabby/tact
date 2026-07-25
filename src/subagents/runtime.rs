//! Async child-agent sessions, turns, and lifecycle orchestration.

use super::{
    capacity::{Capacity, TurnCapacity},
    model::{
        AgentDescriptor, AgentId, AgentStatus, AgentUpdate, ScopedAgentUpdate, SubagentRuntimeId,
    },
    task_tree::TaskTree,
};
use futures_util::future::join_all;
use nanocodex::{AgentEvents, Nanocodex, NanocodexError, TurnControl};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

pub(super) struct ChildSession {
    pub(super) agent: Option<Nanocodex>,
    pub(super) descriptor: AgentDescriptor,
    pub(super) event_task: Option<JoinHandle<()>>,
    pub(super) status: AgentStatus,
    pub(super) active: Option<ActiveTurn>,
    pub(super) next_generation: u64,
    pub(super) last_report: Option<String>,
}

pub(super) struct ActiveTurn {
    pub(super) generation: u64,
    pub(super) cancellation: CancellationToken,
    pub(super) control: Option<TurnControl>,
    pub(super) _capacity: TurnCapacity,
}

pub(crate) struct Registry {
    id: SubagentRuntimeId,
    state: tokio::sync::Mutex<RegistryState>,
    pub(super) updates: mpsc::UnboundedSender<ScopedAgentUpdate>,
    revision: watch::Sender<u64>,
    capacity: Capacity,
}

pub(super) struct TurnLaunch {
    pub(super) root_session_id: String,
    pub(super) id: AgentId,
    pub(super) generation: u64,
    pub(super) agent: Nanocodex,
    pub(super) cancellation: CancellationToken,
}

#[derive(Default)]
pub(super) struct RegistryState {
    root_by_session: HashMap<String, String>,
    scopes: HashMap<String, AgentScope>,
}

#[derive(Default)]
struct AgentScope {
    topology: TaskTree,
    sessions: HashMap<AgentId, ChildSession>,
}

pub(super) struct AgentReservation {
    pub(super) root_session_id: String,
    pub(super) id: AgentId,
    pub(super) parent: Option<AgentId>,
}

pub(super) struct CloseRequest {
    pub(super) root_session_id: String,
    pub(super) ids: Vec<AgentId>,
    pub(super) controls: Vec<TurnControl>,
    pub(super) status_updates: Vec<(AgentId, AgentStatus)>,
}

pub(super) struct ClosedSessions {
    pub(super) summaries: Vec<AgentSummary>,
    pub(super) agents: Vec<Nanocodex>,
    pub(super) event_tasks: Vec<JoinHandle<()>>,
}

#[derive(Clone, Serialize)]
pub(super) struct AgentSummary {
    pub(super) agent_id: AgentId,
    pub(super) role: String,
    pub(super) task: String,
    pub(super) parent_agent_id: Option<AgentId>,
    pub(super) status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_report: Option<String>,
}

impl RegistryState {
    fn reserve_for(&mut self, session_id: &str) -> std::io::Result<AgentReservation> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let parent = self
            .scopes
            .get(&root_session_id)
            .and_then(|scope| scope.topology.agent_for_session(session_id));
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
        let id = self.scope_mut(&root_session_id).topology.reserve(parent)?;
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
        self.scope_mut(&root_session_id).topology.insert(
            id,
            session_id.clone(),
            session.descriptor.parent,
        )?;
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
        Ok(scope
            .topology
            .visible_ids(session_id)
            .into_iter()
            .filter_map(|id| scope.sessions.get(&id).map(ChildSession::summary))
            .collect())
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
        let ids = scope.topology.all_postorder();
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
            .map(|scope| scope.topology.ids())
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
        self.scopes
            .get(root_session_id)
            .ok_or_else(|| std::io::Error::other("subagent scope disappeared"))?
            .topology
            .subtree_postorder(id)
    }

    fn authorize(&self, session_id: &str, id: AgentId) -> std::io::Result<String> {
        let root_session_id = self.root_session_id(session_id);
        self.scopes
            .get(root_session_id)
            .ok_or_else(|| std::io::Error::other(format!("unknown agent_id {id}")))?
            .topology
            .authorize(session_id, id)?;
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

const AGENT_STOP_TIMEOUT: Duration = Duration::from_secs(30);

impl Registry {
    pub(super) fn new(
        updates: mpsc::UnboundedSender<ScopedAgentUpdate>,
        max_concurrency: usize,
    ) -> Self {
        let (revision, _) = watch::channel(0);
        Self {
            id: SubagentRuntimeId::next(),
            state: tokio::sync::Mutex::new(RegistryState::default()),
            updates,
            revision,
            capacity: Capacity::new(max_concurrency),
        }
    }

    pub(super) fn reserve_turn(&self) -> std::io::Result<TurnCapacity> {
        self.capacity.reserve()
    }

    pub(super) fn set_max_concurrency(&self, limit: usize) {
        self.capacity.set_limit(limit);
    }

    pub(super) async fn reserve(&self, session_id: &str) -> std::io::Result<AgentReservation> {
        self.state.lock().await.reserve_for(session_id)
    }

    pub(super) async fn insert(
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

    pub(super) async fn launch_initial_turn(
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

    pub(super) async fn launch_follow_up(
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

    pub(super) fn send(&self, root_session_id: &str, update: AgentUpdate) {
        let _ = send_update(&self.updates, root_session_id, update);
    }

    pub(super) async fn list(&self, session_id: &str) -> std::io::Result<Vec<AgentSummary>> {
        self.state.lock().await.list(session_id)
    }

    pub(super) async fn steer(
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

    pub(super) async fn wait(
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

    pub(super) async fn interrupt(
        &self,
        session_id: &str,
        id: AgentId,
    ) -> std::io::Result<Vec<AgentSummary>> {
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

    pub(super) async fn close(
        &self,
        session_id: &str,
        id: AgentId,
    ) -> std::io::Result<Vec<AgentSummary>> {
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

impl ChildSession {
    pub(super) fn begin_turn(
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

    pub(super) fn summary(&self) -> AgentSummary {
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

pub(super) fn forward_events(
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

#[cfg(test)]
mod tests {
    use super::{
        AgentDescriptor, AgentId, AgentStatus, ChildSession, Registry, RegistryState,
        forward_events,
    };
    use crate::subagents::AgentOrigin;
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
