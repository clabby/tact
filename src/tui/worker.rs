//! Independently scheduled Nanocodex turn worker.

use crate::{
    config::ReasoningEffort,
    tui::{components::QueueId, pane::PaneId, prompt::Submission, transcript::TurnId},
};
use nanocodex::{AgentEvents, Nanocodex, NanocodexError, SessionSnapshot, TurnControl};
use std::collections::{HashMap, HashSet};
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;

pub(crate) enum WorkerCommand {
    Submit {
        pane: PaneId,
        id: TurnId,
        prompt: Submission,
    },
    Steer {
        pane: PaneId,
        queue_id: QueueId,
        fallback_id: TurnId,
        prompt: Submission,
    },
    ReplaceAgent {
        pane: PaneId,
        agent: Nanocodex,
    },
    SetThinking {
        pane: PaneId,
        effort: ReasoningEffort,
    },
    SetFastMode {
        pane: PaneId,
        enabled: bool,
    },
    CancelAll(PaneId),
    OpenFork(PaneId),
    ClosePane(PaneId),
}

pub(crate) enum WorkerEvent {
    TurnAccepted {
        pane: PaneId,
        id: TurnId,
    },
    TurnFinished {
        pane: PaneId,
        id: TurnId,
        error: Option<String>,
        snapshot: Option<Box<SessionSnapshot>>,
    },
    SteerAdmitted {
        pane: PaneId,
        queue_id: QueueId,
    },
    SteerPromoted {
        pane: PaneId,
        queue_id: QueueId,
        id: TurnId,
        prompt: Submission,
    },
    SteerFailed {
        pane: PaneId,
        queue_id: QueueId,
        error: String,
    },
    TurnsCancelled {
        pane: PaneId,
        count: usize,
        error: Option<String>,
    },
    ForkOpened {
        pane: PaneId,
        events: AgentEvents,
    },
    ForkFailed {
        pane: PaneId,
        error: String,
    },
    ThinkingUpdated {
        pane: PaneId,
        effort: ReasoningEffort,
        result: Result<(), NanocodexError>,
    },
    FastModeUpdated {
        pane: PaneId,
        enabled: bool,
        result: Result<(), NanocodexError>,
    },
    Stopped {
        error: Option<NanocodexError>,
    },
}

type TurnResult = Result<Box<SessionSnapshot>, NanocodexError>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TurnKey {
    pane: PaneId,
    id: TurnId,
}

struct SteerRequest {
    pane: PaneId,
    queue_id: QueueId,
    fallback_id: TurnId,
    prompt: Submission,
}

pub(crate) fn spawn(
    agent: Nanocodex,
    shutdown: CancellationToken,
) -> (
    mpsc::UnboundedSender<WorkerCommand>,
    mpsc::UnboundedReceiver<WorkerEvent>,
) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (updates, update_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(agent, command_rx, updates, shutdown));
    (commands, update_rx)
}

