//! Handoff: replacing a pane's session with a fresh one that continues from a prompt the current
//! agent wrote.
//!
//! The current agent writes the continuation prompt in an auxiliary turn, then a fresh agent is
//! configured off the loop. At most one handoff runs at a time. Its completion is applied only if
//! it is still the active handoff and the pane still runs the session that started it.

use super::{
    EventLoop,
    panes::{InstalledAgent, InstalledSession, PaneSettings},
};
use crate::{
    app::{
        config::{Config, ReasoningEffort, ReasoningMode, Speed},
        error::Result,
    },
    core::{
        ConfiguredAgent,
        pane::PaneId,
        protocol::AuxiliaryError,
        supported_reasoning_mode,
        worker::{AuxiliaryContext, WorkerCommand},
    },
    tui::components::AppEvent,
};
use nanocodex::HarnessModel as Model;
use std::future;
use tokio::{
    sync::oneshot,
    task::{JoinError, JoinHandle},
};
use tokio_util::sync::CancellationToken;

const HANDOFF_PROMPT: &str = concat!(
    "Prepare a self-contained continuation prompt for a new coding agent that will take over this ",
    "thread. Summarize the user's objective and requirements, important decisions and constraints, ",
    "work already completed, the current repository and revision state, relevant files and symbols, ",
    "validation performed, unresolved blockers, and concrete next steps. Preserve exact technical ",
    "details that the next agent would otherwise need to rediscover. Do not continue the task, use ",
    "tools, or address the user. Return only the continuation prompt, ready to be edited and sent ",
    "to the new agent."
);

type HandoffResult = std::result::Result<PreparedHandoff, AuxiliaryError>;
type HandoffTask = JoinHandle<HandoffCompletion>;

/// Identifies one handoff: the pane and session generation it started from, and its position
/// among the controller's handoffs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HandoffIdentity {
    pane: PaneId,
    pane_generation: u64,
    controller_generation: u64,
}

pub(super) struct HandoffCompletion {
    identity: HandoffIdentity,
    result: HandoffResult,
}

/// A continuation prompt with a fresh agent configured to receive it.
struct PreparedHandoff {
    prompt: String,
    effort: ReasoningEffort,
    reasoning_mode: ReasoningMode,
    speed: Speed,
    model: Model,
    configured: ConfiguredAgent,
}

struct ActiveHandoff {
    identity: HandoffIdentity,
    cancellation: CancellationToken,
    task: HandoffTask,
}

/// The handoff in flight, if any.
pub(super) struct HandoffController {
    next_generation: u64,
    active: Option<ActiveHandoff>,
}

impl HandoffController {
    pub(super) const fn new() -> Self {
        Self {
            next_generation: 0,
            active: None,
        }
    }

