#![doc = include_str!("../README.md")]
#![doc(html_logo_url = "TODO", html_favicon_url = "TODO")]

//! Binary entry point and top-level diagnostic reporting.

mod app;
mod core;
mod tui;

use app::Cli;
use clap::Parser;
use miette::{Report, Result};

#[tokio::main]
async fn main() -> Result<()> {
    Cli::parse().run().await.map_err(Report::new)
}
