//! The front-end the loop serves beside the web interface: the interactive terminal, or nothing
//! for `tact serve`.
//!
//! Headless, web clients are the only front-end. Nothing is drawn and no working directory is
//! reported, but the loop still publishes every change to the web stream. Effects that need the
//! terminal, such as the external editor and the clipboard, fail with an invalid-request error.

use crate::{core::protocol::CommandError, tui::terminal::TerminalSession};
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use std::{
    future, io,
    path::{Path, PathBuf},
};

pub(super) enum Frontend {
    Terminal {
        session: TerminalSession,
        /// Terminal input. It is detached while an external editor owns the terminal and while a
        /// task that must finish before the next keystroke is handled runs.
        input: Option<EventStream>,
        /// The working directory last reported to the terminal.
        reported_workspace: PathBuf,
    },
    Headless,
}

impl Frontend {
    /// Enters the terminal and reports `workspace` as its working directory.
    pub(super) fn terminal(workspace: &Path) -> io::Result<Self> {
        let mut session = TerminalSession::enter()?;
        session.report_working_directory(workspace)?;
        Ok(Self::Terminal {
            session,
            input: Some(EventStream::new()),
            reported_workspace: workspace.to_owned(),
        })
    }

    /// The terminal session, for effects that cannot run without one.
    pub(super) fn session(&mut self) -> Result<&mut TerminalSession, CommandError> {
        match self {
            Self::Terminal { session, .. } => Ok(session),
            Self::Headless => Err(CommandError::Invalid(
                "this needs a terminal, and `tact serve` runs without one".to_owned(),
            )),
        }
    }

    pub(super) fn has_input(&self) -> bool {
        matches!(self, Self::Terminal { input: Some(_), .. })
    }

    pub(super) fn detach_input(&mut self) {
        if let Self::Terminal { input, .. } = self {
            *input = None;
        }
    }

    pub(super) fn attach_input(&mut self) {
        if let Self::Terminal { input, .. } = self {
            input.get_or_insert_with(EventStream::new);
        }
    }

    /// The next terminal event; pending forever while input is detached or there is no terminal.
    pub(super) async fn next_input(&mut self) -> Option<io::Result<Event>> {
        match self {
            Self::Terminal {
                input: Some(input), ..
            } => input.next().await,
            _ => future::pending().await,
        }
    }

    /// Reports `workspace` to the terminal as its working directory. Unless `force` is set, a
    /// workspace equal to the last report is skipped.
    pub(super) fn report_workspace(&mut self, workspace: &Path, force: bool) -> io::Result<()> {
        let Self::Terminal {
            session,
            reported_workspace,
            ..
        } = self
        else {
            return Ok(());
        };
        if !force && workspace == reported_workspace.as_path() {
            return Ok(());
        }
        session.report_working_directory(workspace)?;
        workspace.clone_into(reported_workspace);
        Ok(())
    }
}