    /// Starts a handoff with the task `spawn` creates, unless one is already running.
    fn start(
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

    pub(super) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Waits for the active handoff's task. Cancel safe; pending while no handoff runs.
    pub(super) async fn finished(&mut self) -> std::result::Result<HandoffCompletion, JoinError> {
        match &mut self.active {
            Some(handoff) => (&mut handoff.task).await,
            None => future::pending().await,
        }
    }

    /// Asks the active handoff to stop; it still reports its completion.
    pub(super) fn cancel(&self) {
        if let Some(handoff) = &self.active {
            handoff.cancellation.cancel();
        }
    }

    /// Retires the handoff `identity` names, returning whether it was the active one.
    fn complete(&mut self, identity: HandoffIdentity) -> bool {
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

/// Configures the fresh agent for a continuation prompt the current agent returned.
async fn prepare_handoff(
    result: std::result::Result<String, AuxiliaryError>,
    config: Config,
    model: Model,
    cancellation: CancellationToken,
) -> HandoffResult {
    let prompt = result?;
    if prompt.trim().is_empty() {
        return Err(AuxiliaryError::Failed(
            "The handoff agent returned an empty continuation prompt.".to_owned(),
        ));
    }
    if cancellation.is_cancelled() {
        return Err(AuxiliaryError::Cancelled);
    }

    let effort = config.agent().thinking();
    let reasoning_mode = supported_reasoning_mode(model, config.agent().reasoning_mode());
    let speed = config.agent().speed();
    let task = tokio::task::spawn_blocking(move || {
        ConfiguredAgent::from_config_with_session(
            &config,
            effort,
            reasoning_mode,
            model,
            None,
            None,
        )
    });
    let configured = tokio::select! {
        result = task => result
            .map_err(|error| AuxiliaryError::Failed(format!("handoff session task failed: {error}")))?
            .map_err(|error| AuxiliaryError::Failed(format!("Could not start handoff session: {error}")))?,
        () = cancellation.cancelled() => return Err(AuxiliaryError::Cancelled),
    };
    if cancellation.is_cancelled() {
        return Err(AuxiliaryError::Cancelled);
    }
    Ok(PreparedHandoff {
        prompt,
        effort,
        reasoning_mode,
        speed,
        model,
        configured,
    })
}

impl EventLoop {
    /// Asks `pane`'s agent for a continuation prompt and prepares a fresh session for it.
    pub(super) fn start_handoff(&mut self, pane: PaneId) {
        let Some(runtime) = self.panes.get_mut(pane) else {
            self.show(AppEvent::HandoffFailed {
                pane,
                error: "Could not prepare handoff: session pane is no longer available".to_owned(),
            });
            return;
        };
        let pane_generation = runtime.generation;
        let id = runtime.next_turn_id();
        let model = runtime.settings.model;
        let commands = self.worker.commands();
        let config = self.config.with_workspace(
            self.app
                .root(pane)
                .expect("handoff pane exists")
                .workspace()
                .to_owned(),
        );
        let started = self
            .handoff
            .start(pane, pane_generation, move |identity, cancellation| {
                let (completion, result) = oneshot::channel();
                let sent = commands.send(WorkerCommand::Auxiliary {
                    pane,
                    id,
                    prompt: HANDOFF_PROMPT.to_owned().into(),
                    context: AuxiliaryContext::CurrentConversation,
                    shutdown: cancellation.clone(),
                    completion,
                });
                tokio::spawn(async move {
                    let result = if sent.is_err() {
                        Err(AuxiliaryError::Failed(
                            "agent worker stopped before the handoff could start".to_owned(),
                        ))
                    } else {
                        match result.await {
                            Ok(result) => result,
                            Err(_) if cancellation.is_cancelled() => Err(AuxiliaryError::Cancelled),
                            Err(_) => Err(AuxiliaryError::Failed(
                                "agent worker stopped before the handoff completed".to_owned(),
                            )),
                        }
                    };
                    let result = prepare_handoff(result, config, model, cancellation).await;
                    HandoffCompletion { identity, result }
                })
            });
        if started.is_none() {
            self.show(AppEvent::HandoffFailed {
                pane,
                error: "A handoff is already being prepared.".to_owned(),
            });
        }
    }

    /// Installs the fresh session of a finished handoff, unless the handoff or its session was
    /// superseded meanwhile.
    pub(super) fn on_handoff_finished(&mut self, completion: HandoffCompletion) -> Result<()> {
        let HandoffCompletion { identity, result } = completion;
        if !self.handoff.complete(identity) {
            return Ok(());
        }
        let pane = identity.pane;
        if !self
            .panes
            .get(pane)
            .is_some_and(|runtime| runtime.generation == identity.pane_generation)
        {
            return Ok(());
        }
        match result {
            Ok(PreparedHandoff {
                prompt,
                effort,
                reasoning_mode,
                speed,
                model,
                configured,
            }) => {
                let InstalledAgent { session_id, skills } = self.install_agent(
                    pane,
                    configured,
                    InstalledSession::Fresh,
                    PaneSettings::new(effort, reasoning_mode, speed, model),
                )?;
                self.app.session_opened(pane, session_id, Vec::new());
                self.show(AppEvent::HandoffReady {
                    pane,
                    prompt,
                    effort,
                    reasoning_mode,
                    speed,
                    model,
                    skills,
                });
            }
            Err(AuxiliaryError::Cancelled) => self.show(AppEvent::HandoffCancelled(pane)),
            Err(AuxiliaryError::Failed(error)) => {
                self.show(AppEvent::HandoffFailed { pane, error });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{HandoffCompletion, HandoffController, HandoffIdentity, HandoffTask};
    use crate::core::{pane::PaneId, protocol::AuxiliaryError};
    use tokio_util::sync::CancellationToken;

    fn pending_handoff(identity: HandoffIdentity, _: CancellationToken) -> HandoffTask {
        tokio::spawn(async move {
            std::future::pending::<()>().await;
            HandoffCompletion {
                identity,
                result: Err(AuxiliaryError::Cancelled),
            }
        })
    }

    fn cancellable_handoff(
        identity: HandoffIdentity,
        cancellation: CancellationToken,
    ) -> HandoffTask {
        tokio::spawn(async move {
            cancellation.cancelled().await;
            HandoffCompletion {
                identity,
                result: Err(AuxiliaryError::Cancelled),
            }
        })
    }

    #[tokio::test]
    async fn controller_rejects_overlapping_handoffs() {
        let mut controller = HandoffController::new();
        controller
            .start(PaneId::Main, 4, pending_handoff)
            .expect("the first handoff should start");

        assert!(controller.start(PaneId::Main, 4, pending_handoff).is_none());
        assert!(controller.is_active());
    }

    #[tokio::test]
    async fn a_cancelled_handoff_reports_and_retires_before_a_new_one_may_start() {
        let mut controller = HandoffController::new();
        let first = controller
            .start(PaneId::Main, 1, cancellable_handoff)
            .unwrap();
        controller.cancel();

        let completion = controller.finished().await.unwrap();
        assert_eq!(completion.identity, first);
        assert!(matches!(completion.result, Err(AuxiliaryError::Cancelled)));
        assert!(controller.complete(first));
        assert!(!controller.complete(first));
        assert!(!controller.is_active());

        let second = controller.start(PaneId::Main, 1, pending_handoff).unwrap();
        assert_ne!(second, first);
    }
}
