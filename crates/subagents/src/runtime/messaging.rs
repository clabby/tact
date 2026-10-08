//! Directed messages: validation, mailbox admission, delivery, and delegation rollback.

use super::{Registry, state::RegistryState};
use crate::{
    error::{MessageError, RegistryEntry, SubagentError},
    harness::HarnessHandle,
    model::{
        AgentDescriptor, AgentId, AgentMessage, AgentMessageUpdate, AgentStatus, AgentThread,
        AgentUpdate, MessageDeliveryState, MessageDisposition, MessageId, MessagePriority,
        MessagePurpose, MessageSender, ThreadId,
    },
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct MessageReceipt {
    pub(crate) message_id: MessageId,
    pub(crate) thread_id: ThreadId,
    pub(crate) from: MessageSender,
    pub(crate) to_agent_id: AgentId,
    pub(crate) disposition: MessageDisposition,
}

pub(super) struct PreparedMessage {
    root_session_id: String,
    message: AgentMessage,
    harness: HarnessHandle,
}

pub(crate) struct DelegationChange {
    target: AgentId,
    previous_task: String,
}

impl RegistryState {
    pub(super) fn prepare_message(
        &mut self,
        session_id: &str,
        to: AgentId,
        priority: MessagePriority,
        purpose: MessagePurpose,
        in_reply_to: Option<MessageId>,
        body: String,
    ) -> Result<PreparedMessage, SubagentError> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let scope = self
            .scopes
            .get_mut(&root_session_id)
            .ok_or(SubagentError::UnknownAgent(to))?;
        let from = scope
            .topology
            .agent_for_session(session_id)
            .map_or(MessageSender::Root, |agent_id| MessageSender::Agent {
                agent_id,
            });
        if from.agent_id() == Some(to) {
            return Err(MessageError::SelfAddressed.into());
        }
        if purpose == MessagePurpose::Delegate {
            scope.topology.authorize(session_id, to)?;
        }
        let target = scope
            .sessions
            .get(&to)
            .ok_or(SubagentError::UnknownAgent(to))?;
        if matches!(target.status, AgentStatus::Pending) {
            return Err(SubagentError::RecipientPending(to));
        }
        if matches!(target.status, AgentStatus::Closing | AgentStatus::Closed) {
            return Err(SubagentError::RecipientStopped {
                agent: to,
                status: target.status.clone(),
            });
        }
        let harness = target
            .harness
            .clone()
            .ok_or(SubagentError::AgentClosed(to))?;
        let message = scope
            .messages
            .prepare(from, to, priority, purpose, in_reply_to, body)?;
        Ok(PreparedMessage {
            root_session_id,
            message,
            harness,
        })
    }

    pub(super) fn commit_message(
        &mut self,
        root_session_id: &str,
        message: AgentMessage,
    ) -> Result<AgentThread, SubagentError> {
        let scope = self
            .scopes
            .get_mut(root_session_id)
            .ok_or(SubagentError::StateDisappeared(RegistryEntry::Scope))?;
        Ok(scope.messages.commit(message))
    }

    pub(super) fn rollback_message(&mut self, root_session_id: &str, id: MessageId) {
        if let Some(scope) = self.scopes.get_mut(root_session_id) {
            scope.messages.rollback(id);
        }
    }

    pub(super) fn begin_delegation(
        &mut self,
        root_session_id: &str,
        id: MessageId,
    ) -> Option<(DelegationChange, AgentDescriptor)> {
        let scope = self.scopes.get_mut(root_session_id)?;
        let message = scope.messages.message(id)?;
        if message.purpose != MessagePurpose::Delegate {
            return None;
        }
        let target = scope.sessions.get_mut(&message.to)?;
        let previous_task = std::mem::replace(&mut target.descriptor.task, message.body);
        Some((
            DelegationChange {
                target: message.to,
                previous_task,
            },
            target.descriptor.clone(),
        ))
    }

    pub(super) fn rollback_delegation(
        &mut self,
        root_session_id: &str,
        change: DelegationChange,
    ) -> Option<AgentDescriptor> {
        let target = self
            .scopes
            .get_mut(root_session_id)?
            .sessions
            .get_mut(&change.target)?;
        target.descriptor.task = change.previous_task;
        Some(target.descriptor.clone())
    }

    pub(super) fn thread_for_message(
        &self,
        root_session_id: &str,
        id: MessageId,
    ) -> Option<AgentThread> {
        self.scopes
            .get(root_session_id)
            .and_then(|scope| scope.messages.thread_for_message(id))
    }

    pub(super) fn mark_message_admitted(
        &mut self,
        root_session_id: &str,
        id: MessageId,
        disposition: MessageDisposition,
    ) {
        if let Some(scope) = self.scopes.get_mut(root_session_id) {
            scope.messages.mark_admitted(id, disposition);
        }
    }

    pub(super) fn mark_message_terminal(&mut self, root_session_id: &str, id: MessageId) {
        if let Some(scope) = self.scopes.get_mut(root_session_id) {
            scope.messages.mark_terminal(id);
        }
    }
}

