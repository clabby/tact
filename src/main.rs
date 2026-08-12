#![doc = include_str!("../README.md")]
#![doc(html_favicon_url = "https://raw.githubusercontent.com/clabby/tact/main/assets/favicon.svg")]

//! Binary entry point and top-level diagnostic reporting.

mod app;
mod core;
mod review;
mod tui;

use app::Cli;
use clap::Parser;
use miette::{Report, Result};

pub(crate) fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[tokio::main]
async fn main() -> Result<()> {
    install_tls_provider();
    Cli::parse().run().await.map_err(Report::new)
}
