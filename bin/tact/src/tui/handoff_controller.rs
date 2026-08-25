use crate::{
    app::config::{ReasoningEffort, ReasoningMode},
    core::ConfiguredAgent,
    tui::{pane::PaneId, worker::AuxiliaryError},
};
use nanocodex::Model;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(crate) type HandoffResult = Result<PreparedHandoff, AuxiliaryError>;
pub(crate) type HandoffTask = JoinHandle<HandoffCompletion>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandoffIdentity {
    pub(crate) pane: PaneId,
    pub(crate) pane_generation: u64,
    controller_generation: u64,
}

pub(crate) struct HandoffCompletion {
    pub(crate) identity: HandoffIdentity,
    pub(crate) result: HandoffResult,
}

pub(crate) struct PreparedHandoff {
    pub(crate) prompt: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) reasoning_mode: ReasoningMode,
    pub(crate) fast_mode: bool,
    pub(crate) model: Model,
    pub(crate) configured: ConfiguredAgent,
}

struct ActiveHandoff {
    identity: HandoffIdentity,
    cancellation: CancellationToken,
    task: HandoffTask,
}

pub(crate) struct HandoffController {
    next_generation: u64,
    active: Option<ActiveHandoff>,
}

impl HandoffController {
    pub(crate) const fn new() -> Self {
        Self {
            next_generation: 0,
            active: None,
        }
    }

    pub(crate) fn start(
        &mut self,
        pane: PaneId,
        pane_generation: u64,
        spawn: impl FnOnce(HandoffIdentity, CancellationToken) -> HandoffTask,
    ) -> Option<HandoffIdentity> {
        if self.active.is_some() {
            return None;
        }

        let identity = HandoffIdentity {
            pane,
            pane_generation,
            controller_generation: self.next_generation,
        };
        self.next_generation = self.next_generation.saturating_add(1);
        let cancellation = CancellationToken::new();
        let task = spawn(identity, cancellation.clone());
        self.active = Some(ActiveHandoff {
            identity,
            cancellation,
            task,
        });
        Some(identity)
    }

    pub(crate) fn task_mut(&mut self) -> Option<&mut HandoffTask> {
        self.active.as_mut().map(|handoff| &mut handoff.task)
    }

    pub(crate) fn cancel(&mut self) -> Option<HandoffIdentity> {
        let handoff = self.active.as_ref()?;
        handoff.cancellation.cancel();
        Some(handoff.identity)
    }

    pub(crate) fn complete(&mut self, identity: HandoffIdentity) -> bool {
        let matches = self
            .active
            .as_ref()
            .is_some_and(|handoff| handoff.identity == identity);
        if matches {
            self.active = None;
        }
        matches
    }
}

#[cfg(test)]
mod tests {
    use super::{HandoffCompletion, HandoffController, HandoffIdentity};
    use crate::tui::pane::PaneId;

    fn pending_handoff(
        identity: HandoffIdentity,
        _: tokio_util::sync::CancellationToken,
    ) -> super::HandoffTask {
        tokio::spawn(async move {
            std::future::pending::<()>().await;
            HandoffCompletion {
                identity,
                result: Err(crate::tui::worker::AuxiliaryError::Cancelled),
            }
        })
    }

    #[tokio::test]
    async fn controller_rejects_overlap_and_cancels_its_operation() {
        let mut controller = HandoffController::new();
        let identity = controller
            .start(PaneId::Main, 4, pending_handoff)
            .expect("the first handoff should start");

        assert!(controller.start(PaneId::Main, 4, pending_handoff).is_none());
        assert_eq!(controller.cancel(), Some(identity));
        assert!(controller.task_mut().is_some());
    }
}
