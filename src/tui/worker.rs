//! Independently scheduled Nanocodex turn worker.

use crate::{
    app::config::ReasoningEffort,
    core::MEMORY_REVIEW_CHECKPOINT,
    tui::{components::QueueId, pane::PaneId, prompt::Submission, transcript::TurnId},
};
use nanocodex::{
    AgentEvents, Nanocodex, NanocodexError, TurnControl,
    agent::{
        input::{Prompt, PromptInput, UserInput},
        session::SessionSnapshot,
    },
};
use std::collections::{HashMap, HashSet};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;

pub(crate) enum WorkerCommand {
    Submit {
        pane: PaneId,
        id: TurnId,
        prompt: Submission,
    },
    Auxiliary {
        pane: PaneId,
        id: TurnId,
        prompt: Submission,
        shutdown: CancellationToken,
        completion: oneshot::Sender<Result<String, AuxiliaryError>>,
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
        memory_review: MemoryReviewState,
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

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AuxiliaryError {
    Cancelled,
    Failed(String),
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

type TurnResult = Result<CompletedTurn, NanocodexError>;

struct CompletedTurn {
    final_message: String,
    snapshot: Option<Box<SessionSnapshot>>,
}

enum TurnPurpose {
    Conversation,
    Auxiliary(oneshot::Sender<Result<String, AuxiliaryError>>),
}

struct TurnRequest {
    pane: PaneId,
    id: TurnId,
    prompt: Submission,
    purpose: TurnPurpose,
    shutdown: Option<CancellationToken>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryReviewState {
    Disabled,
    BeforeFirstTurn,
    FollowUp,
}

impl MemoryReviewState {
    pub(crate) const fn fresh(enabled: bool) -> Self {
        if enabled {
            Self::BeforeFirstTurn
        } else {
            Self::Disabled
        }
    }

    pub(crate) const fn restored(enabled: bool) -> Self {
        if enabled {
            Self::FollowUp
        } else {
            Self::Disabled
        }
    }

    const fn forked(self) -> Self {
        match self {
            Self::Disabled => Self::Disabled,
            Self::BeforeFirstTurn | Self::FollowUp => Self::FollowUp,
        }
    }

    fn submission_prompt(self, submission: &Submission) -> Prompt {
        match self {
            Self::Disabled | Self::BeforeFirstTurn => submission.agent_prompt(),
            Self::FollowUp => prompt_with_memory_review(submission),
        }
    }

    fn steer_prompt(self, submission: &Submission) -> Prompt {
        match self {
            Self::Disabled => submission.agent_prompt(),
            Self::BeforeFirstTurn | Self::FollowUp => prompt_with_memory_review(submission),
        }
    }

    fn turn_accepted(&mut self) {
        if *self == Self::BeforeFirstTurn {
            *self = Self::FollowUp;
        }
    }
}

fn prompt_with_memory_review(submission: &Submission) -> Prompt {
    let mut prompt = submission.agent_prompt();
    match &mut prompt.instruction {
        PromptInput::Text(text) => {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(MEMORY_REVIEW_CHECKPOINT);
        }
        PromptInput::Content(content) => content.push(UserInput::Text {
            text: format!("\n\n{MEMORY_REVIEW_CHECKPOINT}"),
        }),
    }
    prompt
}

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
    memory_review: MemoryReviewState,
    shutdown: CancellationToken,
) -> (
    mpsc::UnboundedSender<WorkerCommand>,
    mpsc::UnboundedReceiver<WorkerEvent>,
) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (updates, update_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(agent, memory_review, command_rx, updates, shutdown));
    (commands, update_rx)
}

async fn run(
    agent: Nanocodex,
    memory_review: MemoryReviewState,
    mut commands: mpsc::UnboundedReceiver<WorkerCommand>,
    updates: mpsc::UnboundedSender<WorkerEvent>,
    shutdown: CancellationToken,
) {
    let mut main = Some(agent);
    let mut fork = None::<(PaneId, Nanocodex)>;
    let mut controls = HashMap::<TurnKey, TurnControl>::new();
    let mut memory_reviews = HashMap::from([(PaneId::Main, memory_review)]);
    let mut cancelled = HashSet::<TurnKey>::new();
    let mut turns = JoinSet::<(TurnKey, TurnPurpose, bool, TurnResult)>::new();

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
                let request = match command {
                    WorkerCommand::Submit { pane, id, prompt } => TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose: TurnPurpose::Conversation,
                        shutdown: None,
                    },
                    WorkerCommand::Auxiliary {
                        pane,
                        id,
                        prompt,
                        shutdown,
                        completion,
                    } => TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose: TurnPurpose::Auxiliary(completion),
                        shutdown: Some(shutdown),
                    },
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
                        let memory_review = *memory_reviews
                            .get(&pane)
                            .expect("an available pane must have memory-review state");
                        let started_turn = steer_turn(
                            agent,
                            memory_review,
                            &mut controls,
                            &mut turns,
                            &updates,
                            request,
                        )
                        .await;
                        if started_turn {
                            memory_reviews
                                .get_mut(&pane)
                                .expect("an available pane must have memory-review state")
                                .turn_accepted();
                        }
                        continue;
                    }
                    WorkerCommand::ReplaceAgent {
                        pane,
                        agent,
                        memory_review,
                    } => {
                        debug_assert!(!controls.keys().any(|key| key.pane == pane));
                        let retired = match pane {
                            PaneId::Main => {
                                memory_reviews.insert(pane, memory_review);
                                main.replace(agent)
                            }
                            PaneId::Fork(_) if fork.as_ref().is_some_and(|(id, _)| *id == pane) => {
                                memory_reviews.insert(pane, memory_review);
                                fork.replace((pane, agent)).map(|(_, agent)| agent)
                            }
                            PaneId::Fork(_) => {
                                drop(updates.send(WorkerEvent::ForkFailed {
                                    pane,
                                    error: "session pane is no longer available".to_owned(),
                                }));
                                Some(agent)
                            }
                        };
                        if let Some(retired) = retired {
                            drop(retired.shutdown().await);
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
                                let memory_review = *memory_reviews
                                    .get(&PaneId::Main)
                                    .expect("the primary pane must have memory-review state");
                                let memory_review = memory_review.forked();
                                memory_reviews.insert(pane, memory_review);
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
                        let agent = match pane {
                            PaneId::Main => main.take(),
                            PaneId::Fork(_) if fork.as_ref().is_some_and(|(id, _)| *id == pane) => {
                                fork.take().map(|(_, agent)| agent)
                            }
                            PaneId::Fork(_) => None,
                        };
                        memory_reviews.remove(&pane);
                        close_pane(pane, agent, &controls, &mut cancelled, &updates).await;
                        continue;
                    }
                };
                let Some(agent) = agent_for(request.pane, main.as_ref(), fork.as_ref()) else {
                    reject_turn(request, "session pane is no longer available".to_owned(), &updates);
                    continue;
                };
                let pane = request.pane;
                let memory_review = *memory_reviews
                    .get(&pane)
                    .expect("an available pane must have memory-review state");
                let started_conversation = start_turn(
                    agent,
                    request,
                    memory_review,
                    &mut controls,
                    &mut turns,
                    &updates,
                )
                .await;
                if started_conversation {
                    memory_reviews
                        .get_mut(&pane)
                        .expect("an available pane must have memory-review state")
                        .turn_accepted();
                }
            }
        }
    }

    commands.close();
    while commands.try_recv().is_ok() {}

    drop(cancel_turns(&controls, None).await);
    let (main_shutdown, fork_shutdown) = tokio::join!(
        shutdown_agent(main.take()),
        shutdown_agent(fork.take().map(|(_, agent)| agent)),
    );
    let shutdown_error = main_shutdown.err().or_else(|| fork_shutdown.err());

    while let Some(result) = turns.join_next().await {
        finish_turn(Some(result), true, &mut controls, &mut cancelled, &updates);
    }

    drop(updates.send(WorkerEvent::Stopped {
        error: shutdown_error,
    }));
}

