//! Delivery of messages that one live session sends to another through `message_session`.
//!
//! A message becomes a worker steer on the target's pane, so it joins the running turn or starts
//! one when the pane is idle. The sender waits for the worker's verdict, which arrives as the
//! ordinary steer events keyed by a queue identifier that no composer queue entry can use. The
//! target's transcript records the message as received from the sending session, in both cases.

use super::EventLoop;
use crate::{
    app::error::Result,
    core::{
        live_sessions::{Delivery, DeliveryError, LiveSessionMessage},
        pane::PaneId,
        prompt::{QueueId, Submission},
        transcript::{LocalEvent, SessionMessageReceived},
        worker::{WorkerCommand, WorkerError},
    },
    tui::components::AppEvent,
};
use std::collections::HashMap;
use tokio::sync::oneshot;

/// Queue identifiers count down from the top of the range; the composer queue counts up.
const FIRST_MESSAGE_QUEUE_ID: u64 = u64::MAX;

struct PendingMessage {
    received: SessionMessageReceived,
    reply: oneshot::Sender<std::result::Result<Delivery, DeliveryError>>,
}

/// Messages handed to the worker whose delivery has not been reported yet.
pub(super) struct PendingMessages {
    next_id: u64,
    pending: HashMap<QueueId, PendingMessage>,
}

impl PendingMessages {
    pub(super) fn new() -> Self {
        Self {
            next_id: FIRST_MESSAGE_QUEUE_ID,
            pending: HashMap::new(),
        }
    }

    /// Reports a rejected message to its sender. Ignores identifiers of composer steers.
    pub(super) fn fail(&mut self, queue_id: QueueId, error: &WorkerError) {
        if let Some(message) = self.pending.remove(&queue_id) {
            drop(
                message
                    .reply
                    .send(Err(DeliveryError::Rejected(error.to_string()))),
            );
        }
    }

    /// Reports how the target took the message and returns it for the target's transcript.
    /// Returns nothing for identifiers of composer steers.
    pub(super) fn delivered(
        &mut self,
        queue_id: QueueId,
        delivery: Delivery,
    ) -> Option<SessionMessageReceived> {
        let message = self.pending.remove(&queue_id)?;
        drop(message.reply.send(Ok(delivery)));
        Some(message.received)
    }
}

impl EventLoop {
    pub(super) fn on_live_session_message(&mut self, message: LiveSessionMessage) -> Result<()> {
        let prompt = Submission::text(message.prompt());
        let LiveSessionMessage {
            from,
            target,
            text,
            reply,
        } = message;
        let Some(pane) = self
            .panes
            .runtimes()
            .find(|runtime| runtime.session_id == target)
            .map(|runtime| runtime.pane)
        else {
            drop(reply.send(Err(DeliveryError::NotLive(target))));
            return Ok(());
        };
        let queue_id = QueueId::new(self.messages.next_id);
        self.messages.next_id = self.messages.next_id.saturating_sub(1);
        let fallback_id = self.panes.runtime(pane)?.next_turn_id();
        self.messages.pending.insert(
            queue_id,
            PendingMessage {
                received: SessionMessageReceived {
                    from_session_id: from,
                    text,
                },
                reply,
            },
        );
        self.worker.send(WorkerCommand::Steer {
            pane,
            queue_id,
            fallback_id,
            prompt,
        })
    }

    /// Records a message that joined the target's running turn.
    pub(super) fn on_message_admitted(&mut self, pane: PaneId, queue_id: QueueId) -> Result<()> {
        if let Some(received) = self.messages.delivered(queue_id, Delivery::Steered) {
            self.record_received_message(pane, received)?;
        }
        Ok(())
    }

    /// Records a message that started a turn on the idle target. Returns whether `queue_id` named
    /// a message; composer steers are left to the caller.
    pub(super) fn on_message_started(&mut self, pane: PaneId, queue_id: QueueId) -> Result<bool> {
        let Some(received) = self.messages.delivered(queue_id, Delivery::Started) else {
            return Ok(false);
        };
        self.record_received_message(pane, received)?;
        self.show(AppEvent::SteerPromoted { pane, id: queue_id });
        Ok(true)
    }

    fn record_received_message(
        &mut self,
        pane: PaneId,
        received: SessionMessageReceived,
    ) -> Result<()> {
        let Some(runtime) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        let record = runtime.record(LocalEvent::SessionMessageReceived(received))?;
        self.show(AppEvent::Transcript { pane, record });
        Ok(())
    }
}