async fn run(
    agent: Nanocodex,
    mut commands: mpsc::UnboundedReceiver<WorkerCommand>,
    updates: mpsc::UnboundedSender<WorkerEvent>,
    shutdown: CancellationToken,
) {
    let mut main = Some(agent);
    let mut fork = None::<(PaneId, Nanocodex)>;
    let mut controls = HashMap::<TurnKey, TurnControl>::new();
    let mut cancelled = HashSet::<TurnKey>::new();
    let mut turns = JoinSet::<(TurnKey, TurnResult)>::new();

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            result = turns.join_next(), if !turns.is_empty() => {
                finish_turn(result, false, &mut controls, &mut cancelled, &updates);
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                let (pane, id, prompt) = match command {
                    WorkerCommand::Submit { pane, id, prompt } => (pane, id, prompt),
                    WorkerCommand::Steer {
                        pane,
                        queue_id,
                        fallback_id,
                        prompt,
                    } => {
                        let Some(agent) = agent_for(pane, main.as_ref(), fork.as_ref()) else {
                            drop(updates.send(WorkerEvent::SteerFailed {
                                pane,
                                queue_id,
                                error: "session pane is no longer available".to_owned(),
                            }));
                            continue;
                        };
                        let request = SteerRequest {
                            pane,
                            queue_id,
                            fallback_id,
                            prompt,
                        };
                        steer_turn(agent, &mut controls, &mut turns, &updates, request).await;
                        continue;
                    }
                    WorkerCommand::ReplaceAgent { pane, agent } => {
                        debug_assert!(!controls.keys().any(|key| key.pane == pane));
                        match pane {
                            PaneId::Main => main = Some(agent),
                            PaneId::Fork(_) if fork.as_ref().is_some_and(|(id, _)| *id == pane) => {
                                fork = Some((pane, agent));
                            }
                            PaneId::Fork(_) => {
                                drop(updates.send(WorkerEvent::ForkFailed {
                                    pane,
                                    error: "session pane is no longer available".to_owned(),
                                }));
                            }
                        }
                        continue;
                    }
                    WorkerCommand::SetThinking { pane, effort } => {
                        let result = match agent_for(pane, main.as_ref(), fork.as_ref()) {
                            Some(agent) => agent.set_thinking(effort.into()).await,
                            None => Err(NanocodexError::AgentStopped),
                        };
                        drop(updates.send(WorkerEvent::ThinkingUpdated {
                            pane,
                            effort,
                            result,
                        }));
                        continue;
                    }
                    WorkerCommand::SetFastMode { pane, enabled } => {
                        let result = match agent_for(pane, main.as_ref(), fork.as_ref()) {
                            Some(agent) => agent.set_fast_mode(enabled).await,
                            None => Err(NanocodexError::AgentStopped),
                        };
                        drop(updates.send(WorkerEvent::FastModeUpdated {
                            pane,
                            enabled,
                            result,
                        }));
                        continue;
                    }
                    WorkerCommand::CancelAll(pane) => {
                        cancel_pane(pane, &controls, &mut cancelled, &updates).await;
                        continue;
                    }
                    WorkerCommand::OpenFork(pane) => {
                        if fork.is_some() {
                            drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: "a forked session is already open".to_owned(),
                            }));
                            continue;
                        }
                        let Some(agent) = main.as_ref() else {
                            drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: "the primary session is no longer available".to_owned(),
                            }));
                            continue;
                        };
                        match agent.fork().await {
                            Ok((agent, events)) => {
                                fork = Some((pane, agent));
                                drop(updates.send(WorkerEvent::ForkOpened { pane, events }));
                            }
                            Err(error) => drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: error.to_string(),
                            })),
                        }
                        continue;
                    }
                    WorkerCommand::ClosePane(pane) => {
                        cancel_pane(pane, &controls, &mut cancelled, &updates).await;
                        match pane {
                            PaneId::Main => main = None,
                            PaneId::Fork(_) if fork.as_ref().is_some_and(|(id, _)| *id == pane) => {
                                fork = None;
                            }
                            PaneId::Fork(_) => {}
                        }
                        continue;
                    }
                };
                let Some(agent) = agent_for(pane, main.as_ref(), fork.as_ref()) else {
                    drop(updates.send(WorkerEvent::TurnFinished {
                        pane,
                        id,
                        error: Some("session pane is no longer available".to_owned()),
                        snapshot: None,
                    }));
                    continue;
                };
                match agent.prompt(prompt.agent_prompt()).await {
                    Ok(turn) => {
                        let key = TurnKey { pane, id };
                        controls.insert(key, turn.control());
                        turns.spawn(async move {
                            (
                                key,
                                turn.result().await.map(|result| Box::new(result.snapshot())),
                            )
                        });
                        drop(updates.send(WorkerEvent::TurnAccepted { pane, id }));
                    }
                    Err(error) => {
                        drop(updates.send(WorkerEvent::TurnFinished {
                            pane,
                            id,
                            error: Some(error.to_string()),
                            snapshot: None,
                        }));
                    }
                }
            }
        }
    }

    commands.close();
    while commands.try_recv().is_ok() {}

    let pending_controls = controls.values().cloned().collect::<Vec<_>>();
    let mut shutdown_error = None;
    for control in pending_controls {
        match control.cancel().await {
            Ok(()) | Err(NanocodexError::TurnNotCancellable) => {}
            Err(error) if shutdown_error.is_none() => shutdown_error = Some(error),
            Err(_) => {}
        }
    }

    while let Some(result) = turns.join_next().await {
        finish_turn(Some(result), true, &mut controls, &mut cancelled, &updates);
    }

    drop(main);
    drop(fork);
    drop(updates.send(WorkerEvent::Stopped {
        error: shutdown_error,
    }));
}

