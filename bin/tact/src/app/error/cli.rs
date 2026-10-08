//! Errors from commands that run once and exit, reported only through the process diagnostic.
//!
//! The diagnostic renders every error source on its own line, so these messages name the failed
//! step without repeating their cause.

use crate::app::update::UpdateError;
use tact_memory::transfer::TransferError;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum CliError {
    #[error("--resume is only available in interactive mode and cannot be used with a subcommand")]
    ResumeWithCommand,
    #[error("update failed")]
    Update(#[source] UpdateError),
    #[error("memory transfer failed")]
    MemoryTransfer(#[source] TransferError),
}
