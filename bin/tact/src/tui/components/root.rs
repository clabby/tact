//! The component for one session pane.
//!
//! [`RootNode`] owns everything a pane shows: the transcript, the composer, the message queue,
//! the open overlay, and the bookkeeping for turns, steers, and blocking tasks such as compaction.
//! Terminal events and session events arrive through [`RootEvent`]; work that leaves the pane
//! (submitting prompts, cancelling turns, loading sessions) is returned as [`RootEffect`] for the
//! event loop to execute. Web commands enter through [`RootNode::remote_command`] and share the
//! keyboard's preconditions and effects.

mod input;
mod remote;
mod render;
mod turns;

use super::{
    actions::{Action, ActionAvailability, ActionsEffect, ActionsEvent, ActionsMenu},
    clock::unix_time_ms,
    composer::{
        Composer, ComposerChromeTarget, ComposerDraft, ComposerEffect, ComposerEvent, InputMode,
        LiveSessions,
    },
    confirmation::{Confirmation, ConfirmationEffect, ConfirmationEvent},
    context_diagnostics::{
        ContextDiagnosticsEffect, ContextDiagnosticsEvent, ContextDiagnosticsPanel,
    },
    effort::{EffortEffect, EffortEvent, EffortSelector},
    file_finder::{FileFinder, FileFinderEffect, FileFinderEvent},
    keybindings::{KeybindingsEffect, KeybindingsEvent, KeybindingsHelp},
    memory::{MemoryBrowser, MemoryBrowserEffect, MemoryBrowserEvent},
    model_selector::{ModelSelector, ModelSelectorEffect, ModelSelectorEvent},
    node::{Component, ComponentUpdate, RenderRequest},
    qr_code::{QrCodeEffect, QrCodeEvent, QrCodeView},
    queue::{MessageQueue, QueueEffect, QueueEntry, QueueEvent},
    recent_prompt_picker::{RecentPromptPicker, RecentPromptPickerEffect, RecentPromptPickerEvent},
    selection::{Selection, Surface, TextSpan},
    session_picker::{SessionPicker, SessionPickerEffect, SessionPickerEvent, SessionPickerMode},
    skill_picker::{SkillPicker, SkillPickerEffect, SkillPickerEvent},
    speed::{SpeedEffect, SpeedEvent, SpeedSelector},
    subagents::{SubagentEffect, SubagentOverlay, SubagentTree},
    theme_selector::{ThemeSelector, ThemeSelectorEffect, ThemeSelectorEvent},
    transcript::{ScrollCommand, Transcript, TranscriptEvent},
};
use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode, Speed, TuiConfig},
        model,
        theme::{Theme, ThemeMode},
    },
    core::{
        context::{ContextBudget, ContextDiagnostics},
        extensions::Skill,
        prompt::{QueueId, Submission},
        protocol::{Busy, CommandError},
        session::{RecentPrompt, SessionSummary},
        transcript::{LocalKind, TranscriptRecord},
    },
};
use crossterm::event::{Event, MouseButton, MouseEventKind};
use input::{
    is_actions_trigger, is_confirmation_key_repeat, is_control_c, is_control_key, is_escape,
    is_file_finder_trigger, is_file_query_character, is_focus_toggle, is_key_release,
    is_left_click, is_left_click_in, is_mention_edit, is_picker_navigation, is_plain_enter,
    is_queue_shortcut, is_skill_picker_trigger, is_skill_query_character,
    mention_edit_continues_query,
};
use nanocodex::{HarnessModel as Model, agent::events::AgentEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
pub(crate) use remote::PaneCommand;
use semver::Version;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord, MemorySource};
use tact_subagents::{AgentId, AgentStatus, AgentUpdate, MessageSender, SubagentRoster};
use turns::{TurnEnd, TurnLedger};

const KEY_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(2);
const SELECTION_SCROLL_INTERVAL: Duration = Duration::from_millis(60);
const BREADCRUMB_DURATION: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfirmationAction {
    Interrupt,
    Exit,
}

impl ConfirmationAction {
    const fn title_key(self) -> &'static str {
        match self {
            Self::Interrupt => "Esc",
            Self::Exit => "Ctrl+C",
        }
    }

    const fn action_label(self) -> &'static str {
        match self {
            Self::Interrupt => "Interrupt",
            Self::Exit => "Quit",
        }
    }

    const fn effect(self) -> RootEffect {
        match self {
            Self::Interrupt => RootEffect::CancelTurns,
            Self::Exit => RootEffect::Shutdown,
        }
    }
}

struct KeyConfirmation {
    action: ConfirmationAction,
    deadline: Instant,
}

struct Notification {
    message: Line<'static>,
    color: Color,
    deadline: Instant,
}

struct SelectionAutoScroll {
    direction: isize,
    position: Position,
    deadline: Instant,
}

impl Notification {
    fn plain(message: String, color: Color) -> Self {
        Self {
            message: Line::styled(
                message,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            color,
            deadline: Instant::now() + BREADCRUMB_DURATION,
        }
    }

    fn update_available(version: Version) -> Self {
        let green = Style::default().fg(Color::Green);
        Self {
            message: Line::from(vec![
                Span::styled("Update available · ", green),
                Span::styled(format!("v{version}"), green.add_modifier(Modifier::BOLD)),
                Span::styled(" · run ", green),
                Span::styled("`tact update`", Style::default().fg(Color::Reset)),
            ]),
            color: Color::Green,
            deadline: Instant::now() + BREADCRUMB_DURATION,
        }
    }
}

pub(crate) enum RootEvent {
    Terminal(Event),
    PasteImage(String),
    ContextBudget(ContextBudget),
    Transcript(Arc<TranscriptRecord>),
    AgentStreamClosed,
    Subagent(AgentUpdate),
    /// A subagent transcript record, built once so every front-end shares it.
    SubagentRecord {
        id: AgentId,
        record: Arc<TranscriptRecord>,
    },
    ReplaceDraft(String),
    HandoffFinished(String),
    HandoffCancelled,
    HandoffFailed(String),
    CompactionFinished,
    /// The worker finished a turn. When `terminal_expected` is set, the agent stream also reports
    /// the end of the turn with a terminal transcript record, which may arrive before or after this
    /// event; the turn counts as finished once both have arrived.
    WorkerTurnFinished {
        terminal_expected: bool,
    },
    ShellFinished,
    TurnsCancelled,
    ForkReady,
    NewSessionFailed(String),
    SessionsLoaded(Vec<SessionSummary>),
    RecentPromptsLoaded {
        session_id: String,
        prompts: Vec<RecentPrompt>,
    },
    RecentPromptLoadFailed(String),
    SessionLoadFailed(String),
    MemoriesLoaded {
        access: MemoryAccess,
        records: Vec<MemoryRecord>,
    },
    MemoryLoadFailed {
        source: MemorySource,
        access: Option<MemoryAccess>,
        error: String,
    },
    MemoryDeleted {
        key: MemoryKey,
    },
    MemoryDeleteFailed {
        error: String,
        conflict: bool,
    },
    SessionRestored {
        projection: Box<RestoredSessionProjection>,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        preferred_reasoning_mode: ReasoningMode,
        speed: Speed,
        model: Model,
        skills: Arc<[Skill]>,
    },
    EffortUpdateFailed {
        effort: ReasoningEffort,
        error: String,
    },
    NotifyError(String),
    NotifySuccess(String),
    /// Shows the QR code for a web sign-in link.
    ShowQrCode(String),
    ConfirmWebInstall,
    UpdateAvailable(Version),
    SteerAdmitted(QueueId),
    SteerPromoted(QueueId),
    SteerFailed {
        id: QueueId,
    },
    AnimationFrame(Instant),
}

pub(crate) struct RestoredSessionProjection {
    transcript: Transcript,
    context_diagnostics: ContextDiagnostics,
    context_tokens: Option<u64>,
    recent_prompts: Vec<RecentPromptDraft>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecentPromptDraft {
    pub(crate) text: String,
    pub(crate) recorded_at_unix_ms: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SessionListKind {
    Resume,
    Mention,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RootEffect {
    Submit(Submission),
    Reflect(Submission),
    Compact,
    RunShell(String),
    ContinueSubagent(Submission),
    OpenDraftEditor,
    OpenConfigEditor,
    OpenLink(String),
    ReloadConfig,
    NewSession(Model),
    LoadSessions(SessionListKind),
    LoadRecentPrompts(Vec<RecentPromptDraft>),
    LoadMemories,
    DeleteMemory(MemoryKey),
    ResumeSession(String),
    Steer {
        id: QueueId,
        prompt: Submission,
    },
    PersistSteer(String),
    Copy(String),
    Handoff,
    /// Opens the web interface, installing its assets first when `install` is set.
    OpenWebInterface {
        install: bool,
    },
    CopyWebLink,
    /// Shows the web sign-in link as a QR code for a phone to scan.
    ShowWebQr,
    OpenSessions,
    SetEffort {
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
    },
    SetModel(Model),
    SetSpeed(Speed),
    SetMaxSubagents(usize),
    SetTheme(ThemeMode),
    Fork,
    CancelTurns,
    CancelHandoff,
    Shutdown,
}

/// The overlay that has input focus; at most one is open, and opening another replaces it. Once
/// pane-wide handling such as the Ctrl+C confirmation has run, terminal events go to the overlay
/// instead of the transcript, queue, or composer.
enum Overlay {
    Actions(ActionsMenu),
    ContextDiagnostics(ContextDiagnosticsPanel),
    Effort(EffortSelector),
    Speed(SpeedSelector),
    Model(ModelSelector),
    Theme(ThemeSelector),
    FileFinder(FileMention),
    Skills(SkillMention),
    Keybindings(KeybindingsHelp),
    QrCode(QrCodeView),
    Memory(MemoryBrowser),
    RecentPrompts(RecentPromptPicker),
    Sessions(SessionPicker),
    WebInstall(Confirmation),
    Subagents(SubagentOverlay),
}

/// Pane-wide work that suspends normal input until it finishes. No turn, shell command, or queued
/// prompt may run alongside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockingTask {
    Compaction,
    Handoff,
}

struct FileMention {
    finder: FileFinder,
    start: usize,
}

struct SkillMention {
    picker: SkillPicker,
    start: usize,
}

struct QueueEdit {
    id: QueueId,
    original_draft: Option<ComposerDraft>,
    original_input_mode: InputMode,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ThreadState {
    New,
    Started,
}

/// How a prompt submitted while a turn is running reaches the agent.
#[derive(Clone, Copy, Eq, PartialEq)]
enum BusyDelivery {
    /// Joins the running turn at its next opportunity.
    Steer,
    /// Waits in the queue until the turn finishes.
    Queue,
}

#[derive(Clone, Copy)]
pub(crate) enum DraftReset {
    Clear,
    Preserve,
}

/// The state and routing of one session pane. See the module documentation.
pub(crate) struct RootNode {
    transcript: Transcript,
    composer: Composer,
    queue: MessageQueue,
    workspace: PathBuf,
    overlay: Option<Overlay>,
    thread: ThreadState,
    key_confirmation: Option<KeyConfirmation>,
    notification: Option<Notification>,
    discarded_draft: Option<ComposerDraft>,
    queue_edit: Option<QueueEdit>,
    selection: Selection,
    selection_auto_scroll: Option<SelectionAutoScroll>,
    transcript_area: Rect,
    composer_area: Rect,
    composer_content_area: Rect,
    queue_area: Rect,
    turns: TurnLedger,
    blocking_task: Option<BlockingTask>,
    fork_available: bool,
    skills: Arc<[Skill]>,
    memory_enabled: bool,
    claude_enabled: bool,
    interactive: bool,
    theme_mode: ThemeMode,
    tui: TuiConfig,
    preferred_reasoning_mode: ReasoningMode,
    subagents: SubagentTree,
    context_diagnostics: ContextDiagnostics,
    recent_prompts: Vec<RecentPromptDraft>,
    /// The first prompt of the conversation, inherited by forks.
    title: Option<String>,
    pending_session_mention: Option<usize>,
    reflection_input: bool,
}

impl RootNode {
    pub(crate) fn new(workspace: &Path, thinking: ReasoningEffort) -> Self {
        let mut transcript = Transcript::with_effort(thinking);
        transcript.set_workspace(workspace);
        let mut subagents = SubagentTree::new(thinking);
        subagents.set_workspace(workspace);
        Self {
            transcript,
            composer: Composer::new(workspace, thinking),
            queue: MessageQueue::default(),
            workspace: workspace.to_path_buf(),
            overlay: None,
            thread: ThreadState::New,
            key_confirmation: None,
            notification: None,
            discarded_draft: None,
            queue_edit: None,
            selection: Selection::default(),
            selection_auto_scroll: None,
            transcript_area: Rect::default(),
            composer_area: Rect::default(),
            composer_content_area: Rect::default(),
            queue_area: Rect::default(),
            turns: TurnLedger::default(),
            blocking_task: None,
            fork_available: true,
            skills: Arc::from([]),
            memory_enabled: false,
            claude_enabled: false,
            interactive: true,
            theme_mode: ThemeMode::Auto,
            tui: TuiConfig::default(),
            preferred_reasoning_mode: ReasoningMode::Standard,
            subagents,
            context_diagnostics: ContextDiagnostics::default(),
            recent_prompts: Vec::new(),
            title: None,
            pending_session_mention: None,
            reflection_input: false,
        }
    }

    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(super) fn set_workspace(&mut self, workspace: PathBuf) {
        self.transcript.set_workspace(&workspace);
        self.subagents.set_workspace(&workspace);
        self.composer.set_workspace(&workspace);
        self.workspace = workspace;
    }

    pub(crate) fn fork(&self, workspace: &Path, thinking: ReasoningEffort) -> Self {
        let mut root = Self::new(workspace, thinking);
        root.transcript = self.transcript.fork_snapshot();
        root.composer
            .update(ComposerEvent::ContextBudget(ContextBudget {
                active_tokens: self.composer.context_tokens(),
                window_tokens: self.context_diagnostics.model_window_tokens,
            }));
        root.set_speed(self.composer.speed());
        root.set_model(self.composer.model());
        root.set_reasoning_modes(
            self.composer.reasoning_mode(),
            self.preferred_reasoning_mode,
        );
        root.set_max_subagents(self.subagents.max_subagents());
        root.thread = ThreadState::Started;
        root.fork_available = false;
        root.set_skills(Arc::clone(&self.skills));
        root.memory_enabled = self.memory_enabled;
        root.claude_enabled = self.claude_enabled;
        root.theme_mode = self.theme_mode;
        root.tui = self.tui;
        root.context_diagnostics = self.context_diagnostics.clone();
        root.title.clone_from(&self.title);
        root.interactive = false;
        root.composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Forking session…".to_owned()),
            now: Instant::now(),
        });
        root
    }

    /// A pane for an unrelated session that shares this pane's process-wide preferences.
    pub(crate) fn sibling(&self, workspace: &Path) -> Self {
        let mut root = self.sibling_with_effort(workspace, self.composer.effort());
        root.set_reasoning_modes(
            self.composer.reasoning_mode(),
            self.preferred_reasoning_mode,
        );
        root
    }

    fn sibling_with_effort(&self, workspace: &Path, thinking: ReasoningEffort) -> Self {
        let mut root = Self::new(workspace, thinking);
        root.memory_enabled = self.memory_enabled;
        root.claude_enabled = self.claude_enabled;
        root.theme_mode = self.theme_mode;
        root.tui = self.tui;
        root.set_max_subagents(self.subagents.max_subagents());
        root
    }

    /// Blocks input and shows `status` until the pane's session is installed.
    pub(crate) fn begin_opening(&mut self, status: &str) {
        self.interactive = false;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: true,
            status: Some(status.to_owned()),
            now: Instant::now(),
        });
    }

