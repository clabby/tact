//! The in-process subagent runtime.
//!
//! [`Registry`] is the shared owner behind [`Subagents`]. Its synchronous bookkeeping lives in
//! `state` behind one async mutex that no flow holds across an await. Each child's turns run in
//! a dedicated harness actor (see `crate::harness`), which reports turn starts, completions,
//! and closes back to the registry; the registry republishes them as [`ScopedAgentUpdate`]s.
//! `messaging` and `lifecycle` hold the multi-step flows that coordinate registry state with
//! harnesses, and `policy` decides which models and efforts a parent may delegate to.

mod lifecycle;
mod messaging;
mod policy;
mod state;

use crate::{
    Speed,
    capacity::{Capacity, TurnCapacity},
    error::{SpawnError, SubagentError},
    harness::{self},
    model::{
        AgentContext, AgentDescriptor, AgentId, AgentStatus, AgentUpdate, ScopedAgentUpdate,
        SubagentRuntimeId,
    },
    output::OutputContract,
    turn::TurnSlot,
};
pub(crate) use messaging::DelegationChange;
use nanocodex::{AgentEvents, HarnessModel as Model, Nanocodex, NanocodexError, Thinking};
use policy::thinking_rank;
use serde_json::Value;
pub(crate) use state::{AgentDirectoryEntry, AgentSummary};
use state::{AgentReservation, ChildSession, RegistryState, TurnSteer, complete_session};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

pub(crate) struct Registry {
    id: SubagentRuntimeId,
    state: tokio::sync::Mutex<RegistryState>,
    pub(super) updates: mpsc::UnboundedSender<ScopedAgentUpdate>,
    revision: watch::Sender<u64>,
    capacity: Capacity,
    message_lock: tokio::sync::Mutex<()>,
    agent_factory: OnceLock<AgentFactory>,
    claude_enabled: AtomicBool,
}

/// Owns the clean-agent recipe and the settings applied to the next spawn.
/// Per-agent tool factories hold only a weak registry reference, keeping this ownership acyclic.
struct AgentFactory {
    build: Box<AgentBuilder>,
    settings: Mutex<AgentSettings>,
}

type AgentBuilder = dyn Fn(Model, Thinking, Speed) -> Result<(Nanocodex, AgentEvents), NanocodexError>
    + Send
    + Sync;

#[derive(Clone, Copy)]
struct AgentSettings {
    max_thinking: Thinking,
    speed: Speed,
}

/// Identifies whether a tool call belongs to a coordinating root agent.
///
/// Clean subagents are registered by session before they can execute a turn. Root sessions and
/// user-created forks are intentionally absent from that map and retain mutation authority.
#[derive(Clone)]
pub struct RootAgentAuthority {
    registry: Weak<Registry>,
}

