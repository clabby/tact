//! Fake model transport for tests that need real child harnesses.

use futures_util::future::Either;
use nanocodex::{
    Nanocodex, OpenAi, Thinking,
    oai::{
        ResponseError,
        tower::{
            ResponsesAttempt, ResponsesAttemptKind, ResponsesOutput, ResponsesServiceResponse,
        },
    },
};
use serde_json::json;
use std::{
    future::{Pending, Ready, pending, ready},
    result::Result as StdResult,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{Notify, mpsc};
use tower::Service;

/// A model request that never completes, optionally recording each turn's latest user prompt.
#[derive(Clone)]
pub(crate) struct PendingService {
    pub(crate) called: Arc<Notify>,
    pub(crate) prompts: Option<mpsc::UnboundedSender<(nanocodex::Model, Thinking, String)>>,
}

impl Service<ResponsesAttempt> for PendingService {
    type Response = ResponsesServiceResponse;
    type Error = ResponseError;
    type Future = Either<
        Ready<StdResult<Self::Response, Self::Error>>,
        Pending<StdResult<Self::Response, Self::Error>>,
    >;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
        if self.prompts.is_some() && matches!(request.kind(), ResponsesAttemptKind::Warmup) {
            return Either::Left(ready(Ok(ResponsesServiceResponse::new(
                ResponsesOutput::Warmup(serde_json::from_value(json!({ "id": "warmup" })).unwrap()),
            ))));
        }
        if let Some(prompts) = &self.prompts
            && let Some(prompt) = request
                .input_items()
                .map(|item| serde_json::to_value(item).unwrap())
                .filter(|item| item["role"] == "user")
                .last()
        {
            prompts
                .send((request.model(), request.thinking(), prompt.to_string()))
                .unwrap();
        }
        self.called.notify_one();
        Either::Right(pending())
    }
}

pub(crate) fn pending_agent(called: Arc<Notify>) -> (Nanocodex, nanocodex::AgentEvents) {
    let openai = OpenAi::builder("test-key")
        .service(move || PendingService {
            called: Arc::clone(&called),
            prompts: None,
        })
        .build()
        .unwrap();
    Nanocodex::builder(openai).build().unwrap()
}

/// Builds a session whose turns never finish and whose latest user prompt is sent to `prompts`.
pub(crate) fn recording_agent(
    prompts: mpsc::UnboundedSender<(nanocodex::Model, Thinking, String)>,
) -> (Nanocodex, nanocodex::AgentEvents) {
    let openai = OpenAi::builder("test-key")
        .service(move || PendingService {
            called: Arc::new(Notify::new()),
            prompts: Some(prompts.clone()),
        })
        .build()
        .unwrap();
    Nanocodex::builder(openai).build().unwrap()
}