    pub(crate) fn set_fork_available(&mut self, available: bool) {
        self.fork_available = available;
        let can_fork = self.can_fork();
        if let Some(Overlay::Actions(actions)) = &mut self.overlay {
            actions.set_fork_available(can_fork);
        }
    }

    pub(crate) fn set_skills(&mut self, skills: Arc<[Skill]>) {
        self.skills = skills;
        if self.skills.is_empty() && matches!(&self.overlay, Some(Overlay::Skills(_))) {
            self.overlay = None;
        }
    }

    pub(crate) fn set_memory_enabled(&mut self, enabled: bool) {
        self.memory_enabled = enabled;
        if !enabled && matches!(&self.overlay, Some(Overlay::Memory(_))) {
            self.overlay = None;
        }
    }

    pub(crate) fn set_claude_enabled(&mut self, enabled: bool) {
        self.claude_enabled = enabled;
        if matches!(self.overlay, Some(Overlay::Model(_))) {
            self.open_model();
        }
    }

    pub(crate) fn set_tui_config(&mut self, tui: TuiConfig) {
        self.tui = tui;
    }

    pub(crate) fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.theme_mode = mode;
    }

    pub(crate) fn set_speed(&mut self, speed: Speed) {
        let _ = self.composer.update(ComposerEvent::SetSpeed(speed));
    }

    pub(crate) fn set_model(&mut self, model: Model) {
        let _ = self.composer.update(ComposerEvent::SetModel(model));
    }

    pub(crate) fn set_reasoning_modes(&mut self, actual: ReasoningMode, preferred: ReasoningMode) {
        self.preferred_reasoning_mode = preferred;
        let _ = self
            .composer
            .update(ComposerEvent::SetReasoningMode(actual));
    }

    pub(crate) const fn set_preferred_reasoning_mode(&mut self, mode: ReasoningMode) {
        self.preferred_reasoning_mode = mode;
    }

    pub(crate) const fn preferred_reasoning_mode(&self) -> ReasoningMode {
        self.preferred_reasoning_mode
    }

    pub(crate) fn set_max_subagents(&mut self, limit: usize) {
        self.subagents.set_max_subagents(limit);
    }

    pub(crate) fn reset_session(
        &mut self,
        workspace: &Path,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        preferred_reasoning_mode: ReasoningMode,
        draft_reset: DraftReset,
    ) {
        let current_draft = self.composer.take_draft();
        let previous_discarded_draft = self.discarded_draft.take();
        let replaced_draft = current_draft.is_some() && matches!(draft_reset, DraftReset::Clear);
        let (preserved_draft, discarded_draft) = match draft_reset {
            DraftReset::Clear => (None, current_draft.or(previous_discarded_draft)),
            DraftReset::Preserve => (current_draft, previous_discarded_draft),
        };
        let fork_available = self.fork_available;
        *self = self.sibling_with_effort(workspace, thinking);
        self.set_reasoning_modes(reasoning_mode, preferred_reasoning_mode);
        self.discarded_draft = discarded_draft;
        self.fork_available = fork_available;
        if let Some(draft) = preserved_draft {
            self.composer.restore_draft(draft);
        }
        if replaced_draft {
            self.show_draft_saved();
        }
    }

    #[allow(dead_code, reason = "used by restoration benchmarks and focused tests")]
    pub(crate) fn restore_session(
        &mut self,
        workspace: &Path,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        preferred_reasoning_mode: ReasoningMode,
        speed: Speed,
        records: Vec<Arc<TranscriptRecord>>,
    ) {
        let projection = Self::project_session(thinking, records);
        self.install_session_projection(
            workspace,
            thinking,
            reasoning_mode,
            preferred_reasoning_mode,
            speed,
            projection,
        );
    }

    pub(crate) fn project_session(
        thinking: ReasoningEffort,
        records: Vec<Arc<TranscriptRecord>>,
    ) -> RestoredSessionProjection {
        let mut transcript = Transcript::with_effort(thinking);
        let mut context_diagnostics = ContextDiagnostics::default();
        let mut context_tokens = None;
        let mut recent_prompts = Vec::new();
        for record in records {
            if let Some(prompt) = recent_prompt(&record) {
                recent_prompts.push(prompt);
            }
            let observation = context_diagnostics.observe(&record);
            if observation.completed_tokens.is_some() {
                context_tokens = observation.completed_tokens;
            }
            let _ = transcript.update(TranscriptEvent::Record(record));
        }
        let _ = transcript.update(TranscriptEvent::AgentStreamClosed);
        RestoredSessionProjection {
            transcript,
            context_diagnostics,
            context_tokens,
            recent_prompts,
        }
    }

    pub(crate) fn install_session_projection(
        &mut self,
        workspace: &Path,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        preferred_reasoning_mode: ReasoningMode,
        speed: Speed,
        mut projection: RestoredSessionProjection,
    ) {
        self.reset_session(
            workspace,
            thinking,
            reasoning_mode,
            preferred_reasoning_mode,
            DraftReset::Clear,
        );
        self.set_speed(speed);
        projection.transcript.set_workspace(workspace);
        self.transcript = projection.transcript;
        self.context_diagnostics = projection.context_diagnostics;
        self.recent_prompts = projection.recent_prompts;
        self.title = self
            .recent_prompts
            .first()
            .map(|prompt| prompt.text.clone());
        if let Some(tokens) = projection.context_tokens {
            let _ = self
                .composer
                .update(ComposerEvent::ContextBudget(ContextBudget {
                    active_tokens: tokens,
                    window_tokens: self.context_diagnostics.model_window_tokens,
                }));
        }
        self.thread = ThreadState::Started;
    }

    pub(crate) const fn composer(&self) -> &Composer {
        &self.composer
    }

    pub(crate) const fn skills(&self) -> &Arc<[Skill]> {
        &self.skills
    }

    pub(crate) const fn context_diagnostics(&self) -> &ContextDiagnostics {
        &self.context_diagnostics
    }

    /// Prompts submitted in this session, oldest first, for the recent-prompt picker.
    pub(crate) fn recent_prompts(&self) -> &[RecentPromptDraft] {
        &self.recent_prompts
    }

    pub(crate) const fn subagent_roster(&self) -> &SubagentRoster {
        self.subagents.roster()
    }

    pub(crate) fn render_focused(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        focused: bool,
    ) {
        self.render_root(frame, area, theme, focused);
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        let selector = match &self.overlay {
            Some(Overlay::Effort(selector)) => selector.animation_deadline(),
            Some(Overlay::Speed(selector)) => selector.animation_deadline(),
            _ => None,
        };
        [
            selector,
            self.transcript.animation_deadline(),
            self.composer.animation_deadline(),
            self.queue.animation_deadline(),
            self.key_confirmation
                .as_ref()
                .map(|confirmation| confirmation.deadline),
            self.notification.as_ref().map(|notice| notice.deadline),
            self.selection_auto_scroll
                .as_ref()
                .map(|scroll| scroll.deadline),
            self.subagents.animation_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn update_terminal(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        if matches!(event, Event::Resize(_, _)) {
            self.selection.clear();
            self.selection_auto_scroll = None;
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if is_confirmation_key_repeat(&event) {
            return ComponentUpdate::none();
        }
        if self.reflection_input && is_escape(&event) {
            return self.cancel_reflection();
        }
        if self.blocking_task.is_some() && is_control_c(&event) {
            return self.update_key_confirmation(ConfirmationAction::Exit, Instant::now());
        }
        match self.blocking_task {
            Some(BlockingTask::Compaction) => return ComponentUpdate::none(),
            Some(BlockingTask::Handoff) => return self.update_handoff_input(event),
            None => {}
        }
        if is_control_c(&event) {
            if self.overlay.is_none()
                && !self.queue.focused()
                && !self.transcript.expandables_focused()
                && !self.composer.draft().is_empty()
            {
                self.key_confirmation = None;
                return self.discard_draft();
            }
            return self.update_key_confirmation(ConfirmationAction::Exit, Instant::now());
        }
        if is_escape(&event)
            && self
                .key_confirmation
                .as_ref()
                .is_some_and(|confirmation| confirmation.action == ConfirmationAction::Exit)
        {
            self.key_confirmation = None;
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        let confirmation_cleared =
            !is_escape(&event) && !is_key_release(&event) && self.key_confirmation.take().is_some();
        let mut update = self.update_terminal_without_confirmation(event);
        if confirmation_cleared {
            update.render = update.render.max(RenderRequest::Immediate);
        }
        update
    }

    pub(crate) fn refresh_terminal_images(&mut self) {
        self.transcript.refresh_terminal_images();
        self.subagents.refresh_terminal_images();
    }

    fn update_terminal_without_confirmation(
        &mut self,
        mut event: Event,
    ) -> ComponentUpdate<RootEffect> {
        if !self.interactive {
            return ComponentUpdate::none();
        }
        if self.queue_edit.is_some() {
            return self.update_queue_editor(event);
        }
        if self.reflection_input && is_plain_enter(&event) {
            return self.submit_reflection();
        }
        if let Some(Overlay::Subagents(SubagentOverlay::Transcript(id))) = self.overlay
            && is_control_key(&event, 'o')
        {
            let render = if self.subagents.toggle_expand_all(id) {
                RenderRequest::Immediate
            } else {
                RenderRequest::None
            };
            return ComponentUpdate::render(render);
        }
        if self.overlay.is_some() {
            return self.update_overlay(event, Instant::now());
        }
        if is_control_key(&event, 'z')
            && !self.queue.focused()
            && !self.transcript.expandables_focused()
        {
            return self.restore_discarded_draft();
        }
        if is_control_key(&event, 'o') {
            return self.update_transcript(TranscriptEvent::ToggleExpandAll);
        }
        if is_control_key(&event, 's') {
            return self.open_effort();
        }
        if is_control_key(&event, 'd') {
            return self.open_model();
        }
        if is_control_key(&event, 'r') {
            return self.load_recent_prompts();
        }
        if is_control_key(&event, 't') {
            return self.open_fork();
        }
        if is_escape(&event) {
            if self.selection.clear() {
                self.selection_auto_scroll = None;
                self.key_confirmation = None;
                return ComponentUpdate::render(RenderRequest::Immediate);
            }
            if self.queue.focused() {
                self.key_confirmation = None;
                return self.update_queue(event);
            }
            if self.transcript.expandables_focused() {
                self.key_confirmation = None;
                return self.update_transcript(TranscriptEvent::BlurExpandables);
            }
            return self.update_key_confirmation(ConfirmationAction::Interrupt, Instant::now());
        }
        if self.transcript.pinned_prompt_clicked(&event) {
            return self.update_transcript(TranscriptEvent::JumpToPinnedPrompt);
        }
        if self.transcript.updates_banner_clicked(&event) {
            return self.update_transcript(TranscriptEvent::FollowTail);
        }
        if let Some(update) = self.update_selection_mouse(&mut event) {
            return update;
        }
        if let Some(destination) = self.transcript.link_destination(&event) {
            self.focus_composer();
            return ComponentUpdate {
                effects: vec![RootEffect::OpenLink(destination.to_string())],
                render: RenderRequest::Immediate,
            };
        }
        if let Event::Mouse(mouse) = &event
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
        {
            let position = Position::new(mouse.column, mouse.row);
            match self.composer.chrome_target(position) {
                Some(ComposerChromeTarget::Effort) => return self.open_effort(),
                Some(ComposerChromeTarget::Speed) => return self.open_speed(),
                Some(ComposerChromeTarget::Model) => return self.open_model(),
                Some(ComposerChromeTarget::Subagents) => {
                    self.subagents.open_tree();
                    self.overlay = Some(Overlay::Subagents(SubagentOverlay::Tree));
                    return ComponentUpdate::render(RenderRequest::Immediate);
                }
                None => {}
            }
        }
        if is_queue_shortcut(&event) && self.can_queue_draft() {
            return self.update_composer_with(
                ComposerEvent::Submit,
                RenderRequest::Immediate,
                BusyDelivery::Queue,
            );
        }
        if is_focus_toggle(&event) {
            return self.update_focus();
        }
        if is_left_click_in(&event, self.queue_area) {
            let Event::Mouse(mouse) = &event else {
                unreachable!("left click helper only accepts mouse events");
            };
            let _ = self.queue.focus_row(mouse.row, self.queue_area);
            let _ = self.transcript.update(TranscriptEvent::BlurExpandables);
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if is_left_click_in(&event, self.composer_area) {
            self.focus_composer();
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if let Some(command) = self.transcript.expandable_command(&event) {
            self.queue.set_focused(false);
            return self.update_transcript(TranscriptEvent::Expandable(command));
        }
        if is_left_click(&event) {
            self.focus_composer();
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if self.queue.focused() {
            return self.update_queue(event);
        }
        if !self.skills.is_empty()
            && !self.composer.draft().starts_with('!')
            && is_skill_picker_trigger(&event)
            && self.composer.cursor_is_at_token_boundary()
        {
            let start = self.composer.cursor();
            let update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            self.overlay = Some(Overlay::Skills(SkillMention {
                picker: SkillPicker::new(Arc::clone(&self.skills)),
                start,
            }));
            return update;
        }
        if is_file_finder_trigger(&event) && self.composer.cursor_is_at_token_boundary() {
            let start = self.composer.cursor();
            let update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            self.overlay = Some(Overlay::FileFinder(FileMention {
                finder: FileFinder::new(&self.workspace),
                start,
            }));
            return update;
        }
        if !self.reflection_input && self.composer.draft().is_empty() && is_actions_trigger(&event)
        {
            let new_session_enabled =
                self.turns.is_idle() && self.blocking_task.is_none() && self.queue.is_empty();
            self.overlay = Some(Overlay::Actions(ActionsMenu::new(ActionAvailability {
                new_session: new_session_enabled,
                fork: self.can_fork(),
                memory: self.memory_enabled,
                model: self.thread == ThreadState::New,
            })));
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if let Some(command) = self
            .transcript
            .scroll_command(&event, self.tui.mouse_scroll_lines)
        {
            let transcript = self.transcript.update(TranscriptEvent::Scroll(command));
            return ComponentUpdate {
                effects: Vec::new(),
                render: transcript.render,
            };
        }
        if self.transcript.expandables_focused() {
            return ComponentUpdate::none();
        }
        self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate)
    }

    fn update_handoff_input(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        if is_escape(&event) {
            self.key_confirmation = None;
            return ComponentUpdate {
                effects: vec![RootEffect::CancelHandoff],
                render: RenderRequest::Immediate,
            };
        }
        if is_key_release(&event) {
            return ComponentUpdate::none();
        }
        let confirmation_cleared = self.key_confirmation.take().is_some();
        ComponentUpdate::render(if confirmation_cleared {
            RenderRequest::Immediate
        } else {
            RenderRequest::None
        })
    }

    fn update_selection_mouse(&mut self, event: &mut Event) -> Option<ComponentUpdate<RootEffect>> {
        let Event::Mouse(mouse) = event else {
            return None;
        };
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let (surface, span) = self.selection_span_at(position)?;
                self.selection.begin(surface, span);
                self.selection_auto_scroll = None;
                Some(ComponentUpdate::render(RenderRequest::Immediate))
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let surface = self.selection.surface()?;
                let span = self.selection_span_on(surface, position)?;
                self.selection.drag(span);
                self.begin_selection_auto_scroll(surface, position);
                Some(ComponentUpdate::render(RenderRequest::Immediate))
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self.selection.is_active() || self.selection.is_pending() =>
            {
                let rows = if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                let render = match self.selection.surface()? {
                    Surface::Transcript => {
                        self.transcript
                            .update(TranscriptEvent::Scroll(ScrollCommand::Rows(rows)));
                        RenderRequest::Immediate
                    }
                    Surface::Composer => {
                        let changed = self
                            .composer
                            .scroll_selection(rows as isize, self.composer_content_area);
                        if changed {
                            RenderRequest::Immediate
                        } else {
                            RenderRequest::None
                        }
                    }
                };
                Some(ComponentUpdate::render(render))
            }
            MouseEventKind::Up(MouseButton::Left)
                if self.selection.is_active() || self.selection.is_pending() =>
            {
                let surface = self.selection.surface()?;
                self.selection_auto_scroll = None;
                let span = self.selection_span_on(surface, position)?;
                if !self.selection.finish(span) {
                    mouse.kind = MouseEventKind::Down(MouseButton::Left);
                    return None;
                }
                let range = self.selection.take_range()?;
                let text = match surface {
                    Surface::Transcript => self.transcript.selection_text(range),
                    Surface::Composer => self.composer.selection_text(range),
                };
                Some(ComponentUpdate {
                    effects: text.map(RootEffect::Copy).into_iter().collect(),
                    render: RenderRequest::Immediate,
                })
            }
            _ => None,
        }
    }

    fn selection_span_at(&mut self, position: Position) -> Option<(Surface, TextSpan)> {
        if self.composer_content_area.contains(position) {
            let span = self
                .composer
                .selection_span(position, self.composer_content_area)?;
            return Some((Surface::Composer, span));
        }
        if !self.transcript_area.contains(position) {
            return None;
        }
        let span = self.transcript.selection_span(position)?;
        Some((Surface::Transcript, span))
    }

    fn selection_span_on(&mut self, surface: Surface, position: Position) -> Option<TextSpan> {
        match surface {
            Surface::Transcript => {
                let position = clamp_to(position, self.transcript_area);
                let anchor = self.selection.anchor()?;
                self.transcript
                    .selection_span_nearest_from(position, anchor)
            }
            Surface::Composer => {
                let position = clamp_to(position, self.composer_content_area);
                self.composer
                    .selection_span(position, self.composer_content_area)
            }
        }
    }

    fn begin_selection_auto_scroll(&mut self, surface: Surface, position: Position) {
        let area = match surface {
            Surface::Transcript => self.transcript_area,
            Surface::Composer => self.composer_content_area,
        };
        let direction = if position.y <= area.y {
            -1
        } else if position.y >= area.bottom().saturating_sub(1) {
            1
        } else {
            self.selection_auto_scroll = None;
            return;
        };
        if let Some(scroll) = &mut self.selection_auto_scroll
            && scroll.direction == direction
        {
            scroll.position = position;
            return;
        }
        self.selection_auto_scroll = Some(SelectionAutoScroll {
            direction,
            position,
            deadline: Instant::now() + SELECTION_SCROLL_INTERVAL,
        });
    }

    fn scroll_selected_surface(&mut self, surface: Surface, rows: isize) -> bool {
        match surface {
            Surface::Transcript => {
                self.transcript
                    .update(TranscriptEvent::Scroll(ScrollCommand::Rows(rows as i32)));
                true
            }
            Surface::Composer => self
                .composer
                .scroll_selection(rows, self.composer_content_area),
        }
    }

    fn update_key_confirmation(
        &mut self,
        action: ConfirmationAction,
        now: Instant,
    ) -> ComponentUpdate<RootEffect> {
        let confirmed = self.key_confirmation.as_ref().is_some_and(|confirmation| {
            confirmation.action == action && now <= confirmation.deadline
        });
        if confirmed {
            self.key_confirmation = None;
            return ComponentUpdate {
                effects: vec![action.effect()],
                render: RenderRequest::Immediate,
            };
        }
        self.key_confirmation = Some(KeyConfirmation {
            action,
            deadline: now + KEY_CONFIRMATION_TIMEOUT,
        });
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_overlay(&mut self, event: Event, now: Instant) -> ComponentUpdate<RootEffect> {
        match &self.overlay {
            Some(Overlay::Actions(_)) => self.update_actions(event),
            Some(Overlay::ContextDiagnostics(_)) => self.update_context_diagnostics(event),
            Some(Overlay::Effort(_)) => self.update_effort(EffortEvent::Terminal { event, now }),
            Some(Overlay::Speed(_)) => self.update_speed(SpeedEvent::Terminal { event, now }),
            Some(Overlay::Model(_)) => self.update_model(ModelSelectorEvent::Terminal(event)),
            Some(Overlay::Theme(_)) => {
                self.update_theme_selector(ThemeSelectorEvent::Terminal(event))
            }
            Some(Overlay::FileFinder(_)) => self.update_file_finder(event),
            Some(Overlay::Skills(_)) => self.update_skill_picker(event),
            Some(Overlay::Keybindings(_)) => self.update_keybindings(event),
            Some(Overlay::QrCode(_)) => self.update_qr_code(event),
            Some(Overlay::Memory(_)) => self.update_memory(MemoryBrowserEvent::Terminal(event)),
            Some(Overlay::RecentPrompts(_)) => self.update_recent_prompt_picker(event),
            Some(Overlay::Sessions(_)) => self.update_session_picker(event),
            Some(Overlay::WebInstall(_)) => self.update_web_install(event),
            Some(Overlay::Subagents(SubagentOverlay::Tree)) => {
                let effect = self.subagents.update_tree(event);
                self.apply_subagent_effect(effect)
            }
            Some(Overlay::Subagents(SubagentOverlay::Transcript(id))) => {
                let effect =
                    self.subagents
                        .update_transcript(*id, event, self.tui.mouse_scroll_lines);
                self.apply_subagent_effect(effect)
            }
            None => ComponentUpdate::none(),
        }
    }

    fn apply_subagent_effect(
        &mut self,
        effect: Option<SubagentEffect>,
    ) -> ComponentUpdate<RootEffect> {
        match effect {
            Some(SubagentEffect::Dismiss) => {
                self.subagents.finish_camera_animation();
                self.overlay = None;
            }
            Some(SubagentEffect::Inspect(id)) => {
                self.subagents.finish_camera_animation();
                self.overlay = Some(Overlay::Subagents(SubagentOverlay::Transcript(id)));
            }
            Some(SubagentEffect::Back) => {
                self.overlay = Some(Overlay::Subagents(SubagentOverlay::Tree));
            }
            Some(SubagentEffect::OpenLink(destination)) => {
                return ComponentUpdate {
                    effects: vec![RootEffect::OpenLink(destination)],
                    render: RenderRequest::None,
                };
            }
            Some(SubagentEffect::SetMaxSubagents(limit)) => {
                return ComponentUpdate {
                    effects: vec![RootEffect::SetMaxSubagents(limit)],
                    render: RenderRequest::Immediate,
                };
            }
            None => {}
        }
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_file_finder(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::FileFinder(mention)) = &self.overlay else {
            return ComponentUpdate::none();
        };
        let start = mention.start;

        if is_key_release(&event) {
            return ComponentUpdate::none();
        }

        let starts_session_mention = is_file_finder_trigger(&event)
            && self
                .mention_query(start, '@')
                .is_some_and(|query| query.is_empty());
        if starts_session_mention {
            let composer =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            let mut sessions = self.load_session_mentions(start);
            sessions.render = sessions.render.max(composer.render);
            return sessions;
        }

        if is_mention_edit(&event) {
            let keep_open = mention_edit_continues_query(&event, is_file_query_character);
            let update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            let query = if keep_open {
                self.mention_query(start, '@')
            } else {
                None
            };
            let Some(query) = query else {
                self.overlay = None;
                return update;
            };
            if let Some(Overlay::FileFinder(mention)) = &mut self.overlay {
                let _ = mention.finder.update(FileFinderEvent::Query(query));
            }
            return update;
        }

        if !is_picker_navigation(&event) {
            self.overlay = None;
            if is_escape(&event) {
                return ComponentUpdate::render(RenderRequest::Immediate);
            }
            let mut update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            update.render = update.render.max(RenderRequest::Immediate);
            return update;
        }

        let Some(Overlay::FileFinder(mention)) = &mut self.overlay else {
            unreachable!("file mention was checked above");
        };
        let update = mention.finder.update(FileFinderEvent::Terminal(event));
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };

        self.overlay = None;
        match effect {
            FileFinderEffect::Dismiss => ComponentUpdate::render(RenderRequest::Immediate),
            FileFinderEffect::Insert(path) => self.update_composer(
                ComposerEvent::ReplaceRange {
                    range: start..self.composer.cursor(),
                    text: format!("@{path} "),
                },
                RenderRequest::Immediate,
            ),
        }
    }

    fn update_skill_picker(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Skills(mention)) = &self.overlay else {
            return ComponentUpdate::none();
        };
        let start = mention.start;

        if is_key_release(&event) {
            return ComponentUpdate::none();
        }

        if is_mention_edit(&event) {
            let keep_open = mention_edit_continues_query(&event, is_skill_query_character);
            let update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            let query = if keep_open {
                self.mention_query(start, '$')
            } else {
                None
            };
            let Some(query) = query else {
                self.overlay = None;
                return update;
            };
            if let Some(Overlay::Skills(mention)) = &mut self.overlay {
                let _ = mention.picker.update(SkillPickerEvent::Query(query));
            }
            return update;
        }

        if !is_picker_navigation(&event) {
            self.overlay = None;
            if is_escape(&event) {
                return ComponentUpdate::render(RenderRequest::Immediate);
            }
            let mut update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            update.render = update.render.max(RenderRequest::Immediate);
            return update;
        }

        let Some(Overlay::Skills(mention)) = &mut self.overlay else {
            unreachable!("skill picker was checked above");
        };
        let update = mention.picker.update(SkillPickerEvent::Terminal(event));
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };

        self.overlay = None;
        match effect {
            SkillPickerEffect::Dismiss => ComponentUpdate::render(RenderRequest::Immediate),
            SkillPickerEffect::Insert(name) => self.update_composer(
                ComposerEvent::ReplaceRange {
                    range: start..self.composer.cursor(),
                    text: format!("${name} "),
                },
                RenderRequest::Immediate,
            ),
        }
    }

    fn mention_query(&self, start: usize, prefix: char) -> Option<String> {
        let composer = &self.composer;
        composer
            .draft()
            .get(start..composer.cursor())?
            .strip_prefix(prefix)
            .map(str::to_owned)
    }

    fn update_actions(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Actions(actions)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = actions.update(ActionsEvent::Terminal(event));
        match update.effects.into_iter().next() {
            Some(ActionsEffect::Dismiss) => self.overlay = None,
            Some(ActionsEffect::Copy(argument)) => {
                self.overlay = None;
                return self.copy_response(&argument);
            }
            Some(ActionsEffect::Trigger(Action::Copy)) => {
                self.overlay = None;
                return self.copy_response("");
            }
            Some(ActionsEffect::Trigger(Action::Subagents)) => {
                self.subagents.open_tree();
                self.overlay = Some(Overlay::Subagents(SubagentOverlay::Tree));
            }
            Some(ActionsEffect::Trigger(Action::Effort)) => {
                return self.open_effort();
            }
            Some(ActionsEffect::Trigger(Action::Model)) => {
                return self.open_model();
            }
            Some(ActionsEffect::Trigger(Action::Speed)) => return self.open_speed(),
            Some(ActionsEffect::Trigger(Action::Theme)) => {
                self.overlay = Some(Overlay::Theme(ThemeSelector::new(self.theme_mode)));
            }
            Some(ActionsEffect::Trigger(Action::NewSession)) => {
                return self.open_new_session();
            }
            Some(ActionsEffect::Trigger(Action::ResumeSession)) => {
                return self.load_sessions();
            }
            Some(ActionsEffect::Trigger(Action::Fork)) => return self.open_fork(),
            Some(ActionsEffect::Trigger(Action::Keybindings)) => {
                self.overlay = Some(Overlay::Keybindings(KeybindingsHelp::default()));
            }
            Some(ActionsEffect::Trigger(Action::ReloadConfig)) => {
                self.overlay = None;
                return ComponentUpdate {
                    effects: vec![RootEffect::ReloadConfig],
                    render: RenderRequest::Immediate,
                };
            }
            Some(ActionsEffect::Trigger(Action::EditConfig)) => {
                self.overlay = None;
                return ComponentUpdate {
                    effects: vec![RootEffect::OpenConfigEditor],
                    render: RenderRequest::Immediate,
                };
            }
            Some(ActionsEffect::Trigger(Action::Memory)) => {
                if !self.memory_enabled {
                    return ComponentUpdate {
                        effects: Vec::new(),
                        render: update.render,
                    };
                }
                self.overlay = Some(Overlay::Memory(MemoryBrowser::new()));
                return ComponentUpdate {
                    effects: vec![RootEffect::LoadMemories],
                    render: RenderRequest::Immediate,
                };
            }
            Some(ActionsEffect::Trigger(Action::DebugContext)) => {
                self.overlay = Some(Overlay::ContextDiagnostics(ContextDiagnosticsPanel::new(
                    self.context_diagnostics.clone(),
                )));
            }
            Some(ActionsEffect::Trigger(Action::Compact)) => {
                self.overlay = None;
                return self
                    .start_compaction()
                    .unwrap_or_else(|_| ComponentUpdate::none());
            }
            Some(ActionsEffect::Trigger(Action::Reflection)) => {
                self.overlay = None;
                self.reflection_input = true;
                return self.update_composer(
                    ComposerEvent::InputMode(InputMode::Reflection),
                    RenderRequest::Immediate,
                );
            }
            Some(ActionsEffect::Trigger(
                action @ (Action::OpenInBrowser
                | Action::CopyWebLink
                | Action::ShowQrCode
                | Action::Sessions),
            )) => {
                self.overlay = None;
                let effect = match action {
                    Action::OpenInBrowser => RootEffect::OpenWebInterface { install: false },
                    Action::CopyWebLink => RootEffect::CopyWebLink,
                    Action::ShowQrCode => RootEffect::ShowWebQr,
                    _ => RootEffect::OpenSessions,
                };
                return ComponentUpdate {
                    effects: vec![effect],
                    render: RenderRequest::Immediate,
                };
            }
            Some(ActionsEffect::Trigger(Action::Handoff)) => {
                self.overlay = None;
                return self.start_handoff();
            }
            None => {}
        }
        ComponentUpdate {
            effects: Vec::new(),
            render: update.render,
        }
    }

    fn update_context_diagnostics(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::ContextDiagnostics(panel)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = panel.update(ContextDiagnosticsEvent::Terminal(event));
        match update.effects.into_iter().next() {
            Some(ContextDiagnosticsEffect::Dismiss) => self.overlay = None,
            Some(ContextDiagnosticsEffect::Refresh) => {
                if let Some(Overlay::ContextDiagnostics(panel)) = &mut self.overlay {
                    panel.replace(self.context_diagnostics.clone());
                }
            }
            None => {}
        }
        ComponentUpdate {
            effects: Vec::new(),
            render: update.render,
        }
    }

    fn update_web_install(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::WebInstall(confirmation)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = confirmation.update(ConfirmationEvent::Terminal(event));
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };
        self.overlay = None;
        match effect {
            ConfirmationEffect::Confirm => ComponentUpdate {
                effects: vec![RootEffect::OpenWebInterface { install: true }],
                render: RenderRequest::Immediate,
            },
            ConfirmationEffect::Dismiss => ComponentUpdate::render(RenderRequest::Immediate),
        }
    }

    fn update_memory(&mut self, event: MemoryBrowserEvent) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Memory(browser)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = browser.update(event);
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };

        match effect {
            MemoryBrowserEffect::Dismiss => {
                self.overlay = None;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            MemoryBrowserEffect::Refresh => ComponentUpdate {
                effects: vec![RootEffect::LoadMemories],
                render: update.render,
            },
            MemoryBrowserEffect::Delete(key) => ComponentUpdate {
                effects: vec![RootEffect::DeleteMemory(key)],
                render: update.render,
            },
        }
    }

    /// Claude fixes effort when its session starts.
    fn effort_locked(&self) -> bool {
        self.thread == ThreadState::Started && matches!(self.composer.model(), Model::Claude(_))
    }

    fn open_effort(&mut self) -> ComponentUpdate<RootEffect> {
        if self.effort_locked() {
            self.overlay = None;
            self.notification = Some(Notification::plain(
                "Effort is fixed for this Claude session; start a new session to change it."
                    .to_owned(),
                Color::Yellow,
            ));
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        self.overlay = Some(Overlay::Effort(EffortSelector::new(
            self.composer.effort(),
            self.preferred_reasoning_mode == ReasoningMode::Pro,
            model::reasoning_modes(self.composer.model()).contains(&ReasoningMode::Pro),
        )));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn open_speed(&mut self) -> ComponentUpdate<RootEffect> {
        self.overlay = Some(Overlay::Speed(SpeedSelector::new(
            self.composer.speed(),
            self.composer.model(),
        )));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_speed(&mut self, event: SpeedEvent) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Speed(selector)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = selector.update(event);
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };
        self.overlay = None;
        match effect {
            SpeedEffect::Dismiss => ComponentUpdate::render(RenderRequest::Immediate),
            SpeedEffect::Apply(speed) => self.apply_speed(speed),
        }
    }

    fn apply_speed(&mut self, speed: Speed) -> ComponentUpdate<RootEffect> {
        self.set_speed(speed);
        ComponentUpdate {
            effects: vec![RootEffect::SetSpeed(speed)],
            render: RenderRequest::Immediate,
        }
    }

    fn open_model(&mut self) -> ComponentUpdate<RootEffect> {
        if self.thread != ThreadState::New {
            return ComponentUpdate::none();
        }
        self.overlay = Some(Overlay::Model(ModelSelector::new(
            self.composer.model(),
            self.claude_enabled,
        )));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_theme_selector(&mut self, event: ThemeSelectorEvent) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Theme(selector)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = selector.update(event);
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };
        self.overlay = None;
        match effect {
            ThemeSelectorEffect::Dismiss => ComponentUpdate::render(RenderRequest::Immediate),
            ThemeSelectorEffect::Apply(mode) => ComponentUpdate {
                effects: vec![RootEffect::SetTheme(mode)],
                render: RenderRequest::Immediate,
            },
        }
    }

    fn open_fork(&mut self) -> ComponentUpdate<RootEffect> {
        if !self.can_fork() {
            return ComponentUpdate::none();
        }
        self.overlay = None;
        ComponentUpdate {
            effects: vec![RootEffect::Fork],
            render: RenderRequest::Immediate,
        }
    }

    fn can_fork(&self) -> bool {
        self.fork_available
    }

    fn open_new_session(&mut self) -> ComponentUpdate<RootEffect> {
        if !self.turns.is_idle() || !self.queue.is_empty() {
            return ComponentUpdate::none();
        }
        self.overlay = None;
        self.interactive = false;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Starting new session…".to_owned()),
            now: Instant::now(),
        });
        ComponentUpdate {
            effects: vec![RootEffect::NewSession(self.composer.model())],
            render: RenderRequest::Immediate,
        }
    }

    pub(super) fn load_sessions(&mut self) -> ComponentUpdate<RootEffect> {
        self.overlay = None;
        self.pending_session_mention = None;
        self.interactive = false;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Loading sessions…".to_owned()),
            now: Instant::now(),
        });
        ComponentUpdate {
            effects: vec![RootEffect::LoadSessions(SessionListKind::Resume)],
            render: RenderRequest::Immediate,
        }
    }

    fn load_session_mentions(&mut self, start: usize) -> ComponentUpdate<RootEffect> {
        self.overlay = None;
        self.pending_session_mention = Some(start);
        self.interactive = false;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Loading sessions…".to_owned()),
            now: Instant::now(),
        });
        ComponentUpdate {
            effects: vec![RootEffect::LoadSessions(SessionListKind::Mention)],
            render: RenderRequest::Immediate,
        }
    }

    fn load_recent_prompts(&mut self) -> ComponentUpdate<RootEffect> {
        self.overlay = None;
        self.interactive = false;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: true,
            status: Some("Loading recent prompts…".to_owned()),
            now: Instant::now(),
        });
        ComponentUpdate {
            effects: vec![RootEffect::LoadRecentPrompts(self.recent_prompts.clone())],
            render: RenderRequest::Immediate,
        }
    }

    fn recent_prompts_loaded(
        &mut self,
        session_id: String,
        prompts: Vec<RecentPrompt>,
    ) -> ComponentUpdate<RootEffect> {
        self.interactive = true;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: false,
            status: None,
            now: Instant::now(),
        });
        self.overlay = Some(Overlay::RecentPrompts(RecentPromptPicker::new(
            prompts, session_id,
        )));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_recent_prompt_picker(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::RecentPrompts(picker)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = picker.update(RecentPromptPickerEvent::Terminal(event));
        match update.effects.into_iter().next() {
            Some(RecentPromptPickerEffect::Dismiss) => {
                self.overlay = None;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            Some(RecentPromptPickerEffect::Insert(prompt)) => {
                self.overlay = None;
                self.update_composer(
                    ComposerEvent::ReplaceDraft(prompt),
                    RenderRequest::Immediate,
                )
            }
            None => ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            },
        }
    }

    fn recent_prompt_load_failed(&mut self, message: String) -> ComponentUpdate<RootEffect> {
        self.interactive = true;
        self.notification = Some(Notification::plain(message, Color::Red));
        self.update_composer(
            ComposerEvent::Activity {
                active: false,
                status: None,
                now: Instant::now(),
            },
            RenderRequest::Immediate,
        )
    }

    fn sessions_loaded(&mut self, sessions: Vec<SessionSummary>) -> ComponentUpdate<RootEffect> {
        self.interactive = true;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: false,
            status: None,
            now: Instant::now(),
        });
        let mode = if self.pending_session_mention.is_some() {
            SessionPickerMode::Mention
        } else {
            SessionPickerMode::Resume
        };
        self.overlay = Some(Overlay::Sessions(SessionPicker::new(sessions, mode)));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_session_picker(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Sessions(picker)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = picker.update(SessionPickerEvent::Terminal(event));
        match update.effects.into_iter().next() {
            Some(SessionPickerEffect::Dismiss) => {
                self.overlay = None;
                self.pending_session_mention = None;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            Some(SessionPickerEffect::Resume(session_id)) => {
                self.overlay = None;
                self.interactive = false;
                let _ = self.composer.update(ComposerEvent::Activity {
                    active: true,
                    status: Some("Resuming session…".to_owned()),
                    now: Instant::now(),
                });
                ComponentUpdate {
                    effects: vec![RootEffect::ResumeSession(session_id)],
                    render: RenderRequest::Immediate,
                }
            }
            Some(SessionPickerEffect::Mention(session_id)) => {
                self.overlay = None;
                let Some(start) = self.pending_session_mention.take() else {
                    return ComponentUpdate::none();
                };
                self.update_composer(
                    ComposerEvent::ReplaceRange {
                        range: start..self.composer.cursor(),
                        text: format!("@@{session_id} "),
                    },
                    RenderRequest::Immediate,
                )
            }
            None => ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            },
        }
    }

    fn session_load_failed(&mut self, message: String) -> ComponentUpdate<RootEffect> {
        self.pending_session_mention = None;
        self.interactive = true;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: false,
            status: None,
            now: Instant::now(),
        });
        self.notification = Some(Notification::plain(message, Color::Red));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn new_session_failed(&mut self, message: String) -> ComponentUpdate<RootEffect> {
        self.interactive = true;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: false,
            status: None,
            now: Instant::now(),
        });
        self.notification = Some(Notification::plain(
            format!("Could not start a new session: {message}"),
            Color::Red,
        ));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn fork_ready(&mut self) -> ComponentUpdate<RootEffect> {
        self.interactive = true;
        let update = self.composer.update(ComposerEvent::Activity {
            active: false,
            status: None,
            now: Instant::now(),
        });
        debug_assert!(update.changed);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_qr_code(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::QrCode(view)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = view.update(QrCodeEvent::Terminal(event));
        if matches!(update.effects.as_slice(), [QrCodeEffect::Dismiss]) {
            self.overlay = None;
        }
        ComponentUpdate::render(update.render)
    }

    fn update_keybindings(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Keybindings(help)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = help.update(KeybindingsEvent::Terminal(event));
        if matches!(update.effects.as_slice(), [KeybindingsEffect::Dismiss]) {
            self.overlay = None;
        }
        ComponentUpdate::render(update.render)
    }

    fn set_effort(&mut self, effort: ReasoningEffort) {
        self.transcript.set_effort(effort);
        self.subagents.set_effort(effort);
        self.composer.update(ComposerEvent::SetEffort(effort));
    }

    fn update_effort(&mut self, event: EffortEvent) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Effort(selector)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = selector.update(event);
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };

        self.overlay = None;
        match effect {
            EffortEffect::Dismiss => ComponentUpdate::render(RenderRequest::Immediate),
            EffortEffect::Apply(effort, pro) => self.apply_effort(effort, pro),
        }
    }

    fn apply_effort(&mut self, effort: ReasoningEffort, pro: bool) -> ComponentUpdate<RootEffect> {
        let reasoning_mode = if pro {
            ReasoningMode::Pro
        } else {
            ReasoningMode::Standard
        };
        let previous_reasoning_mode = self.preferred_reasoning_mode;
        self.preferred_reasoning_mode = reasoning_mode;
        if reasoning_mode != previous_reasoning_mode {
            let state = if pro { "enabled" } else { "disabled" };
            let suffix = if self.composer.reasoning_mode() != reasoning_mode {
                " · start a new session to apply."
            } else {
                "."
            };
            let message = format!("Pro {state} for new sessions{suffix}");
            self.notification = Some(Notification::plain(message, Color::Green));
        }
        self.set_effort(effort);
        ComponentUpdate {
            effects: vec![RootEffect::SetEffort {
                effort,
                reasoning_mode,
            }],
            render: RenderRequest::Immediate,
        }
    }

    fn update_model(&mut self, event: ModelSelectorEvent) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::Model(selector)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = selector.update(event);
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };

        self.overlay = None;
        match effect {
            ModelSelectorEffect::Dismiss => ComponentUpdate::render(RenderRequest::Immediate),
            ModelSelectorEffect::Apply(model) if model == self.composer.model() => {
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            ModelSelectorEffect::Apply(model) => self.apply_model(model),
        }
    }

    fn apply_model(&mut self, model: Model) -> ComponentUpdate<RootEffect> {
        self.interactive = false;
        let _ = self.composer.update(ComposerEvent::Activity {
            active: true,
            status: Some(format!("Starting {} session…", model::name(model))),
            now: Instant::now(),
        });
        ComponentUpdate {
            effects: vec![RootEffect::SetModel(model)],
            render: RenderRequest::Immediate,
        }
    }

    fn update_focus(&mut self) -> ComponentUpdate<RootEffect> {
        let focus_queue = !self.queue.focused() && !self.queue.is_empty();
        self.queue.set_focused(focus_queue);
        let transcript = self.transcript.update(TranscriptEvent::BlurExpandables);
        ComponentUpdate::render(if focus_queue || transcript.render != RenderRequest::None {
            RenderRequest::Immediate
        } else {
            RenderRequest::None
        })
    }

    fn focus_composer(&mut self) {
        self.queue.set_focused(false);
        let _ = self.transcript.update(TranscriptEvent::BlurExpandables);
    }

    fn update_queue(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let update = self.queue.update(QueueEvent::Terminal(event));
        let mut result = ComponentUpdate::render(update.render);
        for effect in update.effects {
            match effect {
                QueueEffect::Blur => {}
                QueueEffect::Edit { id, text } => result.merge(self.begin_queue_edit(id, text)),
                QueueEffect::Steer { id, prompt } => {
                    result.effects.push(RootEffect::Steer { id, prompt });
                }
            }
        }
        result
    }

    /// Starts delivering a waiting queue item into the running turn.
    fn steer_queued(&mut self, id: QueueId) -> ComponentUpdate<RootEffect> {
        let Some(prompt) = self.queue.steer(id) else {
            return ComponentUpdate::none();
        };
        ComponentUpdate {
            effects: vec![RootEffect::Steer { id, prompt }],
            render: RenderRequest::Immediate,
        }
    }

    fn begin_queue_edit(&mut self, id: QueueId, text: String) -> ComponentUpdate<RootEffect> {
        let original_input_mode = self.composer.input_mode();
        let original_draft = self.composer.take_draft();
        self.composer.replace_draft(text);
        let _ = self
            .composer
            .update(ComposerEvent::InputMode(InputMode::EditingQueued));
        self.queue_edit = Some(QueueEdit {
            id,
            original_draft,
            original_input_mode,
        });
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn update_queue_editor(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        if is_escape(&event) {
            return self.finish_queue_edit(false);
        }
        if is_plain_enter(&event) {
            return self.finish_queue_edit(true);
        }
        self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate)
    }

    fn finish_queue_edit(&mut self, save: bool) -> ComponentUpdate<RootEffect> {
        let Some(edit) = self.queue_edit.take() else {
            return ComponentUpdate::none();
        };
        let text = save.then(|| self.composer.draft().to_owned());
        self.composer.replace_draft(String::new());
        if let Some(draft) = edit.original_draft {
            self.composer.restore_draft(draft);
        }
        let _ = self
            .composer
            .update(ComposerEvent::InputMode(edit.original_input_mode));

        let restored = match text {
            Some(text) => self.queue.finish_edit(edit.id, text),
            None => self.queue.cancel_edit(edit.id),
        };
        if !restored {
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        self.submit_next_queued()
    }

    /// Closes overlays and cancels an inline queue edit, restoring the draft it displaced.
    fn end_local_interaction(&mut self) -> ComponentUpdate<RootEffect> {
        let overlay_closed = self.overlay.take().is_some();
        self.pending_session_mention = None;
        let mut update = if self.queue_edit.is_some() {
            self.finish_queue_edit(false)
        } else {
            ComponentUpdate::none()
        };
        if overlay_closed {
            update.render = update.render.max(RenderRequest::Immediate);
        }
        update
    }

    fn compaction_allowed(&self) -> Result<(), CommandError> {
        if !self.turns.is_idle() || self.blocking_task.is_some() {
            return Err(CommandError::TurnRunning);
        }
        if !self.queue.is_empty() {
            return Err(CommandError::QueueNotEmpty);
        }
        Ok(())
    }

    fn start_compaction(&mut self) -> Result<ComponentUpdate<RootEffect>, CommandError> {
        self.compaction_allowed()?;
        self.blocking_task = Some(BlockingTask::Compaction);
        Ok(ComponentUpdate {
            effects: vec![RootEffect::Compact],
            render: RenderRequest::Immediate,
        })
    }

    pub(crate) const fn busy(&self) -> Busy {
        self.turns.busy()
    }

    /// The draft as other windows see it: an inline queue edit borrows the composer, so the draft
    /// it displaced is the one shared.
    pub(crate) fn shared_draft(&self) -> &str {
        match &self.queue_edit {
            Some(edit) => edit.original_draft.as_ref().map_or("", ComposerDraft::text),
            None => self.composer.draft(),
        }
    }

    /// The images of [`Self::shared_draft`] as (marker, data URL) pairs.
    pub(crate) fn shared_draft_images(&self) -> Box<dyn Iterator<Item = (&str, &str)> + '_> {
        match &self.queue_edit {
            Some(edit) => match &edit.original_draft {
                Some(draft) => Box::new(draft.images()),
                None => Box::new(std::iter::empty()),
            },
            None => Box::new(self.composer.images()),
        }
    }

    pub(super) fn queued_prompts(&self) -> impl Iterator<Item = QueueEntry<'_>> {
        self.queue.entries()
    }

    /// The session's first prompt, which names it in session lists.
    pub(crate) fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub(super) fn set_live_sessions(&mut self, summary: Option<LiveSessions>) -> RenderRequest {
        let update = self.composer.update(ComposerEvent::LiveSessions(summary));
        if update.changed {
            RenderRequest::Immediate
        } else {
            RenderRequest::None
        }
    }

    fn copy_response(&mut self, argument: &str) -> ComponentUpdate<RootEffect> {
        let argument = argument.trim();
        let index = if argument.is_empty() {
            Some(1)
        } else if argument.bytes().all(|byte| byte.is_ascii_digit()) {
            argument.parse::<usize>().ok().filter(|index| *index > 0)
        } else {
            None
        };
        let result = match index {
            Some(index) => self
                .transcript
                .assistant_response(index)
                .map(|text| RootEffect::Copy(text.to_owned()))
                .ok_or_else(|| format!("No completed assistant response at position {index}.")),
            None => Err("Usage: /copy [N], where N is a positive integer (1 = latest).".to_owned()),
        };
        match result {
            Ok(effect) => ComponentUpdate {
                effects: vec![effect],
                render: RenderRequest::Immediate,
            },
            Err(message) => {
                self.notification = Some(Notification::plain(message, Color::Red));
                ComponentUpdate::render(RenderRequest::Immediate)
            }
        }
    }

    fn update_composer(
        &mut self,
        event: ComposerEvent,
        priority: RenderRequest,
    ) -> ComponentUpdate<RootEffect> {
        self.update_composer_with(event, priority, BusyDelivery::Steer)
    }

    /// Whether shift+tab has a draft to queue: a turn (or a steer still being applied) is busy and
    /// the composer holds a prompt.
    fn can_queue_draft(&self) -> bool {
        !self.reflection_input
            && (self.turns.turn_running() || self.queue.has_pending_steer())
            && !self.composer.draft().trim().is_empty()
    }

    fn update_composer_with(
        &mut self,
        event: ComposerEvent,
        priority: RenderRequest,
        delivery: BusyDelivery,
    ) -> ComponentUpdate<RootEffect> {
        let update = self.composer.update(event);
        if let Some(ComposerEffect::Submit(prompt)) = &update.effect
            && let Some(argument) = copy_command_argument(prompt.display_text().trim())
        {
            return self.copy_response(argument);
        }
        let submitted = matches!(&update.effect, Some(ComposerEffect::Submit(_)));
        if submitted {
            self.thread = ThreadState::Started;
        }
        let mut render = if update.changed {
            priority
        } else {
            RenderRequest::None
        };
        if submitted {
            render = render.max(self.update_transcript(TranscriptEvent::FollowTail).render);
        }
        let effects = match update.effect {
            Some(ComposerEffect::Submit(prompt))
                if self.turns.turn_running() || self.queue.has_pending_steer() =>
            {
                let id = self.queue.push(prompt);
                // A steer is only possible while a turn runs; while one is still being applied the
                // prompt waits behind it.
                if delivery == BusyDelivery::Steer && self.turns.turn_running() {
                    let steer = self.steer_queued(id);
                    render = render.max(steer.render);
                    steer.effects
                } else {
                    Vec::new()
                }
            }
            Some(ComposerEffect::Submit(prompt)) => {
                self.turns.start_turn();
                vec![RootEffect::Submit(prompt)]
            }
            Some(ComposerEffect::RunShell(command)) => {
                self.turns.start_shell();
                vec![RootEffect::RunShell(command)]
            }
            Some(ComposerEffect::OpenDraftEditor) => vec![RootEffect::OpenDraftEditor],
            None => Vec::new(),
        };

        ComponentUpdate { effects, render }
    }

    fn start_handoff(&mut self) -> ComponentUpdate<RootEffect> {
        self.blocking_task = Some(BlockingTask::Handoff);
        let waiting = self.update_composer(
            ComposerEvent::TaskStatus {
                status: Some("Preparing handoff…".to_owned()),
                now: Instant::now(),
            },
            RenderRequest::Immediate,
        );
        ComponentUpdate {
            effects: vec![RootEffect::Handoff],
            render: waiting.render.max(RenderRequest::Immediate),
        }
    }

    fn submit_reflection(&mut self) -> ComponentUpdate<RootEffect> {
        let instructions = self
            .composer
            .take_submission()
            .unwrap_or_else(|| Submission::text(String::new()));
        self.start_reflection(instructions)
    }

    fn start_reflection(&mut self, instructions: Submission) -> ComponentUpdate<RootEffect> {
        self.reflection_input = false;
        let mode = self.update_composer(
            ComposerEvent::InputMode(InputMode::Prompt),
            RenderRequest::Immediate,
        );
        self.thread = ThreadState::Started;
        self.turns.start_turn();
        let transcript = self.update_transcript(TranscriptEvent::FollowTail);
        ComponentUpdate {
            effects: vec![RootEffect::Reflect(instructions)],
            render: mode.render.max(transcript.render),
        }
    }

    fn cancel_reflection(&mut self) -> ComponentUpdate<RootEffect> {
        self.reflection_input = false;
        self.composer.replace_draft(String::new());
        self.update_composer(
            ComposerEvent::InputMode(InputMode::Prompt),
            RenderRequest::Immediate,
        )
    }

    fn discard_draft(&mut self) -> ComponentUpdate<RootEffect> {
        let Some(draft) = self.composer.take_draft() else {
            return ComponentUpdate::none();
        };
        self.discarded_draft = Some(draft);
        self.show_draft_saved();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn restore_discarded_draft(&mut self) -> ComponentUpdate<RootEffect> {
        if !self.composer.draft().is_empty() {
            return ComponentUpdate::none();
        }
        let Some(draft) = self.discarded_draft.take() else {
            return ComponentUpdate::none();
        };
        self.composer.restore_draft(draft);
        self.notification = Some(Notification::plain(
            "Draft restored.".to_owned(),
            Color::Green,
        ));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn show_draft_saved(&mut self) {
        self.notification = Some(Notification::plain(
            "Draft cleared · Ctrl+Z to restore".to_owned(),
            Color::Yellow,
        ));
    }

    /// Records a turn-end signal and continues the queue once the turn has finished.
    fn turn_ended(&mut self, end: TurnEnd) -> ComponentUpdate<RootEffect> {
        if self.turns.record_end(end) {
            self.submit_next_queued()
        } else {
            ComponentUpdate::none()
        }
    }

    fn turns_cancelled(&mut self) -> ComponentUpdate<RootEffect> {
        self.queue.cancel_steers();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn steer_admitted(&mut self, id: QueueId) -> ComponentUpdate<RootEffect> {
        let applied = self.queue.steer_admitted(id);
        self.finish_applied_steer(applied)
    }

    fn steer_promoted(&mut self, id: QueueId) -> ComponentUpdate<RootEffect> {
        let _ = self.queue.steer_promoted(id);
        self.turns.start_turn();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn steer_failed(&mut self, id: QueueId) -> ComponentUpdate<RootEffect> {
        self.queue.steer_failed(id);
        self.submit_next_queued()
    }

    fn steer_applied(&mut self) -> ComponentUpdate<RootEffect> {
        let applied = self.queue.steer_applied();
        self.finish_applied_steer(applied)
    }

    fn finish_applied_steer(&mut self, applied: Option<Submission>) -> ComponentUpdate<RootEffect> {
        let mut update = self.submit_next_queued();
        if let Some(prompt) = applied {
            update.effects.insert(
                0,
                RootEffect::PersistSteer(prompt.display_text().to_owned()),
            );
        }
        update
    }

    fn submit_next_queued(&mut self) -> ComponentUpdate<RootEffect> {
        if self.turns.turn_running() || self.queue.has_pending_steer() {
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        let prompts = self.queue.drain_ready();
        if prompts.is_empty() {
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        self.turns.start_turn();
        ComponentUpdate {
            effects: vec![RootEffect::Submit(Submission::join(prompts))],
            render: RenderRequest::Immediate,
        }
    }

    fn update_transcript(&mut self, event: TranscriptEvent) -> ComponentUpdate<RootEffect> {
        let update = self.transcript.update(event);
        let mut render = update.render;
        for effect in update.effects {
            let composer = self.composer.update(ComposerEvent::Activity {
                active: effect.active,
                status: effect.status,
                now: Instant::now(),
            });
            if composer.changed {
                render = render.max(RenderRequest::Streaming);
            }
        }
        ComponentUpdate {
            effects: Vec::new(),
            render,
        }
    }

    fn update_animation(&mut self, now: Instant) -> ComponentUpdate<RootEffect> {
        let confirmation = if self
            .key_confirmation
            .as_ref()
            .is_some_and(|confirmation| now >= confirmation.deadline)
        {
            self.key_confirmation = None;
            RenderRequest::Immediate
        } else {
            RenderRequest::None
        };
        let effort = self.update_effort(EffortEvent::AnimationFrame(now));
        let speed = self.update_speed(SpeedEvent::AnimationFrame(now));
        let transcript = self.update_transcript(TranscriptEvent::AnimationFrame(now));
        let composer =
            self.update_composer(ComposerEvent::AnimationFrame(now), RenderRequest::Streaming);
        let queue = self.queue.update(QueueEvent::AnimationFrame(now));
        debug_assert!(queue.effects.is_empty());
        let subagents = if self.subagents.advance(now) {
            RenderRequest::Streaming
        } else {
            RenderRequest::None
        };
        let selection = self.update_selection_auto_scroll(now);
        let notification = if self
            .notification
            .as_ref()
            .is_some_and(|notice| now >= notice.deadline)
        {
            self.notification = None;
            RenderRequest::Immediate
        } else {
            RenderRequest::None
        };
        ComponentUpdate {
            effects: effort
                .effects
                .into_iter()
                .chain(speed.effects)
                .chain(composer.effects)
                .collect(),
            render: effort
                .render
                .max(speed.render)
                .max(transcript.render)
                .max(composer.render)
                .max(queue.render)
                .max(subagents)
                .max(selection)
                .max(confirmation)
                .max(notification),
        }
    }

    fn update_selection_auto_scroll(&mut self, now: Instant) -> RenderRequest {
        let Some(mut scroll) = self.selection_auto_scroll.take() else {
            return RenderRequest::None;
        };
        if now < scroll.deadline {
            self.selection_auto_scroll = Some(scroll);
            return RenderRequest::None;
        }
        let Some(surface) = self.selection.surface() else {
            return RenderRequest::None;
        };
        let Some(span) = self.selection_span_on(surface, scroll.position) else {
            return RenderRequest::None;
        };
        self.selection.drag(span);
        if !self.scroll_selected_surface(surface, scroll.direction) {
            return RenderRequest::None;
        }
        scroll.deadline = now + SELECTION_SCROLL_INTERVAL;
        self.selection_auto_scroll = Some(scroll);
        RenderRequest::Immediate
    }

    fn apply_subagent_update(&mut self, update: AgentUpdate) -> ComponentUpdate<RootEffect> {
        let previous_active = self.subagents.active_count();
        let completion = match &update {
            AgentUpdate::Status {
                id,
                status: AgentStatus::Completed { .. },
            } => Some(*id),
            _ => None,
        };
        let root_message = match &update {
            AgentUpdate::Message(update)
                if update.thread.messages.iter().any(|message| {
                    message.id == update.message_id
                        && matches!(message.from, MessageSender::Agent { .. })
                }) =>
            {
                Some(update.clone())
            }
            _ => None,
        };
        let subagents_changed = self.subagents.apply(update);
        let mut result = root_message.map_or_else(ComponentUpdate::none, |update| {
            self.update_transcript(TranscriptEvent::DirectedMessage {
                perspective: MessageSender::Root,
                update,
            })
        });
        if !subagents_changed && result.render == RenderRequest::None {
            return result;
        }
        if let Some(Overlay::Subagents(SubagentOverlay::Transcript(id))) = self.overlay
            && !self.subagents.contains(id)
        {
            self.overlay = Some(Overlay::Subagents(SubagentOverlay::Tree));
        }
        let active = self.subagents.active_count();
        if active != previous_active {
            let _ = self.composer.update(ComposerEvent::ActiveSubagents {
                count: active,
                now: Instant::now(),
            });
        }
        if subagents_changed {
            result.render = result.render.max(RenderRequest::Immediate);
        }
        if let Some(id) = completion
            && subagents_changed
            && self.subagents.is_direct_child(id)
            && !self.turns.turn_running()
            && self.blocking_task.is_none()
            && self.interactive
        {
            self.thread = ThreadState::Started;
            self.turns.start_turn();
            result
                .effects
                .push(RootEffect::ContinueSubagent(subagent_completion_prompt(id)));
        }
        result
    }
}

/// The argument of a `/copy` command, which copies a response to this terminal's clipboard.
fn copy_command_argument(text: &str) -> Option<&str> {
    let (command, argument) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    command.eq_ignore_ascii_case("/copy").then_some(argument)
}

fn subagent_completion_prompt(id: AgentId) -> Submission {
    Submission::text(format!(
        "A subagent completed after the previous turn ended. Continue the current task by \
         inspecting its structured result. In code mode, include completed agents when calling \
         list_agents, find agent {id}, and expose only the result fields needed for the next step. \
         Integrate or verify them as appropriate, perform any remaining work, and then respond to \
         the user. Do not merely repeat the raw result.\n\n\
         <subagent_completion agent_id=\"{id}\" />"
    ))
}

impl Component for RootNode {
    type Event = RootEvent;
    type Effect = RootEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            RootEvent::Terminal(event) => self.update_terminal(event),
            RootEvent::PasteImage(data_url) => {
                if self.blocking_task.is_some() || self.overlay.is_some() || self.queue.focused() {
                    ComponentUpdate::none()
                } else {
                    self.update_composer(
                        ComposerEvent::PasteImage(data_url),
                        RenderRequest::Immediate,
                    )
                }
            }
            RootEvent::ContextBudget(budget) => {
                self.context_diagnostics.set_native_budget(budget);
                if let Some(Overlay::ContextDiagnostics(panel)) = &mut self.overlay {
                    panel.replace(self.context_diagnostics.clone());
                }
                self.update_composer(
                    ComposerEvent::ContextBudget(budget),
                    RenderRequest::Streaming,
                )
            }
            RootEvent::Transcript(record) => {
                if let Some(prompt) = recent_prompt(&record) {
                    if self.title.is_none() {
                        self.title = Some(prompt.text.clone());
                    }
                    self.recent_prompts.push(prompt);
                }
                let steer_applied = record.agent_kind() == Some(AgentEventKind::RunSteered);
                let turn_finished = matches!(
                    record.agent_kind(),
                    Some(AgentEventKind::RunCompleted | AgentEventKind::RunFailed)
                );
                let turn_timer = turn_timer_event(&record);
                let observation = self.context_diagnostics.observe(&record);
                if let Some(Overlay::ContextDiagnostics(panel)) = &mut self.overlay {
                    panel.replace(self.context_diagnostics.clone());
                }
                let mut update = self.update_transcript(TranscriptEvent::Record(record));
                if let Some(event) = turn_timer {
                    let timer = self.update_composer(event, RenderRequest::Streaming);
                    update.merge(timer);
                }
                if let Some(tokens) = observation.completed_tokens {
                    let context = self.update_composer(
                        ComposerEvent::ContextBudget(ContextBudget {
                            active_tokens: tokens,
                            window_tokens: self.context_diagnostics.model_window_tokens,
                        }),
                        RenderRequest::Streaming,
                    );
                    update.merge(context);
                }
                if steer_applied {
                    let applied = self.steer_applied();
                    update.merge(applied);
                }
                if turn_finished {
                    let finished = self.turn_ended(TurnEnd::Terminal);
                    update.merge(finished);
                }
                update
            }
            RootEvent::AgentStreamClosed => {
                let mut update = self.update_transcript(TranscriptEvent::AgentStreamClosed);
                let timer =
                    self.update_composer(ComposerEvent::TurnsCleared, RenderRequest::Immediate);
                update.merge(timer);
                update
            }
            RootEvent::Subagent(update) => self.apply_subagent_update(update),
            RootEvent::SubagentRecord { id, record } => {
                if self.subagents.apply_record(id, record) {
                    ComponentUpdate::render(RenderRequest::Immediate)
                } else {
                    ComponentUpdate::none()
                }
            }
            RootEvent::ReplaceDraft(draft) => {
                self.update_composer(ComposerEvent::ReplaceDraft(draft), RenderRequest::Immediate)
            }
            RootEvent::HandoffFinished(prompt) => {
                self.blocking_task = None;
                let waiting = self.update_composer(
                    ComposerEvent::TaskStatus {
                        status: None,
                        now: Instant::now(),
                    },
                    RenderRequest::Immediate,
                );
                let mut draft = self.update_composer(
                    ComposerEvent::ReplaceDraft(prompt),
                    RenderRequest::Immediate,
                );
                draft.merge(waiting);
                draft
            }
            RootEvent::HandoffCancelled => {
                self.blocking_task = None;
                self.notification = Some(Notification::plain(
                    "Handoff cancelled.".to_owned(),
                    Color::Yellow,
                ));
                self.update_composer(
                    ComposerEvent::TaskStatus {
                        status: None,
                        now: Instant::now(),
                    },
                    RenderRequest::Immediate,
                )
            }
            RootEvent::HandoffFailed(message) => {
                self.blocking_task = None;
                self.notification = Some(Notification::plain(message, Color::Red));
                self.update_composer(
                    ComposerEvent::TaskStatus {
                        status: None,
                        now: Instant::now(),
                    },
                    RenderRequest::Immediate,
                )
            }
            RootEvent::CompactionFinished => {
                self.blocking_task = None;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            RootEvent::WorkerTurnFinished { terminal_expected } => {
                self.turn_ended(TurnEnd::Worker { terminal_expected })
            }
            RootEvent::ShellFinished => {
                self.turns.finish_shell();
                ComponentUpdate::none()
            }
            RootEvent::TurnsCancelled => self.turns_cancelled(),
            RootEvent::ForkReady => self.fork_ready(),
            RootEvent::NewSessionFailed(message) => self.new_session_failed(message),
            RootEvent::SessionsLoaded(sessions) => self.sessions_loaded(sessions),
            RootEvent::RecentPromptsLoaded {
                session_id,
                prompts,
            } => self.recent_prompts_loaded(session_id, prompts),
            RootEvent::RecentPromptLoadFailed(message) => self.recent_prompt_load_failed(message),
            RootEvent::SessionLoadFailed(message) => self.session_load_failed(message),
            RootEvent::MemoriesLoaded { access, records } => {
                self.update_memory(MemoryBrowserEvent::Loaded { access, records })
            }
            RootEvent::MemoryLoadFailed {
                source,
                access,
                error,
            } => self.update_memory(MemoryBrowserEvent::LoadFailed {
                source,
                access,
                error,
            }),
            RootEvent::MemoryDeleted { key } => {
                self.update_memory(MemoryBrowserEvent::Deleted { key })
            }
            RootEvent::MemoryDeleteFailed { error, conflict } => {
                self.update_memory(MemoryBrowserEvent::DeleteFailed { error, conflict })
            }
            RootEvent::SessionRestored {
                projection,
                effort,
                reasoning_mode,
                preferred_reasoning_mode,
                speed,
                model,
                skills,
            } => {
                let workspace = self.workspace.clone();
                self.install_session_projection(
                    &workspace,
                    effort,
                    reasoning_mode,
                    preferred_reasoning_mode,
                    speed,
                    *projection,
                );
                self.set_model(model);
                self.set_skills(skills);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            RootEvent::EffortUpdateFailed { effort, error } => {
                self.set_effort(effort);
                self.notification = Some(Notification::plain(error, Color::Red));
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            RootEvent::NotifyError(message) => {
                self.notification = Some(Notification::plain(message, Color::Red));
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            RootEvent::NotifySuccess(message) => {
                self.notification = Some(Notification::plain(message, Color::Green));
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            RootEvent::ShowQrCode(link) => {
                match QrCodeView::new(&link) {
                    Ok(view) => self.overlay = Some(Overlay::QrCode(view)),
                    Err(error) => self.notification = Some(Notification::plain(error, Color::Red)),
                }
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            RootEvent::ConfirmWebInstall => {
                self.overlay = Some(Overlay::WebInstall(Confirmation::install_web_interface()));
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            RootEvent::UpdateAvailable(version) => {
                self.notification = Some(Notification::update_available(version));
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            RootEvent::SteerAdmitted(id) => self.steer_admitted(id),
            RootEvent::SteerPromoted(id) => self.steer_promoted(id),
            RootEvent::SteerFailed { id } => self.steer_failed(id),
            RootEvent::AnimationFrame(now) => self.update_animation(now),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.render_root(frame, area, theme, true);
    }
}

fn turn_timer_event(record: &TranscriptRecord) -> Option<ComposerEvent> {
    let kind = record.agent_kind()?;
    if kind == AgentEventKind::RunStarted {
        let now = Instant::now();
        let elapsed_ms = unix_time_ms().saturating_sub(record.recorded_at_unix_ms());
        return Some(ComposerEvent::TurnStarted {
            elapsed: Duration::from_millis(elapsed_ms),
            now,
        });
    }
    matches!(
        kind,
        AgentEventKind::RunCompleted | AgentEventKind::RunFailed
    )
    .then_some(ComposerEvent::TurnFinished)
}

fn recent_prompt(record: &TranscriptRecord) -> Option<RecentPromptDraft> {
    #[derive(serde::Deserialize)]
    struct UserPrompt {
        text: String,
    }

    if !matches!(
        record.local_kind(),
        Some(LocalKind::UserSubmitted | LocalKind::UserSteered)
    ) {
        return None;
    }
    let prompt = record.decode_payload::<UserPrompt>().ok()?;
    Some(RecentPromptDraft {
        text: prompt.text,
        recorded_at_unix_ms: record.recorded_at_unix_ms(),
    })
}

fn clamp_to(position: Position, area: Rect) -> Position {
    Position::new(
        position.x.clamp(area.x, area.right().saturating_sub(1)),
        position.y.clamp(area.y, area.bottom().saturating_sub(1)),
    )
}

#[cfg(test)]
mod tests;
