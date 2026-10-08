//! Authenticated HTTP memory client.

mod client;
#[cfg(test)]
mod tests;

pub use client::{RemoteClientError, RemoteMemoryClient, RemoteToken};
