#![doc(html_favicon_url = "https://raw.githubusercontent.com/clabby/tact/main/assets/favicon.svg")]

//! Binary entry point and top-level diagnostic reporting.

mod app;
mod core;
mod search;
mod tui;
mod vcs;
mod web;

use app::Cli;
use clap::Parser;
use miette::{Report, Result};

pub(crate) fn install_tls_provider() {
    tact_runtime::init().expect("initialize Nanocode runtime");
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[tokio::main]
async fn main() -> Result<()> {
    install_tls_provider();
    tact_runtime::scope(Cli::parse().run())
        .await
        .map_err(Report::new)
}
