//! Synchronous registry state: per-root task trees, child sessions, and their summaries.
//!
//! Every method here runs under the registry lock and never awaits, so each transition is atomic
//! with respect to concurrent tool calls.

use crate::{
    error::{RegistryEntry, SubagentError},
    harness::HarnessHandle,
    message::MessageThreads,
    model::{AgentContext, AgentDescriptor, AgentId, AgentStatus, serialize_reasoning_mode},
    task_tree::TaskTree,
    turn::TurnSlot,
};
use jsonschema::Validator;
use nanocodex::{HarnessModel as Model, ReasoningMode};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use tokio::task::JoinHandle;

pub(crate) struct ChildSession {
    pub(crate) descriptor: AgentDescriptor,
    pub(crate) event_task: Option<JoinHandle<()>>,
    pub(crate) harness: Option<HarnessHandle>,
    pub(crate) harness_task: Option<JoinHandle<()>>,
    pub(crate) status: AgentStatus,
    pub(crate) turn: TurnSlot,
    pub(crate) output_validator: Validator,
    pub(crate) last_output: Option<Value>,
}

impl ChildSession {
    pub(crate) fn summary(&self) -> AgentSummary {
        let last_output = if matches!(self.status, AgentStatus::Completed { .. }) {
            None
        } else {
            self.last_output.clone()
        };
        AgentSummary {
            agent_id: self.descriptor.id,
            model: self.descriptor.model,
            reasoning_mode: self.descriptor.reasoning_mode,
            role: self.descriptor.role.clone(),
            task: self.descriptor.task.clone(),
            parent_agent_id: self.descriptor.parent,
            status: self.status.clone(),
            last_output,
        }
    }
}

#[derive(Default)]
pub(crate) struct RegistryState {
    pub(super) root_by_session: HashMap<String, String>,
    pub(super) scopes: HashMap<String, AgentScope>,
}

#[derive(Default)]
pub(super) struct AgentScope {
    pub(super) topology: TaskTree,
    pub(super) sessions: HashMap<AgentId, ChildSession>,
    pub(super) messages: MessageThreads,
}

pub(crate) struct AgentReservation {
    pub(crate) root_session_id: String,
    pub(crate) id: AgentId,
    pub(crate) parent: Option<AgentId>,
    pub(super) parent_context: Option<AgentContext>,
}

#[derive(Clone, Serialize)]
pub(crate) struct AgentSummary {
    pub(crate) agent_id: AgentId,
    pub(crate) model: Model,
    #[serde(serialize_with = "serialize_reasoning_mode")]
    pub(crate) reasoning_mode: ReasoningMode,
    pub(crate) role: String,
    pub(crate) task: String,
    pub(crate) parent_agent_id: Option<AgentId>,
    pub(crate) status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_output: Option<Value>,
}

#[derive(Serialize)]
pub(crate) struct AgentDirectoryEntry {
    pub(crate) agent_id: AgentId,
    pub(crate) model: Model,
    #[serde(serialize_with = "serialize_reasoning_mode")]
    pub(crate) reasoning_mode: ReasoningMode,
    pub(crate) role: String,
    pub(crate) task: String,
    pub(crate) parent_agent_id: Option<AgentId>,
    pub(crate) status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_output: Option<Value>,
    pub(crate) can_message: bool,
    pub(crate) can_manage: bool,
}

pub(crate) struct TurnSteer {
    pub(super) id: AgentId,
    pub(super) token: u64,
}

impl TurnSteer {
    pub(crate) const fn token(&self) -> u64 {
        self.token
    }
}