async fn start_turn(
    agent: &Nanocodex,
    request: TurnRequest,
    memory_review: MemoryReviewState,
    controls: &mut HashMap<TurnKey, TurnControl>,
    turns: &mut JoinSet<(TurnKey, TurnPurpose, bool, TurnResult)>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) -> bool {
    if request
        .shutdown
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        reject_cancelled_turn(request);
        return false;
    }
    let TurnRequest {
        pane,
        id,
        prompt,
        purpose,
        shutdown,
    } = request;
    let auxiliary = matches!(purpose, TurnPurpose::Auxiliary(_));
    let (isolated_agent, event_drain) = if auxiliary {
        let spawn = agent.spawn();
        let spawned = if let Some(scope) = shutdown.clone() {
            tokio::select! {
                result = spawn => result,
                () = scope.cancelled() => {
                    reject_cancelled_turn(TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose,
                        shutdown,
                    });
                    return false;
                }
            }
        } else {
            spawn.await
        };
        let (agent, mut events) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                reject_turn(
                    TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose,
                        shutdown,
                    },
                    error.to_string(),
                    updates,
                );
                return false;
            }
        };
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        (Some(agent), Some(drain))
    } else {
        (None, None)
    };
    let turn_agent = isolated_agent.as_ref().unwrap_or(agent);
    let agent_prompt = if auxiliary {
        prompt.agent_prompt()
    } else {
        memory_review.submission_prompt(&prompt)
    };
    let turn = match turn_agent.prompt(agent_prompt).await {
        Ok(turn) => turn,
        Err(error) => {
            if let Some(agent) = isolated_agent {
                drop(agent.shutdown().await);
            }
            if let Some(drain) = event_drain {
                drop(drain.await);
            }
            reject_turn(
                TurnRequest {
                    pane,
                    id,
                    prompt,
                    purpose,
                    shutdown,
                },
                error.to_string(),
                updates,
            );
            return false;
        }
    };
    let key = TurnKey { pane, id };
    let control = turn.control();
    let task_control = control.clone();
    controls.insert(key, control);
    turns.spawn(async move {
        let mut turn = Box::pin(turn);
        let (cancelled_by_scope, result) = match shutdown {
            Some(shutdown) => {
                tokio::select! {
                    result = turn.as_mut() => (false, result),
                    () = shutdown.cancelled() => {
                        drop(task_control.cancel().await);
                        (true, turn.await)
                    }
                }
            }
            None => (false, turn.await),
        };
        let result = result.map(|result| CompletedTurn {
            final_message: result.final_message().to_owned(),
            snapshot: (!auxiliary).then(|| Box::new(result.snapshot())),
        });
        if let Some(agent) = isolated_agent {
            drop(agent.shutdown().await);
        }
        if let Some(drain) = event_drain {
            drop(drain.await);
        }
        (key, purpose, cancelled_by_scope, result)
    });
    if !auxiliary {
        drop(updates.send(WorkerEvent::TurnAccepted { pane, id }));
    }
    !auxiliary
}

