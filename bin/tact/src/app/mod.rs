//! Application boundaries for configuration, authentication, and command dispatch.

pub(crate) mod auth;
pub(crate) mod browser;
mod cli;
pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod herdr;
pub(crate) mod hook;
pub(crate) mod installation;
mod machine;
pub(crate) mod model;
pub(crate) mod secret;
mod shutdown;
pub(crate) mod theme;
pub(crate) mod update;

pub(crate) use cli::Cli;