async fn steer_turn(
    agent: &Nanocodex,
    controls: &mut HashMap<TurnKey, TurnControl>,
    turns: &mut JoinSet<(TurnKey, TurnResult)>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    request: SteerRequest,
) {
    let SteerRequest {
        pane,
        queue_id,
        fallback_id,
        prompt,
    } = request;
    let mut active = controls
        .iter()
        .filter(|(key, _)| key.pane == pane)
        .collect::<Vec<_>>();
    active.sort_unstable_by_key(|(key, _)| key.id);
    for (_, control) in active {
        match control.steer(prompt.agent_prompt()).await {
            Ok(()) => {
                drop(updates.send(WorkerEvent::SteerAdmitted { pane, queue_id }));
                return;
            }
            Err(NanocodexError::TurnNotSteerable) => {}
            Err(error) => {
                drop(updates.send(WorkerEvent::SteerFailed {
                    pane,
                    queue_id,
                    error: error.to_string(),
                }));
                return;
            }
        }
    }

    match agent.prompt(prompt.agent_prompt()).await {
        Ok(turn) => {
            let control = turn.control();
            let key = TurnKey {
                pane,
                id: fallback_id,
            };
            turns.spawn(async move {
                (
                    key,
                    turn.result()
                        .await
                        .map(|result| Box::new(result.snapshot())),
                )
            });
            controls.insert(key, control);
            drop(updates.send(WorkerEvent::TurnAccepted {
                pane,
                id: fallback_id,
            }));
            drop(updates.send(WorkerEvent::SteerPromoted {
                pane,
                queue_id,
                id: fallback_id,
                prompt,
            }));
        }
        Err(error) => {
            drop(updates.send(WorkerEvent::SteerFailed {
                pane,
                queue_id,
                error: error.to_string(),
            }));
        }
    }
}

async fn cancel_turns(
    controls: &HashMap<TurnKey, TurnControl>,
    pane: Option<PaneId>,
) -> (Vec<TurnKey>, Option<String>) {
    let pending = controls
        .iter()
        .filter(|(key, _)| pane.is_none_or(|pane| key.pane == pane))
        .map(|(&key, control)| (key, control.clone()))
        .collect::<Vec<_>>();
    let mut cancelled = Vec::with_capacity(pending.len());
    let mut first_error = None;
    for (key, control) in pending {
        match control.cancel().await {
            Ok(()) => cancelled.push(key),
            Err(NanocodexError::TurnNotCancellable) => {}
            Err(error) if first_error.is_none() => first_error = Some(error.to_string()),
            Err(_) => {}
        }
    }
    (cancelled, first_error)
}

fn finish_turn(
    result: Option<Result<(TurnKey, TurnResult), JoinError>>,
    shutting_down: bool,
    controls: &mut HashMap<TurnKey, TurnControl>,
    cancelled: &mut HashSet<TurnKey>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let Some(result) = result else {
        return;
    };
    let (key, result) = match result {
        Ok(result) => result,
        Err(error) => {
            drop(updates.send(WorkerEvent::TurnFinished {
                pane: PaneId::Main,
                id: TurnId::new(0),
                error: Some(format!("turn task stopped unexpectedly: {error}")),
                snapshot: None,
            }));
            return;
        }
    };
    controls.remove(&key);
    let was_cancelled = cancelled.remove(&key);
    let (error, snapshot) = match result {
        Ok(snapshot) => (None, Some(snapshot)),
        Err(NanocodexError::TurnCancelled) if shutting_down || was_cancelled => (None, None),
        Err(error) => (Some(error.to_string()), None),
    };
    drop(updates.send(WorkerEvent::TurnFinished {
        pane: key.pane,
        id: key.id,
        error,
        snapshot,
    }));
}