impl RegistryState {
    pub(super) fn submit_result(
        &mut self,
        session_id: &str,
        turn_token: u64,
        output: Value,
    ) -> Result<(), SubagentError> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let scope = self
            .scopes
            .get_mut(&root_session_id)
            .ok_or(SubagentError::NotSubagent)?;
        let id = scope
            .topology
            .agent_for_session(session_id)
            .ok_or(SubagentError::NotSubagent)?;
        let session = scope
            .sessions
            .get_mut(&id)
            .ok_or(SubagentError::StateDisappeared(RegistryEntry::Session))?;
        session
            .turn
            .submit(turn_token, output, &session.output_validator)
    }

    pub(super) fn begin_turn_steer(
        &mut self,
        root_session_id: &str,
        id: AgentId,
    ) -> Option<TurnSteer> {
        let session = self
            .scopes
            .get_mut(root_session_id)?
            .sessions
            .get_mut(&id)?;
        let token = session.turn.begin_steer()?;
        Some(TurnSteer { id, token })
    }

    pub(super) fn finish_turn_steer(
        &mut self,
        root_session_id: &str,
        steer: TurnSteer,
        committed: bool,
    ) {
        let Some(session) = self
            .scopes
            .get_mut(root_session_id)
            .and_then(|scope| scope.sessions.get_mut(&steer.id))
        else {
            return;
        };
        session.turn.finish_steer(steer.token, committed);
    }

    pub(super) fn reserve_for(
        &mut self,
        session_id: &str,
    ) -> Result<AgentReservation, SubagentError> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let parent = self
            .scopes
            .get(&root_session_id)
            .and_then(|scope| scope.topology.agent_for_session(session_id));
        let parent_context = if let Some(parent) = parent {
            let parent_session = self
                .scopes
                .get(&root_session_id)
                .and_then(|scope| scope.sessions.get(&parent))
                .ok_or(SubagentError::StateDisappeared(RegistryEntry::Parent))?;
            if matches!(
                parent_session.status,
                AgentStatus::Closing | AgentStatus::Closed
            ) {
                return Err(SubagentError::ParentClosing(parent));
            }
            Some(parent_session.descriptor.context())
        } else {
            None
        };
        let mut reservation = self.reserve(&root_session_id, parent)?;
        reservation.parent_context = parent_context;
        Ok(reservation)
    }

    pub(super) fn reserve(
        &mut self,
        session_id: &str,
        parent: Option<AgentId>,
    ) -> Result<AgentReservation, SubagentError> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let id = self.scope_mut(&root_session_id).topology.reserve(parent)?;
        Ok(AgentReservation {
            root_session_id,
            id,
            parent,
            parent_context: None,
        })
    }

    pub(super) fn insert(
        &mut self,
        root_session_id: String,
        id: AgentId,
        session_id: String,
        session: ChildSession,
    ) -> Result<(), SubagentError> {
        if let Some(parent) = session.descriptor.parent {
            let parent_session = self
                .scopes
                .get(&root_session_id)
                .and_then(|scope| scope.sessions.get(&parent))
                .ok_or(SubagentError::UnknownParent(parent))?;
            if matches!(
                parent_session.status,
                AgentStatus::Closing | AgentStatus::Closed
            ) {
                return Err(SubagentError::ParentStopped { parent, child: id });
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

    pub(super) fn harness_in_scope(
        &self,
        root_session_id: &str,
        id: AgentId,
    ) -> Result<HarnessHandle, SubagentError> {
        self.scopes
            .get(root_session_id)
            .and_then(|scope| scope.sessions.get(&id))
            .and_then(|session| session.harness.clone())
            .ok_or(SubagentError::AgentClosed(id))
    }

    pub(super) fn directory(
        &self,
        session_id: &str,
        include_completed: bool,
        include_self: bool,
    ) -> Vec<AgentDirectoryEntry> {
        let root_session_id = self.root_session_id(session_id);
        let Some(scope) = self.scopes.get(root_session_id) else {
            return Vec::new();
        };
        let caller = scope.topology.agent_for_session(session_id);
        let mut ids = scope.topology.ids();
        ids.sort_unstable();
        ids.into_iter()
            .filter(|id| include_self || caller != Some(*id))
            .filter_map(|id| {
                let session = scope.sessions.get(&id)?;
                if !include_completed
                    && !matches!(session.status, AgentStatus::Pending | AgentStatus::Running)
                {
                    return None;
                }
                let can_message = caller != Some(id)
                    && !matches!(
                        session.status,
                        AgentStatus::Pending | AgentStatus::Closing | AgentStatus::Closed
                    );
                let can_manage = can_message && scope.topology.authorize(session_id, id).is_ok();
                Some(AgentDirectoryEntry {
                    agent_id: id,
                    model: session.descriptor.model,
                    reasoning_mode: session.descriptor.reasoning_mode,
                    role: bounded_summary(&session.descriptor.role),
                    task: bounded_summary(&session.descriptor.task),
                    parent_agent_id: session.descriptor.parent,
                    status: session.status.clone(),
                    last_output: if matches!(session.status, AgentStatus::Completed { .. }) {
                        None
                    } else {
                        session.last_output.clone()
                    },
                    can_message,
                    can_manage,
                })
            })
            .collect()
    }

    pub(super) fn summaries(
        &self,
        session_id: &str,
        ids: &[AgentId],
    ) -> Result<Vec<AgentSummary>, SubagentError> {
        let root_session_id = self.root_session_id(session_id);
        for &id in ids {
            self.authorize(session_id, id)?;
        }
        self.summaries_in_scope(root_session_id, ids)
    }

    pub(super) fn summaries_in_scope(
        &self,
        root_session_id: &str,
        ids: &[AgentId],
    ) -> Result<Vec<AgentSummary>, SubagentError> {
        let scope = self
            .scopes
            .get(root_session_id)
            .ok_or(SubagentError::StateDisappeared(RegistryEntry::Scope))?;
        ids.iter()
            .map(|id| {
                scope
                    .sessions
                    .get(id)
                    .map(ChildSession::summary)
                    .ok_or(SubagentError::UnknownAgent(*id))
            })
            .collect()
    }

    pub(super) fn authorize(&self, session_id: &str, id: AgentId) -> Result<String, SubagentError> {
        let root_session_id = self.root_session_id(session_id);
        self.scopes
            .get(root_session_id)
            .ok_or(SubagentError::UnknownAgent(id))?
            .topology
            .authorize(session_id, id)?;
        Ok(root_session_id.to_owned())
    }

    pub(super) fn root_session_id<'a>(&'a self, session_id: &'a str) -> &'a str {
        self.root_by_session
            .get(session_id)
            .map_or(session_id, String::as_str)
    }

    pub(super) fn scope_mut(&mut self, root_session_id: &str) -> &mut AgentScope {
        self.scopes.entry(root_session_id.to_owned()).or_default()
    }
}

pub(super) fn complete_session(session: &mut ChildSession, output: Option<Value>) -> AgentStatus {
    let Some(output) = output else {
        return AgentStatus::Failed {
            error: "subagent turn ended without a valid submit_result call".to_owned(),
        };
    };
    session.last_output = Some(output.clone());
    AgentStatus::Completed { output }
}

pub(super) fn bounded_summary(value: &str) -> String {
    const MAX_BYTES: usize = 160;
    if value.len() <= MAX_BYTES {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= MAX_BYTES)
        .last()
        .unwrap_or_default();
    value[..end].to_owned()
}
