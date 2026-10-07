//! The channel pair joining the terminal event loop to the web server, and the server status
//! the terminal shows.
//!
//! Messages on these channels are the front-end protocol in [`crate::core::protocol`].

use super::tailscale::Tailnet;
use crate::core::protocol::{AuxiliaryRequest, Publication, Publisher, QueryRequest, Request};
use tokio::sync::{mpsc, watch};

/// What the TUI shows for the web interface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WebStatus {
    Starting,
    /// `url` embeds the login credential in its fragment and must only be shown on request.
    Ready {
        url: String,
        /// Present with `web.tailscale`: publishes the server to the tailnet when a sign-in link
        /// for another device is wanted.
        tailnet: Option<Tailnet>,
    },
    Unavailable {
        reason: String,
    },
}

/// The loop's half of the bridge.
pub(crate) struct LoopEnd {
    pub(crate) publisher: Publisher,
    pub(crate) requests: mpsc::UnboundedReceiver<Request>,
    pub(crate) queries: mpsc::UnboundedReceiver<QueryRequest>,
    pub(crate) auxiliary: mpsc::UnboundedReceiver<AuxiliaryRequest>,
    pub(crate) status: watch::Receiver<WebStatus>,
}

/// The server's half of the bridge.
pub(crate) struct WebEnd {
    pub(crate) publications: mpsc::UnboundedReceiver<Publication>,
    pub(crate) requests: mpsc::UnboundedSender<Request>,
    pub(crate) queries: mpsc::UnboundedSender<QueryRequest>,
    pub(crate) auxiliary: mpsc::UnboundedSender<AuxiliaryRequest>,
    pub(crate) status: watch::Sender<WebStatus>,
}

pub(crate) fn bridge() -> (LoopEnd, WebEnd) {
    let (publish, publications) = mpsc::unbounded_channel();
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let (queries_tx, queries) = mpsc::unbounded_channel();
    let (auxiliary_tx, auxiliary) = mpsc::unbounded_channel();
    let (status_tx, status) = watch::channel(WebStatus::Starting);
    (
        LoopEnd {
            publisher: Publisher::new(publish),
            requests,
            queries,
            auxiliary,
            status,
        },
        WebEnd {
            publications,
            requests: requests_tx,
            queries: queries_tx,
            auxiliary: auxiliary_tx,
            status: status_tx,
        },
    )
}
