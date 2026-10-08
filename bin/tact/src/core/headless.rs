//! Headless execution: one prompt, one turn, JSONL events on an output stream.
//!
//! A headless run owns its agent from construction to shutdown. Every exit path, including
//! cancellation before the turn starts and a rejected prompt, shuts the agent down, drains its
//! events, and settles the subagent update stream before any error is reported. Cancellation
//! is cooperative: the turn and all subagents are asked to stop, and a turn that ends as
//! cancelled after a requested cancellation counts as a successful shutdown.

use super::ConfiguredAgent;
#[cfg(feature = "harbor-evals")]
use super::orchestration::OrchestrationRecorder;
use crate::app::{config::Config, error::Result, hook};
use nanocodex::{HarnessModel as Model, NanocodexError, TurnControl};
#[cfg(feature = "harbor-evals")]
use serde::Serialize;
#[cfg(feature = "harbor-evals")]
use std::path::PathBuf;
use std::{io, io::Write, mem};
use tact_subagents::ScopedAgentUpdate;
use tokio::sync::mpsc;
#[cfg(not(feature = "harbor-evals"))]
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How a headless run ended, as reported to the orchestration log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    feature = "harbor-evals",
    derive(Serialize),
    serde(rename_all = "snake_case")
)]
pub(super) enum RunOutcome {
    Completed,
    Cancelled,
    Failed,
}

enum Cancellation {
    NotRequested,
    Requested,
    Failed(NanocodexError),
}

impl Cancellation {
    async fn request(control: &TurnControl) -> Self {
        match control.cancel().await {
            Ok(()) => Self::Requested,
            Err(NanocodexError::TurnNotCancellable) => Self::NotRequested,
            Err(error) => Self::Failed(error),
        }
    }
}

/// Consumes subagent updates for the duration of a run so the unbounded channel cannot grow.
/// With the `harbor-evals` feature the updates also feed the orchestration log.
struct SubagentUpdateSink {
    #[cfg(feature = "harbor-evals")]
    recorder: OrchestrationRecorder,
    #[cfg(not(feature = "harbor-evals"))]
    drain: JoinHandle<()>,
}

impl SubagentUpdateSink {
    fn start(
        updates: mpsc::UnboundedReceiver<ScopedAgentUpdate>,
        #[cfg(feature = "harbor-evals")] orchestration_log: Option<PathBuf>,
    ) -> Result<Self> {
        #[cfg(feature = "harbor-evals")]
        return Ok(Self {
            recorder: OrchestrationRecorder::start(updates, orchestration_log)?,
        });
        #[cfg(not(feature = "harbor-evals"))]
        {
            let mut updates = updates;
            Ok(Self {
                drain: tokio::spawn(async move { while updates.recv().await.is_some() {} }),
            })
        }
    }

    #[cfg_attr(not(feature = "harbor-evals"), allow(clippy::unused_async))]
    async fn finish(self, root_session_id: &str, outcome: RunOutcome) -> Result<()> {
        #[cfg(feature = "harbor-evals")]
        self.recorder.finish(root_session_id, outcome).await?;
        #[cfg(not(feature = "harbor-evals"))]
        {
            let _ = (root_session_id, outcome);
            self.drain.abort();
        }
        Ok(())
    }
}

/// The results a turn produced, collected after cleanup and settled into one outcome.
struct SettledTurn {
    events: Result<()>,
    cancellation: Cancellation,
    turn: nanocodex::agent::Result<()>,
    shutdown: nanocodex::agent::Result<()>,
}

impl SettledTurn {
    fn outcome(&self) -> RunOutcome {
        if matches!(self.cancellation, Cancellation::Requested) {
            RunOutcome::Cancelled
        } else if self.events.is_err() || self.turn.is_err() {
            RunOutcome::Failed
        } else {
            RunOutcome::Completed
        }
    }