fn agent_for<'a>(
    pane: PaneId,
    main: Option<&'a Nanocodex>,
    fork: Option<&'a (PaneId, Nanocodex)>,
) -> Option<&'a Nanocodex> {
    match pane {
        PaneId::Main => main,
        PaneId::Fork(_) => fork
            .filter(|(fork_pane, _)| *fork_pane == pane)
            .map(|(_, agent)| agent),
    }
}

async fn cancel_pane(
    pane: PaneId,
    controls: &HashMap<TurnKey, TurnControl>,
    cancelled: &mut HashSet<TurnKey>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let (keys, error) = cancel_turns(controls, Some(pane)).await;
    let count = keys.len();
    cancelled.extend(keys);
    drop(updates.send(WorkerEvent::TurnsCancelled { pane, count, error }));
}

#[cfg(test)]
mod tests {
    use super::{WorkerCommand, WorkerEvent, spawn};
    use crate::{
        config::ReasoningEffort,
        tui::{components::QueueId, pane::PaneId, transcript::TurnId},
    };
    use nanocodex::{
        AgentEvents, Nanocodex, NanocodexError, Responses, ResponsesAttempt,
        ResponsesServiceResponse,
    };
    use std::{
        future::{Pending, pending},
        result::Result as StdResult,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::{sync::Notify, time::timeout};
    use tokio_util::sync::CancellationToken;
    use tower::Service;

    #[derive(Clone)]
    struct PendingService {
        called: Arc<Notify>,
        calls: Arc<AtomicUsize>,
    }

    impl Service<ResponsesAttempt> for PendingService {
        type Response = ResponsesServiceResponse;
        type Error = NanocodexError;
        type Future = Pending<StdResult<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.called.notify_one();
            pending()
        }
    }

