//! Interactive terminal runtime.
//!
//! [`run`] serves the terminal and the web front-end from one event loop (see
//! [`event_loop`]). The loop owns the application tree and every pane's runtime, which holds the
//! pane's session lock and transcript journal; the turn worker owns every agent.
//!
//! # Event-loop contract
//!
//! Terminal input, web commands, queries, and auxiliary requests from the bridge, worker updates,
//! agent and subagent events, journal writer completions, and background task completions each
//! arrive on their own `tokio::select!` arm. Arms are polled in random order, so no source can
//! starve another; an arm whose source has nothing to offer, or whose work is disabled during
//! shutdown, is skipped. Each arm's handler runs to completion before the next event is taken, so
//! handlers mutate state without locks. Web commands wait while a task that replaces a pane's agent
//! or settings runs, and terminal input is detached while such a task or an external editor runs.
//! Before waiting, every iteration advances shutdown, publishes changed state to web clients, and
//! draws the terminal when the render scheduler is due.
//!
//! # Shutdown
//!
//! Shutdown runs in this order:
//! 1. The shutdown token is cancelled or a journal writer fails. The loop drops terminal input,
//!    aborts and awaits its background tasks, and disables every arm that would start new work.
//! 2. Shell, open, handoff, and memory tasks are aborted, and each pane's subagents are asked to
//!    close.
//! 3. The worker observes the same token, cancels its turns, and reports that it stopped. Until
//!    then the loop keeps journaling worker updates and agent events, so in-flight turns are
//!    recorded.
//! 4. Once the worker has stopped, every agent event stream has ended, and shells and subagent
//!    shutdowns have finished, each journal records how its session ended and is closed.
//! 5. The loop exits after every journal writer has drained. The terminal is restored, the web
//!    server gets a short grace period, and the main session's ID is returned when it is resumable.

mod clipboard;
mod components;
mod editor;
mod event_loop;
mod format;
mod scheduler;
mod spinner;
mod system_scheme;
mod terminal;

use crate::app::{
    config::Config,
    error::{Result, RuntimeError},
};
use nanocodex::HarnessModel as Model;
use std::io::{self, IsTerminal};
use tokio_util::sync::CancellationToken;

/// The session the terminal opens with.
pub(crate) enum StartupMode {
    NewSession(Model),
    ResumeSession(String),
    /// A new session with the resume selector open over it.
    ResumeSelector(Model),
}

/// Runs the interactive terminal until shutdown, returning the main session's ID when it can be
/// resumed.
pub(crate) async fn run(
    config: Config,
    startup: StartupMode,
    shutdown: CancellationToken,
) -> Result<Option<String>> {
    ensure_interactive()?;
    event_loop::run(config, startup, shutdown).await
}

pub(crate) fn ensure_interactive() -> Result<()> {
    validate_interactive(io::stdin().is_terminal(), io::stdout().is_terminal())
}

fn validate_interactive(stdin: bool, stdout: bool) -> Result<()> {
    if stdin && stdout {
        return Ok(());
    }
    Err(RuntimeError::InteractiveTerminal.into())
}

#[cfg(test)]
mod tests {
    use super::validate_interactive;
    use crate::app::error::{Error, RuntimeError};

    #[test]
    fn bare_non_tty_invocation_points_to_headless_run() {
        let error = validate_interactive(false, true).unwrap_err();

        assert!(matches!(
            error,
            Error::Runtime(RuntimeError::InteractiveTerminal)
        ));
    }
}