    /// Reports the first failure in causal order: event output, cancellation, the turn itself,
    /// and finally shutdown.
    fn into_result(self) -> Result<()> {
        self.events?;
        let cancelled = match self.cancellation {
            Cancellation::Failed(error) => return Err(error.into()),
            Cancellation::Requested => true,
            Cancellation::NotRequested => false,
        };
        match self.turn {
            Err(NanocodexError::TurnCancelled) if cancelled => {}
            Err(error) => return Err(error.into()),
            Ok(()) => {}
        }
        self.shutdown?;
        Ok(())
    }
}

impl ConfiguredAgent {
    pub(crate) async fn run_from_config(
        config: &Config,
        model: Model,
        prompt: String,
        shutdown: CancellationToken,
        #[cfg(feature = "harbor-evals")] orchestration_log: Option<PathBuf>,
    ) -> Result<()> {
        let reasoning_mode =
            super::supported_reasoning_mode(model, config.agent().reasoning_mode());
        let result =
            Self::from_config_with_model(config, config.agent().thinking(), reasoning_mode, model)?
                .run(
                    prompt,
                    shutdown,
                    io::stdout(),
                    #[cfg(feature = "harbor-evals")]
                    orchestration_log,
                )
                .await;
        if let Some(command) = config.agent().completion_hook() {
            drop(hook::execute(command, config.agent().workspace()).await);
        }
        result
    }

    pub(super) async fn run(
        mut self,
        prompt: String,
        shutdown: CancellationToken,
        output: impl Write,
        #[cfg(feature = "harbor-evals")] orchestration_log: Option<PathBuf>,
    ) -> Result<()> {
        let updates = mem::replace(&mut self.subagent_updates, mpsc::unbounded_channel().1);
        let sink = SubagentUpdateSink::start(
            updates,
            #[cfg(feature = "harbor-evals")]
            orchestration_log,
        )?;
        let root_session_id = self.agent.session_id().to_string();
        let (outcome, result) = self
            .drive(prompt, &shutdown, output, &root_session_id)
            .await;
        sink.finish(&root_session_id, outcome).await?;
        result
    }

    /// Runs the turn and always shuts the agent down before returning.
    async fn drive(
        mut self,
        prompt: String,
        shutdown: &CancellationToken,
        mut output: impl Write,
        root_session_id: &str,
    ) -> (RunOutcome, Result<()>) {
        if shutdown.is_cancelled() {
            return (
                RunOutcome::Cancelled,
                self.shutdown().await.map_err(Into::into),
            );
        }
        let turn = match self.agent.prompt(self.context.prompt(prompt)).await {
            Ok(turn) => turn,
            Err(error) => {
                let shutdown = self.shutdown().await.map_err(Into::into);
                return (RunOutcome::Failed, shutdown.and(Err(error.into())));
            }
        };
        let control = turn.control();
        let mut cancellation = Cancellation::NotRequested;
        let events = tokio::select! {
            biased;
            result = self.events.write_turn_jsonl(&mut output) => result,
            () = shutdown.cancelled() => {
                cancellation = Cancellation::request(&control).await;
                self.subagent_control.cancel_all(root_session_id).await;
                self.events.write_turn_jsonl(&mut output).await
            }
        };
        if events.is_err() && matches!(cancellation, Cancellation::NotRequested) {
            cancellation = Cancellation::request(&control).await;
            self.subagent_control.cancel_all(root_session_id).await;
        }
        let turn = turn.await.map(drop);
        drop(control);
        self.subagent_control.close_all(root_session_id).await;
        let settled = SettledTurn {
            events: events.map_err(Into::into),
            cancellation,
            turn,
            shutdown: self.shutdown().await,
        };
        (settled.outcome(), settled.into_result())
    }

    /// Stops the agent and drains its remaining events.
    pub(super) async fn shutdown(mut self) -> nanocodex::agent::Result<()> {
        let result = self.agent.shutdown().await;
        drop(self.agent);
        while self.events.recv().await.is_some() {}
        result
    }
}