    fn pending_agent(called: Arc<Notify>, calls: Arc<AtomicUsize>) -> (Nanocodex, AgentEvents) {
        let responses = Responses::builder()
            .service(move || PendingService {
                called: Arc::clone(&called),
                calls: Arc::clone(&calls),
            })
            .build();
        Nanocodex::builder("test-key")
            .responses(responses)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn thinking_can_change_while_a_turn_is_active() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(agent, shutdown.clone());
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });

        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "keep running".to_owned().into(),
            })
            .unwrap();
        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the model request should start");
        assert!(matches!(
            updates.recv().await,
            Some(WorkerEvent::TurnAccepted { id, .. }) if id == TurnId::new(1)
        ));

        commands
            .send(WorkerCommand::SetThinking {
                pane: PaneId::Main,
                effort: ReasoningEffort::High,
            })
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(5), updates.recv()).await,
            Ok(Some(WorkerEvent::ThinkingUpdated {
                pane: PaneId::Main,
                effort: ReasoningEffort::High,
                result: Ok(()),
            }))
        ));

        shutdown.cancel();
        timeout(Duration::from_secs(5), async {
            while !matches!(updates.recv().await, Some(WorkerEvent::Stopped { .. })) {}
        })
        .await
        .expect("the worker should stop");
        timeout(Duration::from_secs(5), drain)
            .await
            .expect("the event stream should drain")
            .expect("the drain task should not panic");
    }

    #[tokio::test]
    async fn fast_mode_can_change_while_a_turn_is_active() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(agent, shutdown.clone());
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });

        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "keep running".to_owned().into(),
            })
            .unwrap();
        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the model request should start");
        assert!(matches!(
            updates.recv().await,
            Some(WorkerEvent::TurnAccepted { id, .. }) if id == TurnId::new(1)
        ));

        commands
            .send(WorkerCommand::SetFastMode {
                pane: PaneId::Main,
                enabled: true,
            })
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(5), updates.recv()).await,
            Ok(Some(WorkerEvent::FastModeUpdated {
                pane: PaneId::Main,
                enabled: true,
                result: Ok(()),
            }))
        ));

        shutdown.cancel();
        timeout(Duration::from_secs(5), async {
            while !matches!(updates.recv().await, Some(WorkerEvent::Stopped { .. })) {}
        })
        .await
        .expect("the worker should stop");
        timeout(Duration::from_secs(5), drain)
            .await
            .expect("the event stream should drain")
            .expect("the drain task should not panic");
    }

    #[tokio::test]
    async fn steer_is_admitted_without_blocking_the_pending_turn() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(agent, shutdown.clone());
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });

        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "initial".to_owned().into(),
            })
            .unwrap();
        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the model request should start");
        assert!(matches!(
            updates.recv().await,
            Some(WorkerEvent::TurnAccepted { id, .. }) if id == TurnId::new(1)
        ));

        commands
            .send(WorkerCommand::Steer {
                pane: PaneId::Main,
                queue_id: QueueId::new(7),
                fallback_id: TurnId::new(2),
                prompt: "change direction".to_owned().into(),
            })
            .unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(5), updates.recv()).await,
            Ok(Some(WorkerEvent::SteerAdmitted { queue_id, .. }))
                if queue_id == QueueId::new(7)
        ));

        shutdown.cancel();
        timeout(Duration::from_secs(5), async {
            loop {
                match updates.recv().await {
                    Some(WorkerEvent::Stopped { .. }) => break,
                    Some(_) => {}
                    None => panic!("worker updates closed before shutdown completed"),
                }
            }
        })
        .await
        .expect("the worker should stop");
        timeout(Duration::from_secs(5), drain)
            .await
            .expect("the event stream should drain")
            .expect("the drain task should not panic");
    }

    #[tokio::test]
    async fn steer_without_an_active_turn_is_promoted_without_losing_the_message() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(agent, shutdown.clone());
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });

        commands
            .send(WorkerCommand::Steer {
                pane: PaneId::Main,
                queue_id: QueueId::new(9),
                fallback_id: TurnId::new(3),
                prompt: "race-safe prompt".to_owned().into(),
            })
            .unwrap();
        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the promoted model request should start");
        assert!(matches!(
            updates.recv().await,
            Some(WorkerEvent::TurnAccepted { id, .. }) if id == TurnId::new(3)
        ));
        assert!(matches!(
            updates.recv().await,
            Some(WorkerEvent::SteerPromoted { queue_id, id, prompt, .. })
                if queue_id == QueueId::new(9)
                    && id == TurnId::new(3)
                    && prompt.display_text() == "race-safe prompt"
        ));

        shutdown.cancel();
        timeout(Duration::from_secs(5), async {
            loop {
                match updates.recv().await {
                    Some(WorkerEvent::Stopped { .. }) => break,
                    Some(_) => {}
                    None => panic!("worker updates closed before shutdown completed"),
                }
            }
        })
        .await
        .expect("the worker should stop");
        timeout(Duration::from_secs(5), drain)
            .await
            .expect("the event stream should drain")
            .expect("the drain task should not panic");
    }

    #[tokio::test]
    async fn pending_prompt_is_accepted_and_cancelled_during_shutdown() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(agent, shutdown.clone());
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });

        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "keep running".to_owned().into(),
            })
            .unwrap();
        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the model request should start");
        assert!(matches!(
            updates.recv().await,
            Some(WorkerEvent::TurnAccepted { id, .. }) if id == TurnId::new(1)
        ));

        shutdown.cancel();
        timeout(Duration::from_secs(5), async {
            let mut cancelled = false;
            loop {
                match updates.recv().await {
                    Some(WorkerEvent::TurnFinished {
                        id, error: None, ..
                    }) if id == TurnId::new(1) => {
                        cancelled = true;
                    }
                    Some(WorkerEvent::Stopped { error: None }) => break,
                    Some(_) => {}
                    None => panic!("worker updates closed before shutdown completed"),
                }
            }
            assert!(cancelled);
        })
        .await
        .expect("the worker should stop");
        timeout(Duration::from_secs(5), drain)
            .await
            .expect("the event stream should drain")
            .expect("the drain task should not panic");
    }

    #[tokio::test]
    async fn explicit_cancellation_interrupts_the_turn_and_keeps_worker_alive() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(agent, shutdown.clone());
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });

        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "interrupt me".to_owned().into(),
            })
            .unwrap();
        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the model request should start");
        assert!(matches!(
            updates.recv().await,
            Some(WorkerEvent::TurnAccepted { id, .. }) if id == TurnId::new(1)
        ));

        commands
            .send(WorkerCommand::CancelAll(PaneId::Main))
            .unwrap();
        timeout(Duration::from_secs(5), async {
            let mut acknowledged = false;
            let mut finished = false;
            while !acknowledged || !finished {
                match updates.recv().await {
                    Some(WorkerEvent::TurnsCancelled {
                        count: 1,
                        error: None,
                        ..
                    }) => acknowledged = true,
                    Some(WorkerEvent::TurnFinished {
                        id, error: None, ..
                    }) if id == TurnId::new(1) => finished = true,
                    Some(_) => panic!("unexpected worker event"),
                    None => panic!("worker stopped during explicit cancellation"),
                }
            }
        })
        .await
        .expect("the active turn should be cancelled");

        assert!(!shutdown.is_cancelled());
        commands
            .send(WorkerCommand::CancelAll(PaneId::Main))
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(5), updates.recv()).await,
            Ok(Some(WorkerEvent::TurnsCancelled {
                count: 0,
                error: None,
                ..
            }))
        ));

        shutdown.cancel();
        timeout(Duration::from_secs(5), async {
            while !matches!(updates.recv().await, Some(WorkerEvent::Stopped { .. })) {}
        })
        .await
        .expect("the worker should stop");
        timeout(Duration::from_secs(5), drain)
            .await
            .expect("the event stream should drain")
            .expect("the drain task should not panic");
    }

    #[tokio::test]
    async fn replacement_agent_receives_the_first_prompt() {
        let first_called = Arc::new(Notify::new());
        let first_calls = Arc::new(AtomicUsize::new(0));
        let (first_agent, mut first_events) =
            pending_agent(Arc::clone(&first_called), Arc::clone(&first_calls));
        let second_called = Arc::new(Notify::new());
        let second_calls = Arc::new(AtomicUsize::new(0));
        let (second_agent, mut second_events) =
            pending_agent(Arc::clone(&second_called), Arc::clone(&second_calls));
        let first_drain = tokio::spawn(async move { while first_events.recv().await.is_some() {} });
        let second_drain =
            tokio::spawn(async move { while second_events.recv().await.is_some() {} });
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(first_agent, shutdown.clone());

        commands
            .send(WorkerCommand::ReplaceAgent {
                pane: PaneId::Main,
                agent: second_agent,
            })
            .unwrap();
        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "use replacement".to_owned().into(),
            })
            .unwrap();
        timeout(Duration::from_secs(5), second_called.notified())
            .await
            .expect("the replacement agent should receive the prompt");

        assert_eq!(first_calls.load(Ordering::Relaxed), 0);
        assert_eq!(second_calls.load(Ordering::Relaxed), 1);
        assert!(matches!(
            updates.recv().await,
            Some(WorkerEvent::TurnAccepted { id, .. }) if id == TurnId::new(1)
        ));

        shutdown.cancel();
        timeout(Duration::from_secs(5), async {
            while !matches!(updates.recv().await, Some(WorkerEvent::Stopped { .. })) {}
        })
        .await
        .expect("the worker should stop");
        timeout(Duration::from_secs(5), first_drain)
            .await
            .expect("the original event stream should drain")
            .expect("the original drain task should not panic");
        timeout(Duration::from_secs(5), second_drain)
            .await
            .expect("the replacement event stream should drain")
            .expect("the replacement drain task should not panic");
    }
}
