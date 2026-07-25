//! Per-agent actor that exclusively owns a child runtime and its active turn.

use super::{capacity::TurnCapacity, model::AgentId, runtime::Registry};
use nanocodex::{Nanocodex, NanocodexError, TurnControl};
use std::sync::Weak;
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinError, JoinHandle},
};

const COMMAND_CAPACITY: usize = 8;

#[derive(Clone)]
pub(super) struct HarnessHandle {
    commands: mpsc::Sender<HarnessCommand>,
}

enum HarnessCommand {
    Start {
        prompt: String,
        capacity: TurnCapacity,
        response: oneshot::Sender<std::io::Result<()>>,
    },
    Steer {
        message: String,
        response: oneshot::Sender<std::io::Result<()>>,
    },
    Interrupt {
        response: oneshot::Sender<std::io::Result<()>>,
    },
    Close {
        response: oneshot::Sender<std::io::Result<()>>,
    },
}

struct Harness {
    root_session_id: String,
    id: AgentId,
    agent: Option<Nanocodex>,
    active: Option<ActiveTurn>,
    commands: mpsc::Receiver<HarnessCommand>,
    registry: Weak<Registry>,
}

struct ActiveTurn {
    control: TurnControl,
    result: JoinHandle<nanocodex::Result<nanocodex::TurnResult>>,
    _capacity: TurnCapacity,
}

enum HarnessEvent {
    Command(Option<HarnessCommand>),
    TurnFinished(Result<nanocodex::Result<nanocodex::TurnResult>, JoinError>),
}

impl HarnessHandle {
    pub(super) async fn start(
        &self,
        prompt: String,
        capacity: TurnCapacity,
    ) -> std::io::Result<()> {
        self.request(|response| HarnessCommand::Start {
            prompt,
            capacity,
            response,
        })
        .await
    }

    pub(super) async fn steer(&self, message: String) -> std::io::Result<()> {
        self.request(|response| HarnessCommand::Steer { message, response })
            .await
    }

    pub(super) async fn interrupt(&self) -> std::io::Result<()> {
        self.request(|response| HarnessCommand::Interrupt { response })
            .await
    }

    pub(super) async fn close(&self) -> std::io::Result<()> {
        self.request(|response| HarnessCommand::Close { response })
            .await
    }

    async fn request(
        &self,
        command: impl FnOnce(oneshot::Sender<std::io::Result<()>>) -> HarnessCommand,
    ) -> std::io::Result<()> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(command(response))
            .await
            .map_err(|_| std::io::Error::other("subagent harness is closed"))?;
        result
            .await
            .map_err(|_| std::io::Error::other("subagent harness stopped before responding"))?
    }
}

pub(super) fn spawn(
    root_session_id: String,
    id: AgentId,
    agent: Nanocodex,
    registry: Weak<Registry>,
) -> (HarnessHandle, JoinHandle<()>) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let handle = HarnessHandle { commands };
    let task = tokio::spawn(
        Harness {
            root_session_id,
            id,
            agent: Some(agent),
            active: None,
            commands: receiver,
            registry,
        }
        .run(),
    );
    (handle, task)
}

impl Harness {
    async fn run(mut self) {
        loop {
            match self.next_event().await {
                HarnessEvent::Command(Some(command)) => {
                    if self.handle(command).await {
                        return;
                    }
                }
                HarnessEvent::Command(None) => {
                    drop(self.stop_active().await);
                    return;
                }
                HarnessEvent::TurnFinished(result) => self.turn_finished(result).await,
            }
        }
    }

    async fn next_event(&mut self) -> HarnessEvent {
        let Some(active) = self.active.as_mut() else {
            return HarnessEvent::Command(self.commands.recv().await);
        };
        tokio::select! {
            biased;
            command = self.commands.recv() => HarnessEvent::Command(command),
            result = &mut active.result => HarnessEvent::TurnFinished(result),
        }
    }

    async fn handle(&mut self, command: HarnessCommand) -> bool {
        match command {
            HarnessCommand::Start {
                prompt,
                capacity,
                response,
            } => {
                let _ = response.send(self.start_turn(prompt, capacity).await);
                false
            }
            HarnessCommand::Steer { message, response } => {
                let _ = response.send(self.steer(message).await);
                false
            }
            HarnessCommand::Interrupt { response } => {
                let _ = response.send(self.stop_active().await);
                false
            }
            HarnessCommand::Close { response } => {
                let result = self.close().await;
                let _ = response.send(result);
                true
            }
        }
    }

    async fn start_turn(&mut self, prompt: String, capacity: TurnCapacity) -> std::io::Result<()> {
        if self.active.is_some() {
            return Err(std::io::Error::other(format!(
                "agent {} is not idle",
                self.id
            )));
        }
        let agent = self
            .agent
            .as_ref()
            .ok_or_else(|| std::io::Error::other(format!("agent {} is closed", self.id)))?;
        let turn = agent.prompt(prompt).await.map_err(|error| {
            std::io::Error::other(format!("could not start agent {}: {error}", self.id))
        })?;
        let control = turn.control();
        let result = tokio::spawn(async move { turn.result().await });
        self.active = Some(ActiveTurn {
            control,
            result,
            _capacity: capacity,
        });
        if let Some(registry) = self.registry.upgrade() {
            registry
                .harness_turn_started(&self.root_session_id, self.id)
                .await;
        }
        Ok(())
    }

    async fn steer(&self, message: String) -> std::io::Result<()> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| std::io::Error::other(format!("agent {} is not running", self.id)))?;
        active.control.steer(message).await.map_err(|error| {
            std::io::Error::other(format!("could not steer agent {}: {error}", self.id))
        })
    }

    async fn stop_active(&mut self) -> std::io::Result<()> {
        let Some(mut active) = self.active.take() else {
            return Ok(());
        };
        let cancellation = active.control.cancel().await;
        let result = (&mut active.result).await;
        self.publish_turn_result(result).await;
        match cancellation {
            Ok(()) | Err(NanocodexError::TurnNotCancellable) => Ok(()),
            Err(error) => Err(std::io::Error::other(format!(
                "could not stop agent {}: {error}",
                self.id
            ))),
        }
    }

    async fn turn_finished(
        &mut self,
        result: Result<nanocodex::Result<nanocodex::TurnResult>, JoinError>,
    ) {
        self.active = None;
        self.publish_turn_result(result).await;
    }

    async fn publish_turn_result(
        &self,
        result: Result<nanocodex::Result<nanocodex::TurnResult>, JoinError>,
    ) {
        let result = result.unwrap_or_else(|error| {
            Err(NanocodexError::InvalidRequest(format!(
                "subagent turn task failed: {error}"
            )))
        });
        if let Some(registry) = self.registry.upgrade() {
            registry
                .harness_turn_finished(&self.root_session_id, self.id, result)
                .await;
        }
    }

    async fn close(&mut self) -> std::io::Result<()> {
        let stop_result = self.stop_active().await;
        self.agent = None;
        if let Some(registry) = self.registry.upgrade() {
            registry
                .harness_closed(&self.root_session_id, self.id)
                .await;
        }
        stop_result
    }
}
