//! The agent that answers review prompts.

use crate::core::protocol::{AuxiliaryError, AuxiliaryRequest};
use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// One clean-context prompt for a session's agent.
pub(in crate::web) struct AgentPrompt {
    /// The session whose worker runs the prompt.
    pub(in crate::web) session: String,
    pub(in crate::web) prompt: String,
    /// Cancelled when the result is no longer wanted.
    pub(in crate::web) shutdown: CancellationToken,
}

/// Runs review prompts on a session's worker and returns the agent's final text.
pub(in crate::web) trait ReviewAgent: Send + Sync + 'static {
    fn run(&self, prompt: AgentPrompt) -> BoxFuture<'static, Result<String, AuxiliaryError>>;
}

/// The production [`ReviewAgent`]: auxiliary requests handled by the terminal event loop.
pub(in crate::web) struct BridgeAgent {
    requests: mpsc::UnboundedSender<AuxiliaryRequest>,
}

impl BridgeAgent {
    pub(in crate::web) fn new(requests: mpsc::UnboundedSender<AuxiliaryRequest>) -> Self {
        Self { requests }
    }
}

impl ReviewAgent for BridgeAgent {
    fn run(&self, prompt: AgentPrompt) -> BoxFuture<'static, Result<String, AuxiliaryError>> {
        let requests = self.requests.clone();
        Box::pin(async move {
            let stopped = || AuxiliaryError::Failed("the agent worker stopped".to_owned());
            let (completion, result) = oneshot::channel();
            requests
                .send(AuxiliaryRequest {
                    session: prompt.session,
                    prompt: prompt.prompt,
                    shutdown: prompt.shutdown,
                    completion,
                })
                .map_err(|_| stopped())?;
            result.await.map_err(|_| stopped())?
        })
    }
}
