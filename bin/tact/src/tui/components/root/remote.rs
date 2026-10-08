//! Web commands applied to one pane with the same effects and preconditions as their keys.

use super::{BlockingTask, BusyDelivery, RootEffect, RootNode, ThreadState, copy_command_argument};
use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode, Speed},
        model,
    },
    core::{
        prompt::{QueueId, Submission},
        protocol::CommandError,
    },
    tui::components::{
        composer::ComposerEvent,
        node::{ComponentUpdate, RenderRequest},
    },
};
use nanocodex::HarnessModel as Model;

/// A web command that applies to one pane.
#[derive(Debug)]
pub(crate) enum PaneCommand {
    SetDraft(String),
    /// Sends the draft, or queues it instead of steering when a turn is running and `queue` is set.
    Submit {
        queue: bool,
    },
    Interrupt,
    Steer(QueueId),
    Dequeue(QueueId),
    Compact,
    SetModel(Model),
    SetEffort(ReasoningEffort),
    SetReasoningMode(ReasoningMode),
    SetSpeed(Speed),
    EditQueued(QueueId, String),
    AttachImage(String),
    Reflect(String),
    Handoff,
}

impl RootNode {
    /// Applies a web command to this pane through the same effects and preconditions as the
    /// equivalent keypress. Local interactions that the command would invalidate (an open overlay,
    /// an inline queue edit) end the way their own cancellation ends them.
    pub(crate) fn remote_command(
        &mut self,
        command: PaneCommand,
    ) -> Result<ComponentUpdate<RootEffect>, CommandError> {
        if !self.interactive {
            return Err(CommandError::Failed(
                "the session is still starting".to_owned(),
            ));
        }
        if self.blocking_task == Some(BlockingTask::Handoff)
            && matches!(command, PaneCommand::Interrupt)
        {
            self.key_confirmation = None;
            return Ok(ComponentUpdate {
                effects: vec![RootEffect::CancelHandoff],
                render: RenderRequest::Immediate,
            });
        }
        if self.blocking_task.is_some() {
            return Err(CommandError::TurnRunning);
        }
        self.check_remote_command(&command)?;
        let mut update = self.end_local_interaction();
        let applied = match command {
            PaneCommand::SetDraft(text) => {
                self.update_composer(ComposerEvent::ReplaceDraft(text), RenderRequest::Immediate)
            }
            PaneCommand::Submit { .. } if self.reflection_input => self.submit_reflection(),
            PaneCommand::Submit { queue } => self.update_composer_with(
                ComposerEvent::Submit,
                RenderRequest::Immediate,
                if queue {
                    BusyDelivery::Queue
                } else {
                    BusyDelivery::Steer
                },
            ),
            PaneCommand::Interrupt => {
                self.key_confirmation = None;
                ComponentUpdate {
                    effects: vec![RootEffect::CancelTurns],
                    render: RenderRequest::Immediate,
                }
            }
            PaneCommand::Steer(id) => self.steer_queued(id),
            PaneCommand::Dequeue(id) => {
                let removed = self.queue.remove_queued(id);
                debug_assert!(removed, "remote queue commands are checked first");
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            PaneCommand::Compact => self.start_compaction()?,
            PaneCommand::SetModel(model) if model == self.composer.model() => {
                ComponentUpdate::none()
            }
            PaneCommand::SetModel(model) => self.apply_model(model),
            PaneCommand::SetEffort(effort) => {
                self.apply_effort(effort, self.preferred_reasoning_mode == ReasoningMode::Pro)
            }
            PaneCommand::SetReasoningMode(mode) => {
                let mut update =
                    self.apply_effort(self.composer.effort(), mode == ReasoningMode::Pro);
                // A session's reasoning mode is fixed when it is created, so a new thread is
                // recreated to apply the choice immediately.
                if self.composer.reasoning_mode() != mode {
                    update
                        .effects
                        .push(RootEffect::SetModel(self.composer.model()));
                }
                update
            }
            PaneCommand::SetSpeed(speed) if speed == self.composer.speed() => {
                ComponentUpdate::none()
            }
            PaneCommand::SetSpeed(speed) => self.apply_speed(speed),
            PaneCommand::EditQueued(id, text) => {
                let edited = self.queue.finish_edit(id, text);
                debug_assert!(edited, "remote queue commands are checked first");
                self.submit_next_queued()
            }
            PaneCommand::AttachImage(data_url) => {
                self.composer.append_image(data_url);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            PaneCommand::Reflect(instructions) => {
                self.start_reflection(Submission::text(instructions))
            }
            PaneCommand::Handoff => self.start_handoff(),
        };
        update.merge(applied);
        Ok(update)
    }

    fn check_remote_command(&self, command: &PaneCommand) -> Result<(), CommandError> {
        match command {
            PaneCommand::Submit { .. } => {
                let draft = self.shared_draft().trim();
                if draft.is_empty() {
                    return Err(CommandError::Invalid("the draft is empty".to_owned()));
                }
                if !self.reflection_input && copy_command_argument(draft).is_some() {
                    return Err(CommandError::NotAvailableRemotely);
                }
            }
            PaneCommand::Interrupt
                if !self.turns.turn_running() && !self.queue.has_pending_steer() =>
            {
                return Err(CommandError::NothingRunning);
            }
            PaneCommand::Steer(_) if !self.turns.turn_running() => {
                return Err(CommandError::NothingRunning);
            }
            PaneCommand::Steer(id) | PaneCommand::Dequeue(id) | PaneCommand::EditQueued(id, _)
                if !self
                    .queue
                    .entries()
                    .any(|entry| entry.id == *id && !entry.steering) =>
            {
                return Err(CommandError::UnknownQueueItem);
            }
            PaneCommand::Compact | PaneCommand::Reflect(_) | PaneCommand::Handoff => {
                self.compaction_allowed()?;
            }
            PaneCommand::AttachImage(data_url)
                if !(data_url.starts_with("data:image/") && data_url.contains(";base64,")) =>
            {
                return Err(CommandError::Invalid(
                    "an attachment must be a base64 image data URL".to_owned(),
                ));
            }
            PaneCommand::SetReasoningMode(mode)
                if !model::reasoning_modes(self.composer.model()).contains(mode) =>
            {
                return Err(CommandError::Invalid(format!(
                    "{} does not support {} reasoning",
                    model::name(self.composer.model()),
                    mode.as_str()
                )));
            }
            PaneCommand::SetModel(_) if self.thread != ThreadState::New => {
                return Err(CommandError::Invalid(
                    "the model can only change before the first prompt".to_owned(),
                ));
            }
            PaneCommand::SetModel(model)
                if !model::available(self.claude_enabled).contains(model) =>
            {
                return Err(CommandError::Invalid(format!("{model} is not available")));
            }
            PaneCommand::SetReasoningMode(_) if self.thread != ThreadState::New => {
                return Err(CommandError::Invalid(
                    "the reasoning mode can only change before the first prompt".to_owned(),
                ));
            }
            PaneCommand::SetEffort(_) | PaneCommand::SetReasoningMode(_)
                if self.effort_locked() =>
            {
                return Err(CommandError::Invalid(
                    "effort is fixed for this Claude session".to_owned(),
                ));
            }
            _ => {}
        }
        Ok(())
    }
}