fn reject_turn(request: TurnRequest, error: String, updates: &mpsc::UnboundedSender<WorkerEvent>) {
    match request.purpose {
        TurnPurpose::Conversation => drop(updates.send(WorkerEvent::TurnFinished {
            pane: request.pane,
            id: request.id,
            error: Some(error),
            snapshot: None,
        })),
        TurnPurpose::Auxiliary(completion) => {
            drop(completion.send(Err(AuxiliaryError::Failed(error))));
        }
    }
}

fn reject_cancelled_turn(request: TurnRequest) {
    if let TurnPurpose::Auxiliary(completion) = request.purpose {
        drop(completion.send(Err(AuxiliaryError::Cancelled)));
    }
}

async fn steer_turn(
    agent: &Nanocodex,
    memory_review: MemoryReviewState,
    controls: &mut HashMap<TurnKey, TurnControl>,
    turns: &mut JoinSet<(TurnKey, TurnPurpose, bool, TurnResult)>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    request: SteerRequest,
) -> bool {
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
        match control.steer(memory_review.steer_prompt(&prompt)).await {
            Ok(()) => {
                drop(updates.send(WorkerEvent::SteerAdmitted { pane, queue_id }));
                return false;
            }
            Err(NanocodexError::TurnNotSteerable) => {}
            Err(error) => {
                drop(updates.send(WorkerEvent::SteerFailed {
                    pane,
                    queue_id,
                    error: error.to_string(),
                }));
                return false;
            }
        }
    }

    match agent.prompt(memory_review.steer_prompt(&prompt)).await {
        Ok(turn) => {
            let control = turn.control();
            let key = TurnKey {
                pane,
                id: fallback_id,
            };
            turns.spawn(async move {
                let result = turn.await.map(|result| CompletedTurn {
                    final_message: result.final_message().to_owned(),
                    snapshot: Some(Box::new(result.snapshot())),
                });
                (key, TurnPurpose::Conversation, false, result)
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
            true
        }
        Err(error) => {
            drop(updates.send(WorkerEvent::SteerFailed {
                pane,
                queue_id,
                error: error.to_string(),
            }));
            false
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
    result: Option<Result<(TurnKey, TurnPurpose, bool, TurnResult), JoinError>>,
    shutting_down: bool,
    controls: &mut HashMap<TurnKey, TurnControl>,
    cancelled: &mut HashSet<TurnKey>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let Some(result) = result else {
        return;
    };
    let (key, purpose, cancelled_by_scope, result) = match result {
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
    match purpose {
        TurnPurpose::Conversation => {
            let (error, snapshot) = match result {
                Ok(completed) => (None, completed.snapshot),
                Err(NanocodexError::TurnCancelled)
                    if shutting_down || was_cancelled || cancelled_by_scope =>
                {
                    (None, None)
                }
                Err(error) => (Some(error.to_string()), None),
            };
            drop(updates.send(WorkerEvent::TurnFinished {
                pane: key.pane,
                id: key.id,
                error,
                snapshot,
            }));
        }
        TurnPurpose::Auxiliary(completion) => {
            let result = match result {
                Ok(completed) => Ok(completed.final_message),
                Err(NanocodexError::TurnCancelled)
                    if shutting_down || was_cancelled || cancelled_by_scope =>
                {
                    Err(AuxiliaryError::Cancelled)
                }
                Err(error) => Err(AuxiliaryError::Failed(error.to_string())),
            };
            drop(completion.send(result));
        }
    }
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

async fn close_pane(
    pane: PaneId,
    agent: Option<Nanocodex>,
    controls: &HashMap<TurnKey, TurnControl>,
    cancelled: &mut HashSet<TurnKey>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let (keys, mut error) = cancel_turns(controls, Some(pane)).await;
    let count = keys.len();
    cancelled.extend(keys);
    if let Err(shutdown_error) = shutdown_agent(agent).await
        && error.is_none()
    {
        error = Some(shutdown_error.to_string());
    }
    drop(updates.send(WorkerEvent::TurnsCancelled { pane, count, error }));
}

async fn shutdown_agent(agent: Option<Nanocodex>) -> Result<(), NanocodexError> {
    let Some(agent) = agent else {
        return Ok(());
    };
    agent.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::{MemoryReviewState, WorkerCommand, WorkerEvent, spawn};
    use crate::{
        app::config::ReasoningEffort,
        core::MEMORY_REVIEW_CHECKPOINT,
        tui::{components::QueueId, pane::PaneId, prompt::Submission, transcript::TurnId},
    };
    use nanocodex::{
        AgentEvents, Nanocodex, OpenAi,
        agent::input::{Prompt, PromptInput, UserInput},
        oai::{
            ResponseError,
            tower::{ResponsesAttempt, ResponsesServiceResponse},
        },
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
    use tokio::{
        sync::{Notify, oneshot},
        time::timeout,
    };
    use tokio_util::sync::CancellationToken;
    use tower::Service;

    #[derive(Clone)]
    struct PendingService {
        called: Arc<Notify>,
        calls: Arc<AtomicUsize>,
    }

    impl Service<ResponsesAttempt> for PendingService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
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
        let openai = OpenAi::builder("test-key")
            .service(move || PendingService {
                called: Arc::clone(&called),
                calls: Arc::clone(&calls),
            })
            .build()
            .unwrap();
        Nanocodex::builder(openai).build().unwrap()
    }

    fn prompt_text(prompt: Prompt) -> String {
        match prompt.instruction {
            PromptInput::Text(text) => text,
            PromptInput::Content(content) => content
                .into_iter()
                .filter_map(|item| match item {
                    UserInput::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    #[test]
    fn review_state_decorates_followups_and_steers_without_changing_display_text() {
        let initial = Submission::text("initial request".to_owned());
        let follow_up = Submission::text("actually, preserve ordering".to_owned());
        let steer = Submission::text("change direction".to_owned());
        let mut review = MemoryReviewState::fresh(true);

        assert!(
            !prompt_text(review.submission_prompt(&initial)).contains(MEMORY_REVIEW_CHECKPOINT)
        );
        review.turn_accepted();
        assert_eq!(
            prompt_text(review.submission_prompt(&follow_up))
                .matches(MEMORY_REVIEW_CHECKPOINT)
                .count(),
            1
        );
        assert_eq!(
            prompt_text(MemoryReviewState::fresh(true).steer_prompt(&steer))
                .matches(MEMORY_REVIEW_CHECKPOINT)
                .count(),
            1
        );
        assert_eq!(follow_up.display_text(), "actually, preserve ordering");
        assert_eq!(steer.display_text(), "change direction");

        let disabled = MemoryReviewState::fresh(false);
        assert!(!prompt_text(disabled.steer_prompt(&steer)).contains(MEMORY_REVIEW_CHECKPOINT));
    }

    #[test]
    fn restored_and_forked_sessions_start_with_followup_review() {
        let prompt = Submission::text("continue".to_owned());

        assert!(
            prompt_text(MemoryReviewState::restored(true).submission_prompt(&prompt))
                .contains(MEMORY_REVIEW_CHECKPOINT)
        );
        assert!(
            prompt_text(
                MemoryReviewState::fresh(true)
                    .forked()
                    .submission_prompt(&prompt)
            )
            .contains(MEMORY_REVIEW_CHECKPOINT)
        );
    }

    #[tokio::test]
    async fn thinking_can_change_while_a_turn_is_active() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) =
            spawn(agent, MemoryReviewState::fresh(false), shutdown.clone());
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
        let (commands, mut updates) =
            spawn(agent, MemoryReviewState::fresh(false), shutdown.clone());
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
        let (commands, mut updates) =
            spawn(agent, MemoryReviewState::fresh(false), shutdown.clone());
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
        let (commands, mut updates) =
            spawn(agent, MemoryReviewState::fresh(false), shutdown.clone());
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
        let (commands, mut updates) =
            spawn(agent, MemoryReviewState::fresh(false), shutdown.clone());
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
        let (commands, mut updates) =
            spawn(agent, MemoryReviewState::fresh(false), shutdown.clone());
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
    async fn auxiliary_job_is_isolated_and_has_targeted_cancellation() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let worker_shutdown = CancellationToken::new();
        let overview_shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(
            agent,
            MemoryReviewState::fresh(false),
            worker_shutdown.clone(),
        );
        let (completion, result) = oneshot::channel();

        commands
            .send(WorkerCommand::Auxiliary {
                pane: PaneId::Main,
                id: TurnId::new(7),
                prompt: "generate a visible overview".to_owned().into(),
                shutdown: overview_shutdown.clone(),
                completion,
            })
            .unwrap();
        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the isolated model request should start");
        assert!(
            timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(50), updates.recv())
                .await
                .is_err()
        );

        overview_shutdown.cancel();
        assert!(
            timeout(Duration::from_secs(5), result)
                .await
                .expect("the overview completion should resolve")
                .expect("the worker should return a result")
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(50), updates.recv())
                .await
                .is_err()
        );

        worker_shutdown.cancel();
        timeout(Duration::from_secs(5), async {
            while !matches!(updates.recv().await, Some(WorkerEvent::Stopped { .. })) {}
        })
        .await
        .expect("the worker should stop");
        assert!(matches!(
            timeout(Duration::from_secs(5), events.recv()).await,
            Ok(None)
        ));
    }

    #[tokio::test]
    async fn cancelled_auxiliary_job_never_calls_the_model_or_emits_turn_events() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), Arc::clone(&calls));
        let worker_shutdown = CancellationToken::new();
        let job_shutdown = CancellationToken::new();
        job_shutdown.cancel();
        let (commands, mut updates) = spawn(
            agent,
            MemoryReviewState::fresh(false),
            worker_shutdown.clone(),
        );
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        let (completion, result) = oneshot::channel();

        commands
            .send(WorkerCommand::Auxiliary {
                pane: PaneId::Main,
                id: TurnId::new(7),
                prompt: "do not run".to_owned().into(),
                shutdown: job_shutdown,
                completion,
            })
            .unwrap();

        assert_eq!(
            timeout(Duration::from_secs(5), result)
                .await
                .expect("the completion should resolve")
                .expect("the worker should send a completion"),
            Err(super::AuxiliaryError::Cancelled),
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(
            timeout(Duration::from_millis(50), updates.recv())
                .await
                .is_err()
        );

        worker_shutdown.cancel();
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
    async fn closing_a_pane_waits_for_agent_cleanup() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) =
            spawn(agent, MemoryReviewState::fresh(false), shutdown.clone());
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });

        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "close this pane".to_owned().into(),
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
            .send(WorkerCommand::ClosePane(PaneId::Main))
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
                    Some(_) => {}
                    None => panic!("worker stopped before the pane closed"),
                }
            }
        })
        .await
        .expect("the pane should close");
        timeout(Duration::from_secs(5), drain)
            .await
            .expect("the event stream should close after agent shutdown")
            .expect("the drain task should not panic");

        shutdown.cancel();
        assert!(matches!(
            timeout(Duration::from_secs(5), updates.recv()).await,
            Ok(Some(WorkerEvent::Stopped { error: None }))
        ));
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
        let (commands, mut updates) = spawn(
            first_agent,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );

        commands
            .send(WorkerCommand::ReplaceAgent {
                pane: PaneId::Main,
                agent: second_agent,
                memory_review: MemoryReviewState::fresh(false),
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