/// Failure to establish root-session authority.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuthorityError {
    /// The owning subagent runtime has already closed.
    #[error("subagent runtime is closed")]
    RuntimeClosed,
    /// The caller is a registered child session.
    #[error("operation is only available to root agents")]
    ChildSession,
}

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
            message_lock: tokio::sync::Mutex::new(()),
            agent_factory: OnceLock::new(),
            claude_enabled: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_agent_factory<F>(
        &self,
        max_thinking: Thinking,
        speed: Speed,
        factory: F,
    ) -> Result<(), NanocodexError>
    where
        F: Fn(Model, Thinking, Speed) -> Result<(Nanocodex, AgentEvents), NanocodexError>
            + Send
            + Sync
            + 'static,
    {
        self.agent_factory
            .set(AgentFactory {
                build: Box::new(factory),
                settings: Mutex::new(AgentSettings {
                    max_thinking,
                    speed,
                }),
            })
            .map_err(|_| {
                NanocodexError::InvalidRequest("subagent factory is already configured".to_owned())
            })
    }

    pub(super) fn spawn_agent(
        &self,
        model: Model,
        thinking: Thinking,
    ) -> Result<(Nanocodex, AgentEvents), SubagentError> {
        if !crate::SUPPORTED_MODELS.contains(&model) {
            return Err(SpawnError::ModelNotOffered.into());
        }
        if matches!(model, Model::Claude(_)) && !self.claude_enabled() {
            return Err(SpawnError::ClaudeDisabled.into());
        }
        if !model.supports_thinking(thinking) {
            return Err(SpawnError::UnsupportedThinking { model, thinking }.into());
        }
        let factory = self.agent_factory.get().ok_or(SpawnError::FactoryMissing)?;
        let settings = *factory
            .settings
            .lock()
            .expect("subagent settings lock should not be poisoned");
        if thinking_rank(thinking)? > thinking_rank(settings.max_thinking)? {
            return Err(SpawnError::ThinkingExceedsMaximum {
                requested: thinking,
                maximum: settings.max_thinking,
            }
            .into());
        }
        Ok((factory.build)(model, thinking, settings.speed)?)
    }

    pub(super) fn claude_enabled(&self) -> bool {
        self.claude_enabled.load(Ordering::Relaxed)
    }

    fn set_agent_max_thinking(&self, max_thinking: Thinking) {
        if let Some(factory) = self.agent_factory.get() {
            factory
                .settings
                .lock()
                .expect("subagent settings lock should not be poisoned")
                .max_thinking = max_thinking;
        }
    }

    fn set_agent_speed(&self, speed: Speed) {
        if let Some(factory) = self.agent_factory.get() {
            factory
                .settings
                .lock()
                .expect("subagent settings lock should not be poisoned")
                .speed = speed;
        }
    }

    pub(super) fn reserve_turn(&self) -> Result<TurnCapacity, SubagentError> {
        self.capacity.reserve()
    }

    pub(super) fn set_max_concurrency(&self, limit: usize) {
        self.capacity.set_limit(limit);
    }

    async fn is_root_session(&self, session_id: &str) -> bool {
        !self
            .state
            .lock()
            .await
            .root_by_session
            .contains_key(session_id)
    }

    pub(super) async fn reserve(
        &self,
        session_id: &str,
    ) -> Result<AgentReservation, SubagentError> {
        self.state.lock().await.reserve_for(session_id)
    }

    pub(super) async fn submit_result(
        &self,
        session_id: &str,
        turn_token: u64,
        output: Value,
    ) -> Result<(), SubagentError> {
        self.state
            .lock()
            .await
            .submit_result(session_id, turn_token, output)
    }

    pub(super) async fn begin_turn_steer(
        &self,
        root_session_id: &str,
        id: AgentId,
    ) -> Option<TurnSteer> {
        self.state
            .lock()
            .await
            .begin_turn_steer(root_session_id, id)
    }

    pub(super) async fn finish_turn_steer(
        &self,
        root_session_id: &str,
        steer: TurnSteer,
        committed: bool,
    ) {
        self.state
            .lock()
            .await
            .finish_turn_steer(root_session_id, steer, committed);
    }

    pub(super) async fn insert(
        self: &Arc<Self>,
        root_session_id: String,
        descriptor: AgentDescriptor,
        agent: Nanocodex,
        event_task: JoinHandle<()>,
        contract: OutputContract,
    ) -> Result<(), SubagentError> {
        let OutputContract { validator, schema } = contract;
        let (harness, harness_task) = harness::spawn(
            root_session_id.clone(),
            descriptor.id,
            agent,
            self.capacity.clone(),
            Arc::downgrade(self),
            schema,
        );
        self.state.lock().await.insert(
            root_session_id,
            descriptor.id,
            descriptor.session_id.clone(),
            ChildSession {
                descriptor,
                event_task: Some(event_task),
                harness: Some(harness),
                harness_task: Some(harness_task),
                status: AgentStatus::Pending,
                turn: TurnSlot::default(),
                output_validator: validator,
                last_output: None,
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
    ) -> Result<(), SubagentError> {
        let harness = self
            .state
            .lock()
            .await
            .harness_in_scope(root_session_id, id)?;
        harness.start(prompt, capacity).await
    }

    pub(super) async fn harness_turn_started(
        &self,
        root_session_id: &str,
        id: AgentId,
    ) -> Option<(u64, AgentContext)> {
        let token = {
            let mut state = self.state.lock().await;
            let session = state
                .scopes
                .get_mut(root_session_id)
                .and_then(|scope| scope.sessions.get_mut(&id))?;
            if !session.status.can_start_turn() {
                None
            } else {
                let token = session.turn.start()?;
                session.status = AgentStatus::Running;
                Some((
                    token,
                    AgentContext {
                        model: session.descriptor.model,
                        thinking: session.descriptor.thinking,
                    },
                ))
            }
        };
        if token.is_some() {
            self.send(
                root_session_id,
                AgentUpdate::Status {
                    id,
                    status: AgentStatus::Running,
                },
            );
            self.changed();
        }
        token
    }

    pub(super) async fn harness_turn_start_failed(
        &self,
        root_session_id: &str,
        id: AgentId,
        error: String,
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
            session.turn.finish();
            if !matches!(session.status, AgentStatus::Closing | AgentStatus::Closed) {
                session.status = AgentStatus::Failed { error };
            }
            session.status.clone()
        };
        self.send(root_session_id, AgentUpdate::Status { id, status });
        self.changed();
    }

    pub(super) async fn harness_turn_finished(
        &self,
        root_session_id: &str,
        id: AgentId,
        result: nanocodex::agent::Result<nanocodex::TurnResult>,
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
            let Some(submitted_output) = session.turn.finish() else {
                return;
            };
            if matches!(session.status, AgentStatus::Closing | AgentStatus::Closed) {
                session.status.clone()
            } else {
                match result {
                    Ok(_) => complete_session(session, submitted_output),
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

    pub(super) async fn harness_closed(&self, root_session_id: &str, id: AgentId) {
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
                session.turn.finish();
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

    async fn runtime_closed(&self, root_session_id: &str, id: AgentId) {
        let harness = {
            let state = self.state.lock().await;
            state
                .scopes
                .get(root_session_id)
                .and_then(|scope| scope.sessions.get(&id))
                .filter(|session| {
                    !matches!(session.status, AgentStatus::Closing | AgentStatus::Closed)
                })
                .and_then(|session| session.harness.clone())
        };
        let Some(harness) = harness else {
            self.harness_closed(root_session_id, id).await;
            return;
        };
        drop(harness.close().await);
    }

    pub(super) fn send(&self, root_session_id: &str, update: AgentUpdate) {
        let _ = send_update(&self.updates, root_session_id, update);
    }

    pub(super) async fn directory(
        &self,
        session_id: &str,
        include_completed: bool,
        include_self: bool,
    ) -> Vec<AgentDirectoryEntry> {
        self.state
            .lock()
            .await
            .directory(session_id, include_completed, include_self)
    }

    fn changed(&self) {
        self.revision.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

impl RootAgentAuthority {
    /// Rejects callers that belong to a child session.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime has closed or `session_id` belongs to a registered child.
    pub async fn require_root(&self, session_id: &str) -> Result<(), AuthorityError> {
        let registry = self
            .registry
            .upgrade()
            .ok_or(AuthorityError::RuntimeClosed)?;
        if registry.is_root_session(session_id).await {
            return Ok(());
        }
        Err(AuthorityError::ChildSession)
    }
}

/// Owns the child sessions, shared capacity, and tool state for one agent runtime.
#[derive(Clone)]
pub struct Subagents {
    pub(crate) registry: Arc<Registry>,
}

/// A non-owning handle used by factories installed into child sessions.
///
/// Keeping tool factories weak prevents the runtime from retaining itself through the child-agent
/// factory it owns.
#[derive(Clone)]
pub struct WeakSubagents {
    pub(crate) registry: Weak<Registry>,
}

impl Subagents {
    /// Enables Claude choices for every root and descendant sharing this registry.
    /// Runtime admission checks the current policy even when a caller retained an older schema.
    pub fn set_claude_enabled(&self, enabled: bool) {
        self.registry
            .claude_enabled
            .store(enabled, Ordering::Relaxed);
    }

    /// Creates an isolated runtime and its typed update stream.
    ///
    /// The receiver carries model events as well as lifecycle changes. Keep it alive and drain it
    /// continuously for the lifetime of the runtime. Dropping it ends event forwarding and makes
    /// later lifecycle changes unobservable. The channel is unbounded so a slow consumer never
    /// stalls a child turn; a consumer that falls behind holds the backlog in memory instead.
    /// A zero concurrency limit creates the runtime but rejects child turns until the limit is
    /// raised.
    pub fn new(max_concurrency: usize) -> (Self, mpsc::UnboundedReceiver<ScopedAgentUpdate>) {
        let (updates, receiver) = mpsc::unbounded_channel();
        let registry = Arc::new(Registry::new(updates, max_concurrency));
        (Self { registry }, receiver)
    }

    /// Returns a non-owning handle for tool factories and other runtime-owned callbacks.
    pub fn downgrade(&self) -> WeakSubagents {
        WeakSubagents {
            registry: Arc::downgrade(&self.registry),
        }
    }

    /// Configures how clean child sessions are constructed.
    ///
    /// The factory must return a new session and its event stream on every call. The runtime
    /// supplies the requested model and thinking effort, bounded by `max_thinking`, plus the current
    /// speed preference. A runtime accepts exactly one factory; a second call returns
    /// [`NanocodexError::InvalidRequest`].
    ///
    /// # Errors
    ///
    /// Returns [`NanocodexError::InvalidRequest`] if a factory is already configured.
    pub fn set_agent_factory<F>(
        &self,
        max_thinking: Thinking,
        speed: Speed,
        factory: F,
    ) -> Result<(), NanocodexError>
    where
        F: Fn(Model, Thinking, Speed) -> Result<(Nanocodex, AgentEvents), NanocodexError>
            + Send
            + Sync
            + 'static,
    {
        self.registry
            .set_agent_factory(max_thinking, speed, factory)
    }

    /// Returns a root-session authority checker for application-owned tools.
    pub fn root_agent_authority(&self) -> RootAgentAuthority {
        self.downgrade().root_agent_authority()
    }

    /// Changes the maximum number of concurrently active child turns.
    ///
    /// Lowering the limit does not cancel active turns. New reservations fail until the active
    /// count falls below the new limit.
    pub fn set_max_concurrency(&self, limit: usize) {
        self.registry.set_max_concurrency(limit);
    }

    /// Changes the maximum reasoning effort allowed for newly created child sessions.
    pub fn set_max_thinking(&self, thinking: Thinking) {
        self.registry.set_agent_max_thinking(thinking);
    }

    /// Changes the speed preference inherited by newly created child sessions.
    pub fn set_speed(&self, speed: Speed) {
        self.registry.set_agent_speed(speed);
    }

    /// Attempts to interrupt every active child without closing reusable sessions.
    ///
    /// Shutdown is best effort and bounded. Individual child failures are reflected in updates.
    pub async fn cancel_all(&self, root_session_id: &str) {
        self.registry.cancel_all(root_session_id).await;
    }

    /// Attempts to close and join every child owned by a root session.
    ///
    /// Shutdown is best effort and bounded. Individual child failures are reflected in updates.
    pub async fn close_all(&self, root_session_id: &str) {
        drop(self.registry.close_all(root_session_id).await);
    }

    /// Returns the identity used to reject updates from a replaced runtime.
    pub fn runtime_id(&self) -> SubagentRuntimeId {
        self.registry.id
    }
}

impl WeakSubagents {
    /// Returns a root-session authority checker that does not keep the runtime alive.
    pub fn root_agent_authority(&self) -> RootAgentAuthority {
        RootAgentAuthority {
            registry: self.registry.clone(),
        }
    }
}

#[cfg(test)]
fn channel(
    max_concurrency: usize,
) -> (
    Arc<Registry>,
    Subagents,
    mpsc::UnboundedReceiver<ScopedAgentUpdate>,
) {
    let (subagents, updates) = Subagents::new(max_concurrency);
    (Arc::clone(&subagents.registry), subagents, updates)
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

#[cfg(test)]
mod tests;