impl Registry {
    pub(crate) async fn send_message(
        &self,
        session_id: &str,
        to: AgentId,
        priority: MessagePriority,
        purpose: MessagePurpose,
        in_reply_to: Option<MessageId>,
        body: String,
    ) -> Result<MessageReceipt, SubagentError> {
        let _message_guard = self.message_lock.lock().await;
        let prepared = self.state.lock().await.prepare_message(
            session_id,
            to,
            priority,
            purpose,
            in_reply_to,
            body,
        )?;
        let delivery = prepared
            .harness
            .enqueue_delivery(prepared.message.clone())?;
        self.state
            .lock()
            .await
            .commit_message(&prepared.root_session_id, prepared.message.clone())?;
        let disposition = match delivery.release().await {
            Ok(disposition) => disposition,
            Err(error) => {
                self.state
                    .lock()
                    .await
                    .rollback_message(&prepared.root_session_id, prepared.message.id);
                return Err(error);
            }
        };
        Ok(MessageReceipt {
            message_id: prepared.message.id,
            thread_id: prepared.message.thread_id,
            from: prepared.message.from,
            to_agent_id: prepared.message.to,
            disposition,
        })
    }

    pub(crate) async fn message_admitted(
        &self,
        root_session_id: &str,
        id: MessageId,
        disposition: MessageDisposition,
    ) {
        let thread = {
            let mut state = self.state.lock().await;
            let thread = state.thread_for_message(root_session_id, id);
            state.mark_message_admitted(root_session_id, id, disposition);
            thread
        };
        let Some(thread) = thread else {
            return;
        };
        self.send(
            root_session_id,
            AgentUpdate::Message(AgentMessageUpdate {
                message_id: id,
                thread,
                delivery: MessageDeliveryState::Admitted { disposition },
            }),
        );
        self.changed();
    }

    pub(crate) async fn message_rejected(&self, root_session_id: &str, id: MessageId) {
        self.state
            .lock()
            .await
            .rollback_message(root_session_id, id);
    }

    pub(crate) async fn message_delivered(
        &self,
        root_session_id: &str,
        id: MessageId,
        disposition: MessageDisposition,
    ) {
        self.publish_message_state(
            root_session_id,
            id,
            MessageDeliveryState::Delivered { disposition },
        )
        .await;
    }

    pub(crate) async fn begin_message_delivery(
        &self,
        root_session_id: &str,
        id: MessageId,
    ) -> Option<DelegationChange> {
        let (change, descriptor) = self
            .state
            .lock()
            .await
            .begin_delegation(root_session_id, id)?;
        self.send(root_session_id, AgentUpdate::Added(descriptor));
        self.changed();
        Some(change)
    }

    pub(crate) async fn rollback_message_delivery(
        &self,
        root_session_id: &str,
        change: DelegationChange,
    ) {
        let descriptor = self
            .state
            .lock()
            .await
            .rollback_delegation(root_session_id, change);
        if let Some(descriptor) = descriptor {
            self.send(root_session_id, AgentUpdate::Added(descriptor));
            self.changed();
        }
    }

    pub(crate) async fn message_failed(&self, root_session_id: &str, id: MessageId, error: String) {
        self.publish_message_state(root_session_id, id, MessageDeliveryState::Failed { error })
            .await;
    }

    pub(super) async fn publish_message_state(
        &self,
        root_session_id: &str,
        message_id: MessageId,
        delivery: MessageDeliveryState,
    ) {
        let thread = self
            .state
            .lock()
            .await
            .thread_for_message(root_session_id, message_id);
        let Some(thread) = thread else {
            return;
        };
        self.send(
            root_session_id,
            AgentUpdate::Message(AgentMessageUpdate {
                message_id,
                thread,
                delivery,
            }),
        );
        self.changed();
        self.state
            .lock()
            .await
            .mark_message_terminal(root_session_id, message_id);
    }
}
