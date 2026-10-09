//! Errors from linking, listing, and removing machines.

use std::{io, path::PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum MachineError {
    #[error(
        "machine names are 1 to 32 lowercase letters, digits, and hyphens, starting with a letter or digit"
    )]
    InvalidName,
    #[error("the machine address {0}")]
    InvalidUrl(&'static str),
    #[error("the token is not a Tact web token; run `tact web token` on that machine")]
    InvalidToken,
    #[error(
        "the token is this machine's own web token; never share or sync $TACT_HOME/web between machines"
    )]
    LocalToken,
    #[error("machine `{0}` is already linked; pass --replace to link it again")]
    Exists(String),
    #[error("no machine named `{0}` is linked")]
    Unknown(String),
    #[error("could not reach the machine")]
    Unreachable(#[source] reqwest::Error),
    #[error("the machine did not answer within {} seconds", .0.as_secs())]
    TimedOut(std::time::Duration),
    #[error("the machine refused the token; run `tact web token` on it and link it again")]
    Unauthorized,
    #[error("the machine answered with HTTP status {0}, not as a Tact server")]
    UnexpectedStatus(u16),
    #[error("the machine did not answer as a Tact server")]
    NotTact,
    #[error("could not read the token from standard input")]
    ReadToken(#[source] io::Error),
    /// Parser messages can quote the file, which holds a token, so the cause is not kept.
    #[error("the machine file {0} is malformed")]
    Malformed(PathBuf),
    #[error("failed to {action} {path}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not build the HTTP client")]
    Client(#[source] reqwest::Error),
}
