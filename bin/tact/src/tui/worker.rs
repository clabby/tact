//! Independently scheduled Nanocodex turn worker.

use crate::{
    app::config::{ReasoningEffort, Speed},
    core::{IMAGE_RENDERING_INSTRUCTIONS, MEMORY_REVIEW_CHECKPOINT, set_speed},
    tui::{
        components::QueueId,
        context::ContextBudget,
        pane::PaneId,
        prompt::Submission,
        session::AgentSnapshot,
        transcript::{TerminalStopReason, TurnId},
    },
};
use nanocodex::{
    AgentEvents, HarnessModel, Nanocodex, NanocodexError, TurnControl,
    agent::input::{Prompt, PromptInput, UserInput},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use tact_subagents::AgentContext;
use tokio::{
    sync::{mpsc, oneshot},
    task::{Id as TaskId, JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;

pub(crate) enum WorkerCommand {
    Compact(PaneId),
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
        context: AgentContext,
        memory_review: MemoryReviewState,
    },
    SetThinking {
        pane: PaneId,
        effort: ReasoningEffort,
    },
    SetSpeed {
        pane: PaneId,
        speed: Speed,
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
    CompactionFinished {
        pane: PaneId,
        result: Result<Box<AgentSnapshot>, String>,
        terminal_stop: Option<TerminalStopReason>,
        duration_ns: u64,
    },
    ContextBudget {
        pane: PaneId,
        session_id: String,
        budget: ContextBudget,
    },
    TurnAccepted {
        pane: PaneId,
        id: TurnId,
    },
    TurnFinished {
        pane: PaneId,
        id: TurnId,
        error: Option<String>,
        terminal_stop: Option<TerminalStopReason>,
        snapshot: Option<Box<AgentSnapshot>>,
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
    SpeedUpdated {
        pane: PaneId,
        speed: Speed,
        result: Result<(), NanocodexError>,
    },
    Stopped {
        error: Option<NanocodexError>,
    },
}

type TurnResult = Result<CompletedTurn, NanocodexError>;

struct CompletedCompaction {
    pane: PaneId,
    result: Result<Box<AgentSnapshot>, NanocodexError>,
    duration_ns: u64,
}

struct CompletedTurn {
    final_message: String,
    snapshot: Option<Box<AgentSnapshot>>,
}

async fn complete_turn(
    agent: &Nanocodex,
    result: Result<nanocodex::agent::TurnResult, NanocodexError>,
    claude: bool,
    auxiliary: bool,
    pane: PaneId,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) -> TurnResult {
    let snapshot = if auxiliary {
        Ok(None)
    } else if claude {
        agent
            .runtime_snapshot()
            .await
            .and_then(AgentSnapshot::from_claude)
            .map(Some)
    } else {
        Ok(result
            .as_ref()
            .ok()
            .and_then(|result| result.snapshot())
            .map(|snapshot| AgentSnapshot::Codex(Box::new(snapshot))))
    };
    if let Ok(Some(snapshot)) = &snapshot {
        publish_context_budget(snapshot, agent.session_id(), pane, updates);
    }
    // Failed turns may update telemetry but never replace the successful resume checkpoint.
    let result = result?;
    Ok(CompletedTurn {
        final_message: result.final_message().to_owned(),
        snapshot: snapshot?.map(Box::new),
    })
}

fn publish_context_budget(
    snapshot: &AgentSnapshot,
    session_id: &str,
    pane: PaneId,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    if let Some(budget) = snapshot.context_budget() {
        drop(updates.send(WorkerEvent::ContextBudget {
            pane,
            session_id: session_id.to_owned(),
            budget,
        }));
    }
}

async fn observe_initial_context(
    agent: &Nanocodex,
    model: HarnessModel,
    pane: PaneId,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    if matches!(model, HarnessModel::Claude(_))
        && let Ok(snapshot) = agent
            .runtime_snapshot()
            .await
            .and_then(AgentSnapshot::from_claude)
    {
        publish_context_budget(&snapshot, agent.session_id(), pane, updates);
    }
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
    "automatically. Use `parent_session_id` to avoid counting forks or descendants as independent ",
    "evidence. Pass `next_cursor` back only when another bounded page is needed. ",
    "After selecting a small number of high-value session IDs, use `read_session` with exact kinds ",
    "to read only enough context to establish what happened. Targeted searches over both ",
    "`user.submitted` and `user.steered` can locate candidate corrections; a separate call starting ",
    "from a matched event ID can retrieve the adjacent assistant response without requiring it to ",
    "match the same text filter. Stop when the evidence is sufficient.\n\n",
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
    "counterevidence or uncertainty, and important patterns already covered. Do not claim exact ",
    "frequencies unless the relevant scope was inspected exhaustively; otherwise describe recurrence ",
    "as sampled evidence. End the report with sections named `Findings` and `Recommended actions`. ",
    "Findings should state the supported conclusions and their scope. Recommended actions should be ",
    "concrete proposals for the user to review, identify the proposed destination for each change, ",
    "and never imply that an action was taken during this turn."
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
            Self::Disabled | Self::BeforeFirstTurn => prompt_with_image_rendering(submission),
            Self::FollowUp => prompt_with_memory_review(submission),
        }
    }

    fn steer_prompt(self, submission: &Submission) -> Prompt {
        match self {
            Self::Disabled => prompt_with_image_rendering(submission),
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
    let mut prompt = prompt_with_image_rendering(submission);
    append_prompt_instructions(&mut prompt, MEMORY_REVIEW_CHECKPOINT);
    prompt
}

fn prompt_with_image_rendering(submission: &Submission) -> Prompt {
    let mut prompt = submission.agent_prompt();
    append_prompt_instructions(&mut prompt, IMAGE_RENDERING_INSTRUCTIONS);
    prompt
}

fn append_prompt_instructions(prompt: &mut Prompt, instructions: &str) {
    match &mut prompt.instruction {
        PromptInput::Text(text) => {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(instructions);
        }
        PromptInput::Content(content) => content.push(UserInput::Text {
            text: format!("\n\n{instructions}"),
        }),
    }
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

struct PaneAgent {
    agent: Nanocodex,
    context: AgentContext,
}

pub(crate) fn spawn(
    agent: Nanocodex,
    context: AgentContext,
    memory_review: MemoryReviewState,
    shutdown: CancellationToken,
) -> (
    mpsc::UnboundedSender<WorkerCommand>,
    mpsc::UnboundedReceiver<WorkerEvent>,
) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (updates, update_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(
        agent,
        context,
        memory_review,
        command_rx,
        updates,
        shutdown,
    ));
    (commands, update_rx)
}

async fn run(
    agent: Nanocodex,
    context: AgentContext,
    memory_review: MemoryReviewState,
    mut commands: mpsc::UnboundedReceiver<WorkerCommand>,
    updates: mpsc::UnboundedSender<WorkerEvent>,
    shutdown: CancellationToken,
) {
    observe_initial_context(&agent, context.model, PaneId::Main, &updates).await;
    let mut main = Some((PaneId::Main, PaneAgent { agent, context }));
    let mut fork = None::<(PaneId, PaneAgent)>;
    let mut controls = HashMap::<TurnKey, TurnControl>::new();
    let mut memory_reviews = HashMap::from([(PaneId::Main, memory_review)]);
    let mut cancelled = HashSet::<TurnKey>::new();
    let mut turns = JoinSet::<(TurnKey, TurnPurpose, bool, TurnResult)>::new();
    let mut compactions = JoinSet::new();
    let mut compacting = HashMap::<PaneId, TaskId>::new();

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            Some(result) = compactions.join_next(), if !compactions.is_empty() => {
                finish_compaction(result, &mut compacting, &updates);
            }
            result = turns.join_next(), if !turns.is_empty() => {
                finish_turn(result, false, &mut controls, &mut cancelled, &updates);
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                let request = match command {
                    WorkerCommand::Compact(pane) => {
                        let rejection = if controls.keys().any(|key| key.pane == pane) || compacting.contains_key(&pane) {
                            Some("finish active work before compacting context")
                        } else if agent_for(pane, main.as_ref(), fork.as_ref()).is_none() {
                            Some("session pane is no longer available")
                        } else { None };
                        if let Some(error) = rejection {
                            drop(updates.send(WorkerEvent::CompactionFinished { pane, result: Err(error.to_owned()), terminal_stop: None, duration_ns: 0 }));
                            continue;
                        }
                        let agent = agent_for(pane, main.as_ref(), fork.as_ref()).unwrap();
                        let claude = matches!(agent.context.model, HarnessModel::Claude(_));
                        let agent = agent.agent.clone();
                        let task = compactions.spawn(async move {
                            let started = std::time::Instant::now();
                            let operation = async {
                                agent.compact().await?;
                                if claude {
                                    agent.runtime_snapshot().await.and_then(AgentSnapshot::from_claude)
                                } else {
                                    agent.snapshot().await.map(|snapshot| AgentSnapshot::Codex(Box::new(snapshot)))
                                }
                            };
                            let result = operation.await.map(Box::new);
                            CompletedCompaction { pane, result, duration_ns: u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX) }
                        });
                        compacting.insert(pane, task.id());
                        continue;
                    }
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
                        if compacting.contains_key(&pane) {
                            drop(updates.send(WorkerEvent::SteerFailed { pane, queue_id, error: "context compaction is still running".to_owned() }));
                            continue;
                        }
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
                        context,
                        memory_review,
                    } => {
                        debug_assert!(!controls.keys().any(|key| key.pane == pane));
                        observe_initial_context(&agent, context.model, pane, &updates).await;
                        let retired = if main.as_ref().is_some_and(|(id, _)| *id == pane) {
                            memory_reviews.insert(pane, memory_review);
                            main.replace((pane, PaneAgent { agent, context })).map(|(_, agent)| agent.agent)
                        } else if fork.as_ref().is_some_and(|(id, _)| *id == pane) {
                            memory_reviews.insert(pane, memory_review);
                            fork.replace((pane, PaneAgent { agent, context })).map(|(_, agent)| agent.agent)
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
                        if compacting.contains_key(&pane) {
                            drop(updates.send(WorkerEvent::ThinkingUpdated { pane, effort, result: Err(NanocodexError::InvalidRequest("context compaction is still running".to_owned())) }));
                            continue;
                        }
                        let result = match main.iter_mut().chain(fork.iter_mut()).find(|(id, _)| *id == pane) {
                            Some((_, agent)) => {
                                let result = agent.agent.set_thinking(effort.into()).await;
                                if result.is_ok() {
                                    agent.context.thinking = effort.into();
                                }
                                result
                            },
                            None => Err(NanocodexError::AgentStopped),
                        };
                        drop(updates.send(WorkerEvent::ThinkingUpdated {
                            pane,
                            effort,
                            result,
                        }));
                        continue;
                    }
                    WorkerCommand::SetSpeed { pane, speed } => {
                        if compacting.contains_key(&pane) {
                            drop(updates.send(WorkerEvent::SpeedUpdated { pane, speed, result: Err(NanocodexError::InvalidRequest("context compaction is still running".to_owned())) }));
                            continue;
                        }
                        let result = match agent_for(pane, main.as_ref(), fork.as_ref()) {
                            Some(agent) => set_speed(&agent.agent, agent.context.model, speed).await,
                            None => Err(NanocodexError::AgentStopped),
                        };
                        drop(updates.send(WorkerEvent::SpeedUpdated {
                            pane,
                            speed,
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
                        if compacting.contains_key(main_pane) {
                            drop(updates.send(WorkerEvent::ForkFailed { pane, error: "context compaction is still running".to_owned() }));
                            continue;
                        }
                        let context = agent.context;
                        if matches!(context.model, HarnessModel::Claude(_)) {
                            drop(updates.send(WorkerEvent::ForkFailed { pane, error: "Claude does not support forking the current conversation".to_owned() }));
                            continue;
                        }
                        match agent.agent.fork().await {
                            Ok((agent, events)) => {
                                let memory_review = *memory_reviews
                                    .get(main_pane)
                                    .expect("the primary pane must have memory-review state");
                                let memory_review = memory_review.forked();
                                memory_reviews.insert(pane, memory_review);
                                fork = Some((pane, PaneAgent { agent, context }));
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
                            let agent = main.take().map(|(_, agent)| agent.agent);
                            main = fork.take();
                            agent
                        } else if fork.as_ref().is_some_and(|(id, _)| *id == pane) {
                            fork.take().map(|(_, agent)| agent.agent)
                        } else {
                            None
                        };
                        memory_reviews.remove(&pane);
                        close_pane(pane, agent, &controls, &mut cancelled, &updates).await;
                        continue;
                    }
                };
                if compacting.contains_key(&request.pane) {
                    reject_turn(request, "context compaction is still running".to_owned(), &updates);
                    continue;
                }
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
        shutdown_agent(main.take().map(|(_, agent)| agent.agent)),
        shutdown_agent(fork.take().map(|(_, agent)| agent.agent)),
    );
    let shutdown_error = main_shutdown.err().or_else(|| fork_shutdown.err());

    while let Some(result) = turns.join_next().await {
        finish_turn(Some(result), true, &mut controls, &mut cancelled, &updates);
    }

    while let Some(result) = compactions.join_next().await {
        finish_compaction(result, &mut compacting, &updates);
    }
    drop(updates.send(WorkerEvent::Stopped {
        error: shutdown_error,
    }));
}

fn finish_compaction(
    completion: Result<CompletedCompaction, JoinError>,
    compacting: &mut HashMap<PaneId, TaskId>,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let (pane, result, duration_ns, terminal_stop) = match completion {
        Ok(CompletedCompaction {
            pane,
            result,
            duration_ns,
        }) => {
            let terminal_stop = result.as_ref().err().and_then(terminal_stop_reason);
            (
                pane,
                result.map_err(|error| error.to_string()),
                duration_ns,
                terminal_stop,
            )
        }
        Err(error) => {
            let Some((&pane, _)) = compacting.iter().find(|(_, id)| **id == error.id()) else {
                return;
            };
            (
                pane,
                Err(format!("compaction task stopped unexpectedly: {error}")),
                0,
                None,
            )
        }
    };
    compacting.remove(&pane);
    drop(updates.send(WorkerEvent::CompactionFinished {
        pane,
        result,
        terminal_stop,
        duration_ns,
    }));
}

fn terminal_stop_reason(error: &NanocodexError) -> Option<TerminalStopReason> {
    if let NanocodexError::Shutdown(source) = error {
        return terminal_stop_reason(source);
    }
    (matches!(
        error,
        NanocodexError::CompactionFailed {
            requires_session_stop: true,
            ..
        }
    ) || error
        .responses_error()
        .is_some_and(|source| source.is_misalignment_policy_violation()))
    .then_some(TerminalStopReason::MisalignmentPolicyViolation)
}

async fn start_turn(
    agent: &PaneAgent,
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
    // A native turn owns its control entry through checkpoint capture, so the next
    // prompt cannot change the committed conversation before it is copied.
    if matches!(agent.context.model, HarnessModel::Claude(_)) {
        let reason = if matches!(
            auxiliary_context,
            Some(AuxiliaryContext::CurrentConversation)
        ) {
            Some("Claude does not support forking the current conversation")
        } else if !auxiliary && controls.keys().any(|key| key.pane == pane) {
            Some("Claude is still finishing the current turn and its checkpoint")
        } else {
            None
        };
        if let Some(reason) = reason {
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
                reason.to_owned(),
                updates,
            );
            return false;
        }
    }
    let (isolated_agent, event_drain) = if let Some(context) = auxiliary_context {
        let create_agent = async {
            match context {
                AuxiliaryContext::Clean => agent.agent.spawn().await,
                AuxiliaryContext::CurrentConversation => agent.agent.fork().await,
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
    let turn_agent = isolated_agent.as_ref().unwrap_or(&agent.agent);
    let agent_prompt = agent
        .context
        .prompt(prompt_kind.prepare(&prompt, memory_review));
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
    let checkpoint_agent = agent.agent.clone();
    let claude = matches!(agent.context.model, HarnessModel::Claude(_));
    let context_updates = updates.clone();
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
        let result = complete_turn(
            &checkpoint_agent,
            result,
            claude,
            auxiliary,
            pane,
            &context_updates,
        )
        .await;
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
            terminal_stop: None,
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
    agent: &PaneAgent,
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

    if matches!(agent.context.model, HarnessModel::Claude(_))
        && controls.keys().any(|key| key.pane == pane)
    {
        drop(updates.send(WorkerEvent::SteerFailed {
            pane,
            queue_id,
            error: "Claude is still finishing the current turn and its checkpoint".to_owned(),
        }));
        return false;
    }

    match agent
        .agent
        .prompt(agent.context.prompt(memory_review.steer_prompt(&prompt)))
        .await
    {
        Ok(turn) => {
            let control = turn.control();
            let key = TurnKey {
                pane,
                id: fallback_id,
            };
            let checkpoint_agent = agent.agent.clone();
            let claude = matches!(agent.context.model, HarnessModel::Claude(_));
            let context_updates = updates.clone();
            turns.spawn(async move {
                let result = complete_turn(
                    &checkpoint_agent,
                    turn.await,
                    claude,
                    false,
                    pane,
                    &context_updates,
                )
                .await;
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
                terminal_stop: None,
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
            let (error, snapshot, terminal_stop) = match result {
                Ok(completed) => (None, completed.snapshot, None),
                Err(NanocodexError::TurnCancelled)
                    if shutting_down || was_cancelled || cancelled_by_scope =>
                {
                    (None, None, None)
                }
                Err(error) => {
                    let terminal_stop = terminal_stop_reason(&error);
                    (Some(error.to_string()), None, terminal_stop)
                }
            };
            drop(updates.send(WorkerEvent::TurnFinished {
                pane: key.pane,
                id: key.id,
                error,
                snapshot,
                terminal_stop,
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
    main: Option<&'a (PaneId, PaneAgent)>,
    fork: Option<&'a (PaneId, PaneAgent)>,
) -> Option<&'a PaneAgent> {
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
        MemoryReviewState, ReflectionContext, TurnKey, TurnPurpose, WorkerCommand, WorkerEvent,
        finish_turn, reflection_prompt, spawn,
    };
    use crate::{
        app::config::{ReasoningEffort, Speed},
        core::{IMAGE_RENDERING_INSTRUCTIONS, MEMORY_REVIEW_CHECKPOINT},
        tui::{
            components::QueueId,
            pane::PaneId,
            prompt::Submission,
            transcript::{TerminalStopReason, TurnId},
        },
    };
    use nanocodex::{
        AgentEvents, HarnessModel as Model, Model as CodexModel, Nanocodex, NanocodexError, OpenAi,
        Thinking,
        agent::input::{Prompt, PromptInput, UserInput},
        oai::{
            ResponseError,
            tower::{
                ResponsesAttempt, ResponsesAttemptKind, ResponsesServiceError,
                ResponsesServiceResponse,
            },
            transport::ResponsesError,
        },
    };
    use std::{
        collections::{HashMap, HashSet},
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
    use tact_subagents::AgentContext;
    use tokio::{
        sync::{Notify, mpsc, oneshot},
        time::timeout,
    };
    use tokio_util::sync::CancellationToken;
    use tower::Service;

    #[derive(Clone)]
    struct CompactionService {
        requests: mpsc::UnboundedSender<oneshot::Sender<StdResult<(), ResponsesError>>>,
    }

    impl Service<ResponsesAttempt> for CompactionService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = StdResult<Self::Response, Self::Error>> + Send>,
        >;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
            use nanocodex::oai::{
                responses::{ResponseItem, ResponseItemId},
                tower::{CompactionOutput, ResponsePipelineStats, ResponsesOutput},
            };
            assert!(matches!(request.kind(), ResponsesAttemptKind::Compaction));
            let (send, receive) = oneshot::channel();
            self.requests.send(send).unwrap();
            Box::pin(async move {
                if let Err(error) = receive.await.unwrap() {
                    return Err(ResponseError::from(ResponsesServiceError::from(error)));
                }
                Ok(ResponsesServiceResponse::new(ResponsesOutput::Compaction(
                    CompactionOutput {
                        id: "compact-response".to_owned(),
                        status: "completed".to_owned(),
                        item: ResponseItem::Compaction {
                            id: Some(ResponseItemId::from("compact-item")),
                            encrypted_content: "opaque-summary".into(),
                            created_by: None,
                            internal_chat_message_metadata_passthrough: None,
                        },
                        usage: None,
                        time_to_first_event_ns: 0,
                        time_to_first_output_ns: None,
                        pipeline_stats: ResponsePipelineStats::default(),
                    },
                )))
            })
        }
    }

    async fn stopped(updates: &mut mpsc::UnboundedReceiver<WorkerEvent>) {
        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(updates.recv().await.unwrap(), WorkerEvent::Stopped { .. }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    async fn compacted(
        updates: &mut mpsc::UnboundedReceiver<WorkerEvent>,
    ) -> Result<Box<crate::tui::session::AgentSnapshot>, String> {
        timeout(Duration::from_secs(5), async {
            loop {
                if let WorkerEvent::CompactionFinished { pane, result, .. } =
                    updates.recv().await.unwrap()
                {
                    assert_eq!(pane, PaneId::Main);
                    return result;
                }
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn manual_compaction_task_failure_releases_its_pane_and_reports_error() {
        let mut tasks = tokio::task::JoinSet::new();
        let task = tasks.spawn(async {
            pending::<()>().await;
            unreachable!()
        });
        let mut compacting = std::collections::HashMap::from([(PaneId::Main, task.id())]);
        let (updates, mut receive) = mpsc::unbounded_channel();
        task.abort();
        super::finish_compaction(tasks.join_next().await.unwrap(), &mut compacting, &updates);
        assert!(compacting.is_empty());
        assert!(
            compacted(&mut receive)
                .await
                .err()
                .unwrap()
                .contains("task stopped unexpectedly")
        );
    }

    #[tokio::test]
    async fn manual_compaction_captures_codex_snapshot_and_failure_has_no_checkpoint() {
        let (requests, mut receive) = mpsc::unbounded_channel();
        let openai = OpenAi::builder("test-key")
            .service(move || CompactionService {
                requests: requests.clone(),
            })
            .build()
            .unwrap();
        let (agent, mut events) = Nanocodex::builder(openai).build().unwrap();
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
        let release = timeout(Duration::from_secs(5), receive.recv())
            .await
            .unwrap()
            .unwrap();
        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(9),
                prompt: "must not race checkpoint".to_owned().into(),
            })
            .unwrap();
        match timeout(Duration::from_secs(5), updates.recv())
            .await
            .unwrap()
            .unwrap()
        {
            WorkerEvent::TurnFinished {
                error: Some(error),
                snapshot: None,
                ..
            } => assert!(error.contains("compaction")),
            _ => panic!("prompt must be rejected while compacting"),
        }
        release.send(Ok(())).unwrap();
        let snapshot = compacted(&mut updates).await.unwrap();
        assert!(
            serde_json::to_string(&snapshot)
                .unwrap()
                .contains("opaque-summary")
        );
        commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
        timeout(Duration::from_secs(5), receive.recv())
            .await
            .unwrap()
            .unwrap()
            .send(Err(ResponsesError::HttpRejected {
                status: 400,
                body: "synthetic compaction failure".to_owned(),
                retry_after: None,
            }))
            .unwrap();
        assert!(
            compacted(&mut updates)
                .await
                .err()
                .unwrap()
                .contains("synthetic compaction failure")
        );
        shutdown.cancel();
        stopped(&mut updates).await;
        drain.await.unwrap();
    }

    #[tokio::test]
    async fn manual_compaction_terminal_failure_stops_followups_and_resume() {
        use crate::tui::{
            session,
            transcript::{LocalEvent, TranscriptJournal},
        };
        let (requests, mut receive) = mpsc::unbounded_channel();
        let openai = OpenAi::builder("test-key")
            .service(move || CompactionService {
                requests: requests.clone(),
            })
            .build()
            .unwrap();
        let (agent, mut events) = Nanocodex::builder(openai).build().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
        timeout(Duration::from_secs(5), receive.recv())
            .await
            .unwrap()
            .unwrap()
            .send(Ok(()))
            .unwrap();
        let snapshot = compacted(&mut updates).await.unwrap();
        session::save_checkpoint(
            &config,
            "session",
            &snapshot.into_codex().unwrap(),
            "instructions",
            false,
        )
        .unwrap();
        commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
        timeout(Duration::from_secs(5), receive.recv()).await.unwrap().unwrap().send(Err(ResponsesError::HttpRejected {
            status: 403, body: r#"{"error":{"code":"misalignment_policy_violation","message":"conversation stopped"}}"#.to_owned(), retry_after: None,
        })).unwrap();
        let WorkerEvent::CompactionFinished {
            result,
            terminal_stop,
            duration_ns,
            ..
        } = timeout(Duration::from_secs(5), updates.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected compaction failure");
        };
        assert_eq!(
            terminal_stop,
            Some(TerminalStopReason::MisalignmentPolicyViolation)
        );
        let error = result
            .err()
            .expect("terminal compaction must not publish a snapshot");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session").unwrap();
        journal.defer_start(crate::tui::transcript::SessionStarted {
            session_id: "session".to_owned(),
            parent_session_id: None,
            parent_sequence: None,
            model: nanocodex::oai::MODEL.to_owned(),
            effort: ReasoningEffort::Medium,
            reasoning_mode: crate::app::config::ReasoningMode::Standard,
            speed: Speed::Standard,
            workspace: directory.path().to_path_buf(),
            application_version: "test".to_owned(),
        });
        journal
            .append_local(LocalEvent::CompactionFinished {
                error: Some(error),
                terminal_stop,
                duration_ns,
            })
            .unwrap();
        journal.flush().await.unwrap();
        assert!(matches!(
            session::load_checkpoint(&config, "session"),
            Err(session::SessionError::TerminalStop { .. })
        ));
        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "must remain stopped".to_owned().into(),
            })
            .unwrap();
        let WorkerEvent::TurnFinished {
            error: Some(_),
            snapshot: None,
            ..
        } = timeout(Duration::from_secs(5), updates.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("stopped agent must reject followup");
        };
        assert!(receive.try_recv().is_err());
        shutdown.cancel();
        stopped(&mut updates).await;
        drain.await.unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn manual_compaction_is_idle_only_and_shutdown_is_responsive() {
        for close_commands in [true, false] {
            let called = Arc::new(Notify::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let (agent, mut events) = pending_agent(called.clone(), calls.clone());
            let shutdown = CancellationToken::new();
            let (commands, mut updates) = spawn(
                agent,
                TEST_CONTEXT,
                MemoryReviewState::fresh(false),
                shutdown.clone(),
            );
            let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
            commands
                .send(WorkerCommand::Submit {
                    pane: PaneId::Main,
                    id: TurnId::new(1),
                    prompt: "pending".to_owned().into(),
                })
                .unwrap();
            timeout(Duration::from_secs(5), called.notified())
                .await
                .unwrap();
            commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
            assert!(
                compacted(&mut updates)
                    .await
                    .err()
                    .unwrap()
                    .contains("finish active work")
            );
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            commands
                .send(WorkerCommand::CancelAll(PaneId::Main))
                .unwrap();
            finished(&mut updates, TurnId::new(1)).await;
            commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
            timeout(Duration::from_secs(5), called.notified())
                .await
                .unwrap();
            if close_commands {
                drop(commands);
            } else {
                shutdown.cancel();
            }
            stopped(&mut updates).await;
            drain.await.unwrap();
        }
    }

    struct CapturedRequest {
        model: nanocodex::Model,
        thinking: Thinking,
        input: String,
        release: oneshot::Sender<()>,
    }

    #[derive(Clone)]
    struct CaptureService(mpsc::UnboundedSender<CapturedRequest>);

    impl Service<ResponsesAttempt> for CaptureService {
        type Response = ResponsesServiceResponse;
        type Error = ResponseError;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = StdResult<Self::Response, Self::Error>> + Send>,
        >;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ResponsesAttempt) -> Self::Future {
            let (release, released) = oneshot::channel();
            if !matches!(request.kind(), ResponsesAttemptKind::Warmup) {
                self.0
                    .send(CapturedRequest {
                        model: request.model(),
                        thinking: request.thinking(),
                        input: serde_json::to_string(&request.input_items().collect::<Vec<_>>())
                            .unwrap(),
                        release,
                    })
                    .unwrap();
            } else {
                drop(release);
            }
            Box::pin(async move {
                let _ = released.await;
                Err(ResponseError::from(ResponsesServiceError::from(
                    ResponsesError::HttpRejected {
                        status: 400,
                        body: "test request completed".to_owned(),
                        retry_after: None,
                    },
                )))
            })
        }
    }

    fn capture_agent(
        sender: mpsc::UnboundedSender<CapturedRequest>,
        context: AgentContext,
    ) -> (Nanocodex, AgentEvents) {
        let openai = OpenAi::builder("test-key")
            .service(move || CaptureService(sender.clone()))
            .build()
            .unwrap();
        Nanocodex::builder(openai)
            .model(context.model.as_str().parse::<CodexModel>().unwrap())
            .thinking(context.thinking)
            .build()
            .unwrap()
    }

    async fn captured(
        receiver: &mut mpsc::UnboundedReceiver<CapturedRequest>,
        model: Model,
        thinking: Thinking,
    ) -> CapturedRequest {
        let request = timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.model, model.as_str().parse::<CodexModel>().unwrap());
        assert_eq!(request.thinking, thinking);
        let context = request
            .input
            .rsplit_once("<agent_context>")
            .expect("request must contain turn context")
            .1;
        let model_name = crate::app::model::name(model).to_ascii_lowercase();
        assert!(
            context.contains(&format!(
                "This turn runs on {model_name} with {thinking} reasoning effort."
            )),
            "{context}"
        );
        request
    }

    async fn finished(updates: &mut mpsc::UnboundedReceiver<WorkerEvent>, id: TurnId) {
        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(updates.recv().await, Some(WorkerEvent::TurnFinished { id: actual, .. }) if actual == id) { break; }
            }
        }).await.unwrap();
    }

    fn claude_text_frames(text: &str) -> String {
        use serde_json::json;
        [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ].into_iter().map(|frame| format!("event: {}\ndata: {frame}\n\n", frame["type"].as_str().unwrap())).collect::<String>()
    }

    #[tokio::test]
    async fn manual_compaction_captures_claude_native_state_and_shuts_down_pending_work() {
        use axum::{Router, http::StatusCode, routing::post};
        use nanocodex::{Claude, ClaudeModel, claude::ClaudeClient};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (requested, mut requests) =
            mpsc::unbounded_channel::<oneshot::Sender<Option<String>>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route("/v1/messages", post(move || {
            let requested = requested.clone();
            async move {
                let (send, receive) = oneshot::channel();
                requested.send(send).unwrap();
                match receive.await.unwrap() {
                    Some(text) => (StatusCode::OK, [("content-type", "text/event-stream")], claude_text_frames(&text)),
                    None => (StatusCode::BAD_REQUEST, [("content-type", "application/json")], "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"synthetic compaction failure\"}}".to_owned()),
                }
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let client = ClaudeClient::new(
            reqwest::Client::new(),
            format!("http://{address}/v1/messages"),
            "synthetic-key",
        );
        let (agent, mut events) = Nanocodex::builder(Claude::new(
            client,
            Model::Claude(ClaudeModel::Opus55).as_str(),
        ))
        .workspace(directory.path().to_string_lossy())
        .build()
        .unwrap();
        let probe = agent.clone();
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        let seed = agent.prompt("seed conversation").await.unwrap();
        timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap()
            .send(Some("seed answer".to_owned()))
            .unwrap();
        timeout(Duration::from_secs(5), seed)
            .await
            .unwrap()
            .unwrap();
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(
            agent,
            AgentContext {
                model: Model::Claude(ClaudeModel::Opus55),
                thinking: Thinking::Medium,
            },
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
        commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
        timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap()
            .send(Some("compacted native summary".to_owned()))
            .unwrap();
        let snapshot = compacted(&mut updates).await.unwrap();
        assert!(snapshot.context_budget().is_some());
        let expected = serde_json::to_value(&snapshot).unwrap();
        assert!(
            expected["payload"]
                .as_str()
                .unwrap()
                .contains("compacted native summary")
        );
        commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
        timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap()
            .send(None)
            .unwrap();
        assert!(
            compacted(&mut updates)
                .await
                .err()
                .unwrap()
                .contains("synthetic compaction failure")
        );
        let actual =
            super::AgentSnapshot::from_claude(probe.runtime_snapshot().await.unwrap()).unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
        drop(probe);
        commands.send(WorkerCommand::Compact(PaneId::Main)).unwrap();
        let pending = timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        shutdown.cancel();
        stopped(&mut updates).await;
        drain.await.unwrap();
        server.abort();
        let _ = server.await;
        drop(pending);
    }

    #[tokio::test]
    async fn claude_success_publishes_a_native_checkpoint_and_rejects_fork() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        use axum::{Router, routing::post};
        use nanocodex::{
            Claude, ClaudeModel, HarnessModel, agent::ChildSnapshot, claude::ClaudeClient,
        };
        let frames = claude_text_frames("done");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (requested, mut requests) = mpsc::unbounded_channel();
        let router = Router::new().route(
            "/v1/messages",
            post(move || {
                let frames = frames.clone();
                let index = calls.fetch_add(1, Ordering::Relaxed);
                let requested = requested.clone();
                async move {
                    requested.send(()).unwrap();
                    if index > 0 {
                        pending::<()>().await;
                    }
                    ([("content-type", "text/event-stream")], frames)
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let client = ClaudeClient::new(
            reqwest::Client::new(),
            format!("http://{address}/v1/messages"),
            "synthetic-key",
        );
        let (agent, mut events) = Nanocodex::builder(Claude::new(
            client,
            Model::Claude(ClaudeModel::Opus55).as_str(),
        ))
        .workspace(directory.path().to_string_lossy())
        .thinking(Thinking::High)
        .unwrap()
        .build()
        .unwrap();
        let expected_session = agent.session_id().to_owned();
        let probe = agent.clone();
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(
            agent,
            AgentContext {
                model: Model::Claude(ClaudeModel::Opus55),
                thinking: Thinking::High,
            },
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        match timeout(Duration::from_secs(5), updates.recv())
            .await
            .unwrap()
            .unwrap()
        {
            WorkerEvent::ContextBudget {
                pane,
                session_id,
                budget,
            } => {
                assert_eq!(pane, PaneId::Main);
                assert_eq!(session_id, expected_session);
                assert_eq!(budget.active_tokens, 0);
                assert_eq!(budget.window_tokens, 1_000_000);
            }
            _ => panic!("expected native context before the first prompt"),
        }
        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(1),
                prompt: "hello".to_owned().into(),
            })
            .unwrap();
        let snapshot = timeout(Duration::from_secs(5), async {
            let mut latest = None;
            loop {
                match updates.recv().await.unwrap() {
                    WorkerEvent::ContextBudget {
                        session_id, budget, ..
                    } => {
                        assert_eq!(session_id, expected_session);
                        latest = Some(budget);
                    }
                    WorkerEvent::TurnFinished {
                        snapshot, error, ..
                    } => {
                        assert!(error.is_none(), "{error:?}");
                        let snapshot = snapshot.expect("successful Claude turn must checkpoint");
                        assert_eq!(latest, snapshot.context_budget());
                        assert_eq!(latest.unwrap().active_tokens, 11);
                        break snapshot;
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let ChildSnapshot::Native {
            model,
            session_id,
            thinking,
            has_conversation,
            payload,
        } = snapshot.into_claude().unwrap()
        else {
            panic!("expected native checkpoint")
        };
        assert_eq!(model, HarnessModel::Claude(ClaudeModel::Opus55));
        assert_eq!(session_id, expected_session);
        assert_eq!(thinking, Thinking::High);
        assert!(has_conversation);
        assert!(payload.contains("hello"));
        assert!(payload.contains("done"));
        commands
            .send(WorkerCommand::OpenFork {
                pane: PaneId::Fork(1),
                parent_sequence: 1,
            })
            .unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(WorkerEvent::ForkFailed { error, .. }) = updates.recv().await {
                    assert!(error.contains("Claude does not support forking"));
                    break;
                }
            }
        })
        .await
        .unwrap();
        requests.recv().await.unwrap();
        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(2),
                prompt: "cancel this turn".to_owned().into(),
            })
            .unwrap();
        timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        commands
            .send(WorkerCommand::CancelAll(PaneId::Main))
            .unwrap();
        let budget = timeout(Duration::from_secs(5), async {
            let mut latest = None;
            loop {
                match updates.recv().await.unwrap() {
                    WorkerEvent::ContextBudget { budget, .. } => latest = Some(budget),
                    WorkerEvent::TurnFinished {
                        id,
                        snapshot,
                        error,
                        ..
                    } if id == TurnId::new(2) => {
                        assert!(snapshot.is_none());
                        assert!(error.is_none());
                        break latest.expect("cancelled turn must publish current context");
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let snapshot = crate::tui::session::AgentSnapshot::from_claude(
            probe.runtime_snapshot().await.unwrap(),
        )
        .unwrap();
        assert_eq!(Some(budget), snapshot.context_budget());
        drop(probe);
        shutdown.cancel();
        drain.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn context_matches_queued_turn_effort_and_changed_auxiliary_settings() {
        let (sender, mut requests) = mpsc::unbounded_channel();
        let context = TEST_CONTEXT;
        let (agent, mut events) = capture_agent(sender.clone(), context);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(
            agent,
            context,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
        let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
        for id in [1, 2] {
            commands
                .send(WorkerCommand::Submit {
                    pane: PaneId::Main,
                    id: TurnId::new(id),
                    prompt: format!("request {id}").into(),
                })
                .unwrap();
        }
        let first = captured(
            &mut requests,
            Model::Codex(CodexModel::Astra),
            Thinking::Low,
        )
        .await;
        commands
            .send(WorkerCommand::SetThinking {
                pane: PaneId::Main,
                effort: ReasoningEffort::High,
            })
            .unwrap();
        loop {
            if let Some(WorkerEvent::ThinkingUpdated { result, .. }) = updates.recv().await {
                result.unwrap();
                break;
            }
        }
        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Main,
                id: TurnId::new(3),
                prompt: "new effort".to_owned().into(),
            })
            .unwrap();
        first.release.send(()).unwrap();
        captured(
            &mut requests,
            Model::Codex(CodexModel::Astra),
            Thinking::Low,
        )
        .await
        .release
        .send(())
        .unwrap();
        captured(
            &mut requests,
            Model::Codex(CodexModel::Astra),
            Thinking::High,
        )
        .await
        .release
        .send(())
        .unwrap();
        finished(&mut updates, TurnId::new(3)).await;

        for context in [
            super::AuxiliaryContext::Clean,
            super::AuxiliaryContext::CurrentConversation,
        ] {
            let (completion, result) = oneshot::channel();
            commands
                .send(WorkerCommand::Auxiliary {
                    pane: PaneId::Main,
                    id: TurnId::new(4),
                    prompt: "auxiliary".to_owned().into(),
                    context,
                    shutdown: CancellationToken::new(),
                    completion,
                })
                .unwrap();
            captured(
                &mut requests,
                Model::Codex(CodexModel::Astra),
                Thinking::High,
            )
            .await
            .release
            .send(())
            .unwrap();
            assert!(
                timeout(Duration::from_secs(5), result)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        }
        commands
            .send(WorkerCommand::OpenFork {
                pane: PaneId::Fork(1),
                parent_sequence: 0,
            })
            .unwrap();
        let mut fork_events = loop {
            match updates.recv().await.unwrap() {
                WorkerEvent::ForkOpened { events, .. } => break events,
                WorkerEvent::ForkFailed { error, .. } => panic!("{error}"),
                _ => {}
            }
        };
        let fork_drain = tokio::spawn(async move { while fork_events.recv().await.is_some() {} });
        commands
            .send(WorkerCommand::Submit {
                pane: PaneId::Fork(1),
                id: TurnId::new(5),
                prompt: "fork".to_owned().into(),
            })
            .unwrap();
        captured(
            &mut requests,
            Model::Codex(CodexModel::Astra),
            Thinking::High,
        )
        .await
        .release
        .send(())
        .unwrap();
        finished(&mut updates, TurnId::new(5)).await;

        let replacement_context = AgentContext {
            model: Model::Codex(CodexModel::Sol),
            thinking: Thinking::Medium,
        };
        let (replacement, mut replacement_events) = capture_agent(sender, replacement_context);
        let replacement_drain =
            tokio::spawn(async move { while replacement_events.recv().await.is_some() {} });
        commands
            .send(WorkerCommand::ReplaceAgent {
                pane: PaneId::Main,
                agent: replacement,
                context: replacement_context,
                memory_review: MemoryReviewState::restored(false),
            })
            .unwrap();
        commands
            .send(WorkerCommand::Steer {
                pane: PaneId::Main,
                queue_id: QueueId::new(1),
                fallback_id: TurnId::new(6),
                prompt: "replacement fallback".to_owned().into(),
            })
            .unwrap();
        captured(
            &mut requests,
            Model::Codex(CodexModel::Sol),
            Thinking::Medium,
        )
        .await
        .release
        .send(())
        .unwrap();
        finished(&mut updates, TurnId::new(6)).await;
        shutdown.cancel();
        for task in [drain, fork_drain, replacement_drain] {
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[test]
    fn terminal_stop_classification_uses_the_typed_provider_code() {
        let violation = r#"{"error":{"code":"misalignment_policy_violation","message":"conversation stopped"}}"#;
        let ordinary =
            r#"{"error":{"code":"server_error","message":"misalignment_policy_violation"}}"#;
        let cases = [
            (
                ResponsesError::Api {
                    event: violation.to_owned(),
                },
                true,
            ),
            (
                ResponsesError::HttpRejected {
                    status: 403,
                    body: violation.to_owned(),
                    retry_after: None,
                },
                true,
            ),
            (
                ResponsesError::Api {
                    event: ordinary.to_owned(),
                },
                false,
            ),
            (ResponsesError::UnexpectedEnd, false),
        ];
        for (source, stopped) in cases {
            let error =
                NanocodexError::Response(ResponseError::from(ResponsesServiceError::from(source)));
            let (updates, mut received) = mpsc::unbounded_channel();
            finish_turn(
                Some(Ok((
                    TurnKey {
                        pane: PaneId::Main,
                        id: TurnId::new(1),
                    },
                    TurnPurpose::Conversation,
                    false,
                    Err(error),
                ))),
                false,
                &mut HashMap::new(),
                &mut HashSet::new(),
                &updates,
            );
            let WorkerEvent::TurnFinished {
                error,
                terminal_stop,
                snapshot,
                ..
            } = received.try_recv().unwrap()
            else {
                panic!("expected terminal worker event");
            };
            assert!(error.is_some());
            assert!(snapshot.is_none());
            assert_eq!(
                terminal_stop,
                stopped.then_some(TerminalStopReason::MisalignmentPolicyViolation)
            );
        }
    }

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

    const TEST_CONTEXT: AgentContext = AgentContext {
        model: Model::Codex(CodexModel::Astra),
        thinking: Thinking::Low,
    };

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

        let initial_prompt = prompt_text(review.submission_prompt(&initial));
        assert!(!initial_prompt.contains(MEMORY_REVIEW_CHECKPOINT));
        assert!(initial_prompt.contains(IMAGE_RENDERING_INSTRUCTIONS));
        review.turn_accepted();
        let follow_up_prompt = prompt_text(review.submission_prompt(&follow_up));
        assert_eq!(
            follow_up_prompt.matches(MEMORY_REVIEW_CHECKPOINT).count(),
            1
        );
        assert!(follow_up_prompt.contains(IMAGE_RENDERING_INSTRUCTIONS));
        let steer_prompt = prompt_text(MemoryReviewState::fresh(true).steer_prompt(&steer));
        assert_eq!(steer_prompt.matches(MEMORY_REVIEW_CHECKPOINT).count(), 1);
        assert!(steer_prompt.contains(IMAGE_RENDERING_INSTRUCTIONS));
        assert_eq!(follow_up.display_text(), "actually, preserve ordering");
        assert_eq!(steer.display_text(), "change direction");

        let disabled = MemoryReviewState::fresh(false);
        let disabled_prompt = prompt_text(disabled.steer_prompt(&steer));
        assert!(!disabled_prompt.contains(MEMORY_REVIEW_CHECKPOINT));
        assert!(disabled_prompt.contains(IMAGE_RENDERING_INSTRUCTIONS));
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
        assert!(text.contains("`parent_session_id`"));
        assert!(text.contains("`user.submitted` and `user.steered`"));
        assert!(text.contains("unless the relevant scope was inspected exhaustively"));
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
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
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
    async fn speed_can_change_while_a_turn_is_active() {
        let called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (agent, mut events) = pending_agent(Arc::clone(&called), calls);
        let shutdown = CancellationToken::new();
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
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
            .send(WorkerCommand::SetSpeed {
                pane: PaneId::Main,
                speed: Speed::Ultrafast,
            })
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(5), updates.recv()).await,
            Ok(Some(WorkerEvent::SpeedUpdated {
                pane: PaneId::Main,
                speed: Speed::Ultrafast,
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
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
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
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
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
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
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
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
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
            TEST_CONTEXT,
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
            TEST_CONTEXT,
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
        let (commands, mut updates) = spawn(
            agent,
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );
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
            TEST_CONTEXT,
            MemoryReviewState::fresh(false),
            shutdown.clone(),
        );

        commands
            .send(WorkerCommand::ReplaceAgent {
                pane: PaneId::Main,
                agent: second_agent,
                context: TEST_CONTEXT,
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
