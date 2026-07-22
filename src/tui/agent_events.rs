//! Lossless forwarding for active and retiring Nanocodex event streams.

use crate::tui::pane::PaneId;
use nanocodex::{AgentEvent, AgentEvents};
use tokio::sync::mpsc;

pub(crate) enum ForwardedAgentEvent {
    Event {
        pane: PaneId,
        session_id: String,
        generation: u64,
        event: AgentEvent,
    },
    Closed {
        pane: PaneId,
        session_id: String,
        generation: u64,
    },
}

pub(crate) fn forward(
    pane: PaneId,
    generation: u64,
    mut events: AgentEvents,
    sender: mpsc::UnboundedSender<ForwardedAgentEvent>,
) {
    let session_id = events.request_id().to_owned();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if sender
                .send(ForwardedAgentEvent::Event {
                    pane,
                    session_id: session_id.clone(),
                    generation,
                    event,
                })
                .is_err()
            {
                return;
            }
        }
        drop(sender.send(ForwardedAgentEvent::Closed {
            pane,
            session_id,
            generation,
        }));
    });
}
