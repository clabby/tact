//! Handlers for what pane runtimes report: agent events, subagent updates, shell completions, and
//! journal writer completions.

use super::{
    EventLoop,
    panes::{StreamEnd, WriterCompletion},
};
use crate::{
    app::error::Result,
    core::{
        agent_events::ForwardedAgentEvent, pane::PaneId, shell::ShellExecution,
        subagent_updates::ForwardedSubagentUpdate,
    },
    tui::components::AppEvent,
};
use nanocodex::agent::events::AgentEventKind;

impl EventLoop {
    /// Journals an agent event of a pane's current session and shows it.
    pub(super) async fn on_agent_event(
        &mut self,
        event: Option<ForwardedAgentEvent>,
    ) -> Result<()> {
        let Some(event) = event else {
            for pane in self.panes.all_agent_streams_ended() {
                self.show(AppEvent::AgentStreamClosed(pane));
            }
            return Ok(());
        };
        match event {
            ForwardedAgentEvent::Event {
                pane,
                session_id,
                generation,
                event,
            } => {
                let Some(runtime) = self.panes.current_mut(pane, &session_id, generation) else {
                    return Ok(());
                };
                let record = runtime.journal_mut()?.append_agent(event)?;
                // `tool.result` is the canonical completion event for every agent tool, and a tool
                // may have changed the directory the terminal should report.
                let tool_finished = record.agent_kind() == Some(AgentEventKind::ToolResult);
                self.apply(AppEvent::Transcript { pane, record }).await?;
                if tool_finished {
                    self.rereport_active_workspace()?;
                }
            }
            ForwardedAgentEvent::Closed {
                pane,
                session_id,
                generation,
            } => {
                if self
                    .panes
                    .agent_stream_ended(pane, &session_id, generation)?
                    == StreamEnd::Stopped
                {
                    self.show(AppEvent::AgentStreamClosed(pane));
                }
            }
        }
        Ok(())
    }

    pub(super) async fn on_subagent_update(
        &mut self,
        update: ForwardedSubagentUpdate,
    ) -> Result<()> {
        let Some(pane) = self.panes.subagent_pane(&update) else {
            return Ok(());
        };
        self.apply(AppEvent::Subagent {
            pane,
            update: update.update,
        })
        .await
    }

    /// Journals a finished shell command and sends the submission that waited for it.
    pub(super) async fn on_shell_finished(
        &mut self,
        pane: PaneId,
        execution: ShellExecution,
    ) -> Result<()> {
        let Some(runtime) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        let (record, submission) = runtime.finish_shell(execution)?;
        self.apply(AppEvent::Transcript { pane, record }).await?;
        self.show(AppEvent::ShellFinished(pane));
        if let Some(submission) = submission {
            self.panes
                .runtime(pane)?
                .send_submission(submission, &self.worker)?;
        }
        Ok(())
    }

    /// Accounts for a drained journal writer. A failed writer means the transcript can no longer
    /// be recorded, so it shuts the loop down and becomes the loop's error.
    pub(super) async fn on_writer_finished(&mut self, completion: Option<WriterCompletion>) {
        if let Err(error) = self.panes.writer_finished(completion) {
            self.writer_error = Some(error);
            self.shutdown.cancel();
            self.begin_shutdown().await;
        }
    }
}
