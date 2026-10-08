//! Shutdown ordering.
//!
//! [`Lifecycle`] only moves forward. Each loop iteration calls [`EventLoop::advance_shutdown`]
//! before waiting, which performs the work each stage requires on entry:
//!
//! 1. [`Lifecycle::Stopping`]: the shutdown token was cancelled or a journal writer failed.
//!    Terminal input is dropped and background tasks are aborted and awaited.
//! 2. [`Lifecycle::Draining`]: every pane's subagents are asked to close. From here on, shell,
//!    open, handoff, memory, and background tasks are aborted on every iteration, while worker
//!    updates and agent events are still journaled so in-flight turns are recorded.
//! 3. [`Lifecycle::Closing`]: once the worker has stopped, every agent event stream has ended, and
//!    shells and subagent shutdowns have finished, each journal records how its session ended and
//!    is closed.
//! 4. The loop exits once every journal writer has drained.

use super::EventLoop;
use crate::app::error::Result;
use std::ops::ControlFlow;

/// Shutdown progress of the event loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Lifecycle {
    /// Serving input, web requests, and background tasks.
    Running,
    /// Shutdown was requested; subagent shutdown has not started yet.
    Stopping,
    /// No new work is accepted; the loop drains the worker, agent streams, shells, and subagent
    /// shutdowns.
    Draining,
    /// Every journal recorded how its session ended; the loop waits for journal writers.
    Closing,
}

impl Lifecycle {
    pub(super) fn is_running(self) -> bool {
        self == Self::Running
    }

    pub(super) fn stop(&mut self) {
        if self.is_running() {
            *self = Self::Stopping;
        }
    }

    /// Enters the next shutdown stage when its precondition holds and returns it. Draining always
    /// follows stopping; closing waits until the loop's work has `drained`.
    fn advance(&mut self, drained: bool) -> Option<Self> {
        let next = match *self {
            Self::Stopping => Self::Draining,
            Self::Draining if drained => Self::Closing,
            Self::Running | Self::Draining | Self::Closing => return None,
        };
        *self = next;
        Some(next)
    }
}

impl EventLoop {
    /// Moves shutdown forward, returning [`ControlFlow::Break`] once the loop may exit.
    pub(super) fn advance_shutdown(&mut self) -> Result<ControlFlow<()>> {
        if self.lifecycle.is_running() {
            return Ok(ControlFlow::Continue(()));
        }
        self.abort_new_work();
        while let Some(stage) = self.lifecycle.advance(self.is_drained()) {
            match stage {
                Lifecycle::Draining => self.panes.shut_down_all_subagents(),
                Lifecycle::Closing => self.panes.close_journals(self.worker.error())?,
                Lifecycle::Running | Lifecycle::Stopping => {
                    unreachable!("shutdown never returns to an earlier stage")
                }
            }
        }
        Ok(
            if self.lifecycle == Lifecycle::Closing && !self.panes.has_open_writers() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            },
        )
    }

    /// Aborts every task that would start new work or report into a session after shutdown began.
    fn abort_new_work(&mut self) {
        self.tasks.abort_all();
        self.shells.abort_all();
        self.opens.abort_all();
        self.handoff.cancel();
        self.memory.abort_all();
    }

    /// Whether the worker, every agent stream, every shell, and every subagent shutdown finished.
    fn is_drained(&self) -> bool {
        !self.worker.is_running() && self.panes.are_drained() && self.shells.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Lifecycle;

    #[test]
    fn shutdown_closes_subagents_before_journals_and_journals_only_once_drained() {
        let mut lifecycle = Lifecycle::Running;
        assert_eq!(lifecycle.advance(true), None);

        lifecycle.stop();
        assert!(!lifecycle.is_running());
        assert_eq!(lifecycle.advance(true), Some(Lifecycle::Draining));
        assert_eq!(lifecycle.advance(false), None);
        assert_eq!(lifecycle.advance(true), Some(Lifecycle::Closing));
        assert_eq!(lifecycle.advance(true), None);

        lifecycle.stop();
        assert_eq!(lifecycle, Lifecycle::Closing);
    }
}
