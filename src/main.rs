#![doc = include_str!("../README.md")]
#![doc(html_logo_url = "TODO", html_favicon_url = "TODO")]

//! Binary entry point and top-level diagnostic reporting.

mod auth;
mod browser;
mod cli;
mod config;
mod core;
mod error;
mod secret;
mod shutdown;
mod subagents;
mod tui;

use clap::Parser;
use cli::Cli;
use miette::{Report, Result};

#[tokio::main]
async fn main() -> Result<()> {
    Cli::parse().run().await.map_err(Report::new)
}
