//! Pane-tagged forwarding for subagent runtime updates.

use crate::{subagents::AgentUpdate, tui::pane::PaneId};
use tokio::sync::mpsc;

pub(crate) struct ForwardedSubagentUpdate {
    pub(crate) pane: PaneId,
    pub(crate) root_session_id: String,
    pub(crate) generation: u64,
    pub(crate) update: AgentUpdate,
}

pub(crate) fn forward(
    pane: PaneId,
    root_session_id: String,
    generation: u64,
    mut updates: mpsc::UnboundedReceiver<AgentUpdate>,
    sender: mpsc::UnboundedSender<ForwardedSubagentUpdate>,
) {
    tokio::spawn(async move {
        while let Some(update) = updates.recv().await {
            if sender
                .send(ForwardedSubagentUpdate {
                    pane,
                    root_session_id: root_session_id.clone(),
                    generation,
                    update,
                })
                .is_err()
            {
                break;
            }
        }
    });
}
