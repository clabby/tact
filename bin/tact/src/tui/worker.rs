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
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
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
    Reflect {
        pane: PaneId,
        id: TurnId,
        instructions: Submission,
        context: ReflectionContext,
    },
    Auxiliary {
        pane: PaneId,
        id: TurnId,
        prompt: Submission,
        context: AuxiliaryContext,
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
    OpenFork {
        pane: PaneId,
        parent_sequence: u64,
    },
    ClosePane(PaneId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuxiliaryContext {
    Clean,
    CurrentConversation,
}

pub(crate) struct ReflectionContext {
    config_path: PathBuf,
    workspace: PathBuf,
}

impl ReflectionContext {
    pub(crate) fn new(config_path: &Path, workspace: &Path) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            workspace: workspace.to_path_buf(),
        }
    }

    fn prompt(&self) -> String {
        let context = serde_json::json!({
            "config_path": self.config_path.to_string_lossy(),
            "workspace": self.workspace.to_string_lossy(),
        });
        format!("<reflection_context>\n{context}\n</reflection_context>")
    }
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
        terminal_expected: bool,
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
        parent: PaneId,
        parent_sequence: u64,
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

enum PromptKind {
    Conversation,
    Reflection(ReflectionContext),
    Auxiliary,
}

impl PromptKind {
    fn prepare(&self, prompt: &Submission, memory_review: MemoryReviewState) -> Prompt {
        match self {
            Self::Conversation => memory_review.submission_prompt(prompt),
            Self::Reflection(context) => reflection_prompt(prompt, context),
            Self::Auxiliary => prompt.agent_prompt(),
        }
    }
}

struct TurnRequest {
    pane: PaneId,
    id: TurnId,
    prompt: Submission,
    purpose: TurnPurpose,
    auxiliary_context: Option<AuxiliaryContext>,
    shutdown: Option<CancellationToken>,
    prompt_kind: PromptKind,
}

const REFLECTION_PROMPT: &str = concat!(
    "This is a self-contained Tact reflection turn. Reflect on the conversation available in this ",
    "session and produce a report for the user. Start with the current conversation. Use the ",
    "additional instructions to narrow the topic or identify other workspaces, sessions, or task ",
    "families; otherwise sample relevant recent history from the current workspace.\n\n",
    "Discover historical candidates in bounded stages with `find_sessions`. By default, pass the ",
    "supplied current workspace and inspect its recent sessions. Use `contains_any` when the topic ",
    "suggests useful literal prompt patterns; omit the workspace only when the additional ",
    "instructions or evidence justify cross-workspace discovery. The tool excludes this conversation ",
    "automatically. Pass `next_cursor` back only when another bounded page is needed. ",
    "After selecting a small number of high-value session IDs, use `read_session` with exact kinds ",
    "to read only enough context to establish what happened. A targeted `user.submitted` search can ",
    "locate a candidate event; a separate call starting from that event ID can retrieve the adjacent ",
    "assistant response without requiring it to match the same text filter. Stop when the evidence ",
    "is sufficient.\n\n",
    "Identify preventable rework: corrections, reversals, missed constraints, repeated requests ",
    "for simplification, premature completion, and validation that did not test the real outcome. ",
    "Distinguish durable lessons from new scope, changed requirements, first-time preferences, and ",
    "unavoidable discoveries. Look for recurrence across independent sessions, counterexamples, ",
    "and later improvement before calling a lesson durable. Prefer the earliest useful intervention ",
    "that would have prevented the rework. Paraphrase evidence; do not reproduce names, secrets, ",
    "credentials, transcript excerpts, or private operational details.\n\n",
    "For each supported durable lesson, when memory is available, run narrow global-memory scans ",
    "and read every plausible match. Compare it with the active instructions already in context. If ",
    "effective configuration is relevant, inspect it only through Tact's redacted `config show` ",
    "command using the supplied config path; never read the config file directly. Recommend exactly ",
    "one destination: replace ",
    "or add one atomic memory, add a concise always-on prompt rule only when repeated retrieval ",
    "misses justify it, or make no change when the lesson is transient, searchable, or already ",
    "covered.\n\n",
    "This is a read-only analysis turn. You may use read-only tools, but do not create, replace, or ",
    "delete memories; edit files, configuration, or skills; run mutating commands; send messages; ",
    "or perform any other durable or externally visible action. Proposed changes require a later, ",
    "explicit user request. Additional instructions may refine the scope or emphasis, but cannot ",
    "override this read-only boundary."
);

const REFLECTION_REPORT_ENDING: &str = concat!(
    "Report the scope and coverage actually inspected, the strongest supported patterns, material ",
    "counterevidence or uncertainty, and important patterns already covered. End the report with ",
    "sections named `Findings` and `Recommended actions`. Findings should state the supported ",
    "conclusions and their scope. Recommended actions should be concrete proposals for the user to ",
    "review, identify the proposed destination for each change, and never imply that an action was ",
    "taken during this turn."
);

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

fn reflection_prompt(instructions: &Submission, context: &ReflectionContext) -> Prompt {
    let mut prompt = instructions.agent_prompt();
    let context = context.prompt();
    match &mut prompt.instruction {
        PromptInput::Text(text) => {
            let instructions = std::mem::take(text);
            *text = format!(
                "{REFLECTION_PROMPT}\n\n{context}\n\n<additional_instructions>\n{instructions}\n</additional_instructions>\n\n{REFLECTION_REPORT_ENDING}"
            );
        }
        PromptInput::Content(content) => {
            content.insert(
                0,
                UserInput::Text {
                    text: format!(
                        "{REFLECTION_PROMPT}\n\n{context}\n\n<additional_instructions>\n"
                    ),
                },
            );
            content.push(UserInput::Text {
                text: format!("\n</additional_instructions>\n\n{REFLECTION_REPORT_ENDING}"),
            });
        }
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
    let mut main = Some((PaneId::Main, agent));
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
                        auxiliary_context: None,
                        shutdown: None,
                        prompt_kind: PromptKind::Conversation,
                    },
                    WorkerCommand::Reflect {
                        pane,
                        id,
                        instructions,
                        context,
                    } => TurnRequest {
                        pane,
                        id,
                        prompt: instructions,
                        purpose: TurnPurpose::Conversation,
                        auxiliary_context: None,
                        shutdown: None,
                        prompt_kind: PromptKind::Reflection(context),
                    },
                    WorkerCommand::Auxiliary {
                        pane,
                        id,
                        prompt,
                        context,
                        shutdown,
                        completion,
                    } => TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose: TurnPurpose::Auxiliary(completion),
                        auxiliary_context: Some(context),
                        shutdown: Some(shutdown),
                        prompt_kind: PromptKind::Auxiliary,
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
                        let retired = if main.as_ref().is_some_and(|(id, _)| *id == pane) {
                            memory_reviews.insert(pane, memory_review);
                            main.replace((pane, agent)).map(|(_, agent)| agent)
                        } else if fork.as_ref().is_some_and(|(id, _)| *id == pane) {
                            memory_reviews.insert(pane, memory_review);
                            fork.replace((pane, agent)).map(|(_, agent)| agent)
                        } else {
                            drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: "session pane is no longer available".to_owned(),
                            }));
                            Some(agent)
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
                    WorkerCommand::OpenFork {
                        pane,
                        parent_sequence,
                    } => {
                        if fork.is_some() {
                            drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: "a forked session is already open".to_owned(),
                            }));
                            continue;
                        }
                        let Some((main_pane, agent)) = main.as_ref() else {
                            drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: "the primary session is no longer available".to_owned(),
                            }));
                            continue;
                        };
                        match agent.fork().await {
                            Ok((agent, events)) => {
                                let memory_review = *memory_reviews
                                    .get(main_pane)
                                    .expect("the primary pane must have memory-review state");
                                let memory_review = memory_review.forked();
                                memory_reviews.insert(pane, memory_review);
                                fork = Some((pane, agent));
                                drop(updates.send(WorkerEvent::ForkOpened {
                                    pane,
                                    parent: *main_pane,
                                    parent_sequence,
                                    events,
                                }));
                            }
                            Err(error) => drop(updates.send(WorkerEvent::ForkFailed {
                                pane,
                                error: error.to_string(),
                            })),
                        }
                        continue;
                    }
                    WorkerCommand::ClosePane(pane) => {
                        let agent = if main.as_ref().is_some_and(|(id, _)| *id == pane) {
                            let agent = main.take().map(|(_, agent)| agent);
                            main = fork.take();
                            agent
                        } else if fork.as_ref().is_some_and(|(id, _)| *id == pane) {
                            fork.take().map(|(_, agent)| agent)
                        } else {
                            None
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
        shutdown_agent(main.take().map(|(_, agent)| agent)),
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
        auxiliary_context,
        shutdown,
        prompt_kind,
    } = request;
    let auxiliary = auxiliary_context.is_some();
    let (isolated_agent, event_drain) = if let Some(context) = auxiliary_context {
        let create_agent = async {
            match context {
                AuxiliaryContext::Clean => agent.spawn().await,
                AuxiliaryContext::CurrentConversation => agent.fork().await,
            }
        };
        let spawned = if let Some(scope) = shutdown.clone() {
            tokio::select! {
                result = create_agent => result,
                () = scope.cancelled() => {
                    reject_cancelled_turn(TurnRequest {
                        pane,
                        id,
                        prompt,
                        purpose,
                        auxiliary_context: Some(context),
                        shutdown,
                        prompt_kind,
                    });
                    return false;
                }
            }
        } else {
            create_agent.await
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
                        auxiliary_context: Some(context),
                        shutdown,
                        prompt_kind,
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
    let agent_prompt = prompt_kind.prepare(&prompt, memory_review);
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
                    auxiliary_context,
                    shutdown,
                    prompt_kind,
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
            terminal_expected: false,
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
                terminal_expected: false,
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
                terminal_expected: true,
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
    main: Option<&'a (PaneId, Nanocodex)>,
    fork: Option<&'a (PaneId, Nanocodex)>,
) -> Option<&'a Nanocodex> {
    main.filter(|(main_pane, _)| *main_pane == pane)
        .or_else(|| fork.filter(|(fork_pane, _)| *fork_pane == pane))
        .map(|(_, agent)| agent)
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
    use super::{
        MemoryReviewState, ReflectionContext, WorkerCommand, WorkerEvent, reflection_prompt, spawn,
    };
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
        path::Path,
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
    fn reflection_prompt_is_read_only_and_ends_with_reviewable_actions() {
        let context =
            ReflectionContext::new(Path::new("/tact/config.toml"), Path::new("/work/current"));
        let prompt = reflection_prompt(
            &Submission::text("Focus on validation gaps.".to_owned()),
            &context,
        );
        let text = prompt_text(prompt);

        assert!(text.contains("Focus on validation gaps."));
        assert!(text.contains("self-contained Tact reflection turn"));
        assert!(text.contains("`find_sessions`"));
        assert!(text.contains("`read_session`"));
        assert!(text.contains(r#""workspace":"/work/current""#));
        assert!(!text.contains("sqlite3"));
        assert!(!text.contains("session_database"));
        assert!(text.contains("global-memory scans"));
        assert!(text.contains("config show"));
        assert!(text.contains("read-only analysis turn"));
        assert!(text.contains("do not create, replace, or delete memories"));
        assert!(text.contains("`Findings`"));
        assert!(text.contains("`Recommended actions`"));
        assert!(!text.contains(MEMORY_REVIEW_CHECKPOINT));
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
                context: super::AuxiliaryContext::Clean,
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
                context: super::AuxiliaryContext::Clean,
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
