//! Root layout and component event routing.

use super::{
    actions::{Action, ActionAvailability, ActionsEffect, ActionsEvent, ActionsMenu},
    composer::{Composer, ComposerChromeTarget, ComposerDraft, ComposerEffect, ComposerEvent},
    context_diagnostics::{
        ContextDiagnosticsEffect, ContextDiagnosticsEvent, ContextDiagnosticsPanel,
    },
    effort::{EffortEffect, EffortEvent, EffortSelector},
    file_finder::{FileFinder, FileFinderEffect, FileFinderEvent},
    floating::Floating,
    keybindings::{KeybindingsEffect, KeybindingsEvent, KeybindingsHelp},
    memory::{MemoryBrowser, MemoryBrowserEffect, MemoryBrowserEvent},
    node::{Component, ComponentUpdate, Node, RenderRequest},
    queue::{MessageQueue, QueueEffect, QueueEvent, QueueId},
    review_confirmation::{
        ReviewConfirmationEffect, ReviewConfirmationEvent, ReviewDownloadConfirmation,
    },
    selection::{Selection, Surface, TextSpan},
    session_picker::{SessionPicker, SessionPickerEffect, SessionPickerEvent},
    skill_picker::{SkillPicker, SkillPickerEffect, SkillPickerEvent},
    subagents::{SubagentEffect, SubagentOverlay, SubagentTree},
    theme_selector::{ThemeSelector, ThemeSelectorEffect, ThemeSelectorEvent},
    transcript::{ScrollCommand, Transcript, TranscriptEvent},
};
use crate::{
    app::config::{ReasoningEffort, ReasoningMode},
    core::extensions::{
        Skill,
        memory::{MemoryKey, MemoryRecord},
        subagents::{AgentUpdate, MessageSender},
    },
    tui::{
        context::ContextDiagnostics,
        prompt::Submission,
        session::SessionSummary,
        theme::{Theme, ThemeMode},
        transcript::TranscriptRecord,
    },
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use semver::Version;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const KEY_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(2);
const SELECTION_SCROLL_INTERVAL: Duration = Duration::from_millis(60);
const BREADCRUMB_DURATION: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Eq, PartialEq)]
enum ConfirmationAction {
    Interrupt,
    CancelReview,
    Exit,
}

impl ConfirmationAction {
    const fn title_key(self) -> &'static str {
        match self {
            Self::Interrupt => "Esc",
            Self::CancelReview => "Esc",
            Self::Exit => "Ctrl+C",
        }
    }

    const fn action_label(self) -> &'static str {
        match self {
            Self::Interrupt => "Interrupt",
            Self::CancelReview => "Cancel review",
            Self::Exit => "Quit",
        }
    }

    const fn effect(self) -> RootEffect {
        match self {
            Self::Interrupt => RootEffect::CancelTurns,
            Self::CancelReview => RootEffect::CancelReview,
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
    #[cfg(test)]
    ContextTokens(u64),
    Transcript(Arc<TranscriptRecord>),
    AgentStreamClosed,
    Subagent(AgentUpdate),
    ReplaceDraft(String),
    ReviewStarted,
    ReviewReady(String),
    ReviewCancelled,
    ReviewFinished(String),
    ReviewFailed(String),
    RestoreQueued {
        index: usize,
        text: String,
    },
    WorkerTurnFinished,
    ShellFinished,
    TurnsCancelled,
    ForkReady,
    NewSessionFailed(String),
    SessionsLoaded(Vec<SessionSummary>),
    SessionLoadFailed(String),
    MemoriesLoaded(Vec<MemoryRecord>),
    MemoryLoadFailed(String),
    MemoryDeleted {
        id: i64,
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
        fast_mode: bool,
        skills: Arc<[Skill]>,
    },
    NotifyError(String),
    NotifySuccess(String),
    ConfirmReviewDownload,
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
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RootEffect {
    Submit(Submission),
    RunShell(String),
    OpenDraftEditor,
    OpenQueueEditor {
        index: usize,
        text: String,
    },
    OpenConfigEditor,
    OpenLink(String),
    ReloadConfig,
    NewSession,
    LoadSessions,
    LoadMemories,
    DeleteMemory(MemoryKey),
    ResumeSession(String),
    Steer {
        id: QueueId,
        prompt: Submission,
    },
    PersistSteer(String),
    Copy(String),
    Review {
        download_assets: bool,
    },
    SetEffort {
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
    },
    SetFastMode(bool),
    SetMaxSubagents(usize),
    SetTheme(ThemeMode),
    Fork,
    CancelTurns,
    CancelReview,
    Shutdown,
}

enum Overlay {
    Actions(Node<ActionsMenu>),
    ContextDiagnostics(Node<ContextDiagnosticsPanel>),
    Effort(Node<EffortSelector>),
    Theme(Node<ThemeSelector>),
    FileFinder(FileMention),
    Skills(SkillMention),
    Keybindings(Node<KeybindingsHelp>),
    Memory(Node<MemoryBrowser>),
    Sessions(Node<SessionPicker>),
    ReviewDownload(Node<ReviewDownloadConfirmation>),
    Subagents(SubagentOverlay),
}

struct FileMention {
    finder: Node<FileFinder>,
    start: usize,
}

struct SkillMention {
    picker: Node<SkillPicker>,
    start: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ThreadState {
    New,
    Started,
}

/// Owns layout and routing so future screen components do not widen the event loop.
pub(crate) struct RootNode {
    transcript: Node<Transcript>,
    composer: Node<Composer>,
    queue: Node<MessageQueue>,
    workspace: PathBuf,
    overlay: Option<Overlay>,
    thread: ThreadState,
    key_confirmation: Option<KeyConfirmation>,
    notification: Option<Notification>,
    discarded_draft: Option<ComposerDraft>,
    selection: Selection,
    selection_auto_scroll: Option<SelectionAutoScroll>,
    transcript_area: Rect,
    composer_area: Rect,
    composer_content_area: Rect,
    queue_area: Rect,
    in_flight_turns: usize,
    in_flight_shells: usize,
    review_active: bool,
    review_url: Option<String>,
    fork_available: bool,
    skills: Arc<[Skill]>,
    memory_enabled: bool,
    interactive: bool,
    theme_mode: ThemeMode,
    preferred_reasoning_mode: ReasoningMode,
    subagents: SubagentTree,
    context_diagnostics: ContextDiagnostics,
}

impl RootNode {
    pub(crate) fn new(workspace: &Path, thinking: ReasoningEffort) -> Self {
        Self {
            transcript: Node::new(Transcript::with_effort(thinking)),
            composer: Node::new(Composer::new(workspace, thinking)),
            queue: Node::new(MessageQueue::default()),
            workspace: workspace.to_path_buf(),
            overlay: None,
            thread: ThreadState::New,
            key_confirmation: None,
            notification: None,
            discarded_draft: None,
            selection: Selection::default(),
            selection_auto_scroll: None,
            transcript_area: Rect::default(),
            composer_area: Rect::default(),
            composer_content_area: Rect::default(),
            queue_area: Rect::default(),
            in_flight_turns: 0,
            in_flight_shells: 0,
            review_active: false,
            review_url: None,
            fork_available: true,
            skills: Arc::from([]),
            memory_enabled: false,
            interactive: true,
            theme_mode: ThemeMode::Auto,
            preferred_reasoning_mode: ReasoningMode::Standard,
            subagents: SubagentTree::new(thinking),
            context_diagnostics: ContextDiagnostics::default(),
        }
    }

    pub(crate) fn fork(&self, workspace: &Path, thinking: ReasoningEffort) -> Self {
        let mut root = Self::new(workspace, thinking);
        root.transcript = Node::new(self.transcript.component().fork_snapshot());
        root.composer
            .component_mut()
            .update(ComposerEvent::ContextTokens(
                self.composer.component().context_tokens(),
            ));
        root.set_fast_mode(self.composer.component().fast_mode());
        root.set_reasoning_modes(
            self.composer.component().reasoning_mode(),
            self.preferred_reasoning_mode,
        );
        root.set_max_subagents(self.subagents.max_subagents());
        root.thread = ThreadState::Started;
        root.fork_available = false;
        root.set_skills(Arc::clone(&self.skills));
        root.memory_enabled = self.memory_enabled;
        root.theme_mode = self.theme_mode;
        root.context_diagnostics = self.context_diagnostics.clone();
        root.interactive = false;
        root.composer
            .component_mut()
            .update(ComposerEvent::Activity {
                active: true,
                status: Some("Forking session…".to_owned()),
                now: Instant::now(),
            });
        root
    }

    pub(crate) fn set_fork_available(&mut self, available: bool) {
        self.fork_available = available;
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

    pub(crate) fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.theme_mode = mode;
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        let _ = self
            .composer
            .component_mut()
            .update(ComposerEvent::SetFastMode(enabled));
    }

    pub(crate) fn set_reasoning_modes(&mut self, actual: ReasoningMode, preferred: ReasoningMode) {
        self.preferred_reasoning_mode = preferred;
        let _ = self
            .composer
            .component_mut()
            .update(ComposerEvent::SetReasoningMode(actual));
    }

    pub(crate) const fn set_preferred_reasoning_mode(&mut self, mode: ReasoningMode) {
        self.preferred_reasoning_mode = mode;
    }

    #[cfg(test)]
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
    ) {
        let current_draft = self.composer.component_mut().take_draft();
        let replaced_draft = current_draft.is_some();
        let discarded_draft = current_draft.or_else(|| self.discarded_draft.take());
        let fork_available = self.fork_available;
        let memory_enabled = self.memory_enabled;
        let theme_mode = self.theme_mode;
        let max_subagents = self.subagents.max_subagents();
        *self = Self::new(workspace, thinking);
        self.set_reasoning_modes(reasoning_mode, preferred_reasoning_mode);
        self.discarded_draft = discarded_draft;
        self.fork_available = fork_available;
        self.memory_enabled = memory_enabled;
        self.theme_mode = theme_mode;
        self.set_max_subagents(max_subagents);
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
        fast_mode: bool,
        records: Vec<Arc<TranscriptRecord>>,
    ) {
        let projection = Self::project_session(thinking, records);
        self.install_session_projection(
            workspace,
            thinking,
            reasoning_mode,
            preferred_reasoning_mode,
            fast_mode,
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
        for record in records {
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
        }
    }

    pub(crate) fn install_session_projection(
        &mut self,
        workspace: &Path,
        thinking: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        preferred_reasoning_mode: ReasoningMode,
        fast_mode: bool,
        projection: RestoredSessionProjection,
    ) {
        self.reset_session(
            workspace,
            thinking,
            reasoning_mode,
            preferred_reasoning_mode,
        );
        self.set_fast_mode(fast_mode);
        self.transcript = Node::new(projection.transcript);
        self.context_diagnostics = projection.context_diagnostics;
        if let Some(tokens) = projection.context_tokens {
            let _ = self
                .composer
                .component_mut()
                .update(ComposerEvent::ContextTokens(tokens));
        }
        self.thread = ThreadState::Started;
    }

    pub(crate) const fn composer(&self) -> &Composer {
        self.composer.component()
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
        let effort = match &self.overlay {
            Some(Overlay::Effort(selector)) => selector.component().animation_deadline(),
            _ => None,
        };
        [
            effort,
            self.transcript.component().animation_deadline(),
            self.composer.component().animation_deadline(),
            self.queue.component().animation_deadline(),
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

    fn render_root(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, focused: bool) {
        let height = self
            .composer
            .component_mut()
            .desired_height(area.width)
            .min(area.height);
        let composer_area = Rect {
            y: area.bottom().saturating_sub(height),
            height,
            ..area
        };
        self.composer_area = composer_area;
        let queue_height = self
            .queue
            .component()
            .desired_height()
            .min(area.height.saturating_sub(height));
        let queue_width = area.width.saturating_mul(95) / 100;
        let queue_area = Rect {
            x: area.x + area.width.saturating_sub(queue_width) / 2,
            y: composer_area.y.saturating_sub(queue_height),
            width: queue_width,
            height: queue_height,
        };
        self.queue_area = queue_area;
        let transcript_area = Rect {
            height: area
                .height
                .saturating_sub(height)
                .saturating_sub(queue_height),
            ..area
        };
        self.transcript_area = transcript_area;
        self.composer_content_area = if composer_area.width >= 2 && composer_area.height >= 3 {
            Rect::new(
                composer_area.x + 1,
                composer_area.y + 1,
                composer_area.width - 2,
                composer_area.height - 2,
            )
        } else {
            Rect {
                height: composer_area.height.min(1),
                ..composer_area
            }
        };
        self.transcript.render(frame, transcript_area, theme);
        self.queue.render(frame, queue_area, theme);
        let composer_selection = (self.selection.surface() == Some(Surface::Composer))
            .then(|| self.selection.range())
            .flatten();
        self.composer.component_mut().render_focused_with_selection(
            frame,
            composer_area,
            theme,
            focused
                && !self.review_active
                && !self.transcript.component().expandables_focused()
                && !self.queue.component().focused(),
            composer_selection,
        );
        if self.selection.surface() == Some(Surface::Transcript)
            && let Some(range) = self.selection.range()
        {
            self.transcript
                .component()
                .render_selection(frame.buffer_mut(), range);
        }
        self.transcript
            .component()
            .render_chrome(frame, transcript_area, theme);
        if let Some(overlay) = &mut self.overlay {
            match overlay {
                Overlay::Actions(actions) => actions.render(frame, area, theme),
                Overlay::ContextDiagnostics(panel) => panel.render(frame, area, theme),
                Overlay::Effort(selector) => selector.render(frame, area, theme),
                Overlay::Theme(selector) => selector.render(frame, area, theme),
                Overlay::FileFinder(mention) => mention.finder.render(frame, area, theme),
                Overlay::Skills(mention) => mention.picker.render(frame, area, theme),
                Overlay::Keybindings(help) => help.render(frame, area, theme),
                Overlay::Memory(browser) => browser.render(frame, area, theme),
                Overlay::Sessions(picker) => picker.render(frame, area, theme),
                Overlay::ReviewDownload(confirmation) => {
                    confirmation.render(frame, area, theme);
                }
                Overlay::Subagents(SubagentOverlay::Tree) => {
                    self.subagents.render_tree(frame, area, theme);
                }
                Overlay::Subagents(SubagentOverlay::Transcript(id)) => {
                    self.subagents.render_transcript(*id, frame, area, theme);
                }
            }
        }
        if let Some(notification) = &self.notification {
            render_notification(
                frame,
                area,
                theme,
                &notification.message,
                notification.color,
            );
        }
        if let Some(confirmation) = &self.key_confirmation {
            render_key_confirmation(frame, area, composer_area, theme, confirmation.action);
        }
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
        if self.review_active && is_control_c(&event) {
            return self.update_key_confirmation(ConfirmationAction::Exit, Instant::now());
        }
        if self.review_active {
            return self.update_review_input(event);
        }
        if is_control_c(&event) {
            if self.overlay.is_none()
                && !self.queue.component().focused()
                && !self.transcript.component().expandables_focused()
                && !self.composer.component().draft().is_empty()
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

    fn update_terminal_without_confirmation(
        &mut self,
        mut event: Event,
    ) -> ComponentUpdate<RootEffect> {
        if !self.interactive {
            return ComponentUpdate::none();
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
            && !self.queue.component().focused()
            && !self.transcript.component().expandables_focused()
        {
            return self.restore_discarded_draft();
        }
        if is_control_key(&event, 'o') {
            return self.update_transcript(TranscriptEvent::ToggleExpandAll);
        }
        if is_control_key(&event, 's') {
            return self.open_effort();
        }
        if is_control_key(&event, 'f') {
            return self.open_fork();
        }
        if is_escape(&event) {
            if self.selection.clear() {
                self.selection_auto_scroll = None;
                self.key_confirmation = None;
                return ComponentUpdate::render(RenderRequest::Immediate);
            }
            if self.queue.component().focused() {
                self.key_confirmation = None;
                return self.update_queue(event);
            }
            if self.transcript.component().expandables_focused() {
                self.key_confirmation = None;
                return self.update_transcript(TranscriptEvent::BlurExpandables);
            }
            return self.update_key_confirmation(ConfirmationAction::Interrupt, Instant::now());
        }
        if self.transcript.component().pinned_prompt_clicked(&event) {
            return self.update_transcript(TranscriptEvent::JumpToPinnedPrompt);
        }
        if let Some(update) = self.update_selection_mouse(&mut event) {
            return update;
        }
        if let Some(destination) = self.transcript.component().link_destination(&event) {
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
            match self.composer.component().chrome_target(position) {
                Some(ComposerChromeTarget::Effort) => return self.open_effort(),
                Some(ComposerChromeTarget::Subagents) => {
                    self.overlay = Some(Overlay::Subagents(SubagentOverlay::Tree));
                    return ComponentUpdate::render(RenderRequest::Immediate);
                }
                None => {}
            }
        }
        if is_focus_toggle(&event) {
            return self.update_focus();
        }
        if is_left_click_in(&event, self.queue_area) {
            let Event::Mouse(mouse) = &event else {
                unreachable!("left click helper only accepts mouse events");
            };
            let _ = self
                .queue
                .component_mut()
                .focus_row(mouse.row, self.queue_area);
            let _ = self
                .transcript
                .component_mut()
                .update(TranscriptEvent::BlurExpandables);
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if is_left_click_in(&event, self.composer_area) {
            self.focus_composer();
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if let Some(command) = self.transcript.component().expandable_command(&event) {
            self.queue.component_mut().set_focused(false);
            return self.update_transcript(TranscriptEvent::Expandable(command));
        }
        if is_left_click(&event) {
            self.focus_composer();
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if self.queue.component().focused() {
            return self.update_queue(event);
        }
        if self.in_flight_turns > 0
            && self.composer.component().draft().is_empty()
            && !self.queue.component().is_empty()
            && !self.queue.component().has_pending_steer()
            && is_plain_enter(&event)
        {
            return self.update_queue(event);
        }
        if !self.skills.is_empty()
            && !self.composer.component().draft().starts_with('!')
            && is_skill_picker_trigger(&event)
            && self.composer.component().cursor_is_at_token_boundary()
        {
            let start = self.composer.component().cursor();
            let update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            self.overlay = Some(Overlay::Skills(SkillMention {
                picker: Node::new(SkillPicker::new(Arc::clone(&self.skills))),
                start,
            }));
            return update;
        }
        if is_file_finder_trigger(&event) && self.composer.component().cursor_is_at_token_boundary()
        {
            let start = self.composer.component().cursor();
            let update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            self.overlay = Some(Overlay::FileFinder(FileMention {
                finder: Node::new(FileFinder::new(&self.workspace)),
                start,
            }));
            return update;
        }
        if self.composer.component().draft().is_empty() && is_actions_trigger(&event) {
            let new_session_enabled = self.in_flight_turns == 0
                && self.in_flight_shells == 0
                && !self.review_active
                && self.queue.component().is_empty();
            self.overlay = Some(Overlay::Actions(Node::new(ActionsMenu::new(
                ActionAvailability {
                    new_session: new_session_enabled,
                    fork: self.fork_available,
                    fast_mode: self.composer.component().fast_mode(),
                    memory: self.memory_enabled,
                },
            ))));
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if let Some(command) = self.transcript.component().scroll_command(&event) {
            let transcript = self.transcript.update(TranscriptEvent::Scroll(command));
            return ComponentUpdate {
                effects: Vec::new(),
                render: transcript.render,
            };
        }
        if self.transcript.component().expandables_focused() {
            return ComponentUpdate::none();
        }
        self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate)
    }

    fn update_review_input(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        if is_control_key(&event, 'f') {
            self.key_confirmation = None;
            return self.open_fork();
        }
        if is_plain_key(&event, 'o')
            && let Some(url) = &self.review_url
        {
            self.key_confirmation = None;
            return ComponentUpdate {
                effects: vec![RootEffect::OpenLink(url.clone())],
                render: RenderRequest::None,
            };
        }
        if is_plain_key(&event, 'c')
            && let Some(url) = &self.review_url
        {
            self.key_confirmation = None;
            return ComponentUpdate {
                effects: vec![RootEffect::Copy(url.clone())],
                render: RenderRequest::None,
            };
        }
        if is_escape(&event) {
            return self.update_key_confirmation(ConfirmationAction::CancelReview, Instant::now());
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
                            .component_mut()
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
                    Surface::Transcript => self.transcript.component().selection_text(range),
                    Surface::Composer => self.composer.component().selection_text(range),
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
                .component_mut()
                .selection_span(position, self.composer_content_area)?;
            return Some((Surface::Composer, span));
        }
        if !self.transcript_area.contains(position) {
            return None;
        }
        let span = self.transcript.component().selection_span(position)?;
        Some((Surface::Transcript, span))
    }

    fn selection_span_on(&mut self, surface: Surface, position: Position) -> Option<TextSpan> {
        match surface {
            Surface::Transcript => {
                let position = clamp_to(position, self.transcript_area);
                self.transcript.component().selection_span_nearest(position)
            }
            Surface::Composer => {
                let position = clamp_to(position, self.composer_content_area);
                self.composer
                    .component_mut()
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
                .component_mut()
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
            Some(Overlay::Theme(_)) => {
                self.update_theme_selector(ThemeSelectorEvent::Terminal(event))
            }
            Some(Overlay::FileFinder(_)) => self.update_file_finder(event),
            Some(Overlay::Skills(_)) => self.update_skill_picker(event),
            Some(Overlay::Keybindings(_)) => self.update_keybindings(event),
            Some(Overlay::Memory(_)) => self.update_memory(MemoryBrowserEvent::Terminal(event)),
            Some(Overlay::Sessions(_)) => self.update_session_picker(event),
            Some(Overlay::ReviewDownload(_)) => self.update_review_confirmation(event),
            Some(Overlay::Subagents(SubagentOverlay::Tree)) => {
                let effect = self.subagents.update_tree(event);
                self.apply_subagent_effect(effect)
            }
            Some(Overlay::Subagents(SubagentOverlay::Transcript(id))) => {
                let effect = self.subagents.update_transcript(*id, event);
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
                    range: start..self.composer.component().cursor(),
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
                    range: start..self.composer.component().cursor(),
                    text: format!("${name} "),
                },
                RenderRequest::Immediate,
            ),
        }
    }

    fn mention_query(&self, start: usize, prefix: char) -> Option<String> {
        let composer = self.composer.component();
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
            Some(ActionsEffect::Trigger(Action::Subagents)) => {
                self.overlay = Some(Overlay::Subagents(SubagentOverlay::Tree));
            }
            Some(ActionsEffect::Trigger(Action::Effort)) => {
                return self.open_effort();
            }
            Some(ActionsEffect::Trigger(Action::FastMode)) => {
                self.overlay = None;
                let enabled = !self.composer.component().fast_mode();
                self.set_fast_mode(enabled);
                return ComponentUpdate {
                    effects: vec![RootEffect::SetFastMode(enabled)],
                    render: RenderRequest::Immediate,
                };
            }
            Some(ActionsEffect::Trigger(Action::Theme)) => {
                self.overlay = Some(Overlay::Theme(Node::new(ThemeSelector::new(
                    self.theme_mode,
                ))));
            }
            Some(ActionsEffect::Trigger(Action::NewSession)) => {
                return self.open_new_session();
            }
            Some(ActionsEffect::Trigger(Action::ResumeSession)) => {
                return self.load_sessions();
            }
            Some(ActionsEffect::Trigger(Action::Fork)) => return self.open_fork(),
            Some(ActionsEffect::Trigger(Action::Keybindings)) => {
                self.overlay = Some(Overlay::Keybindings(Node::new(KeybindingsHelp)));
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
                self.overlay = Some(Overlay::Memory(Node::new(MemoryBrowser::new())));
                return ComponentUpdate {
                    effects: vec![RootEffect::LoadMemories],
                    render: RenderRequest::Immediate,
                };
            }
            Some(ActionsEffect::Trigger(Action::DebugContext)) => {
                self.overlay = Some(Overlay::ContextDiagnostics(Node::new(
                    ContextDiagnosticsPanel::new(self.context_diagnostics.clone()),
                )));
            }
            Some(ActionsEffect::Trigger(Action::Review)) => {
                self.overlay = None;
                return ComponentUpdate {
                    effects: vec![RootEffect::Review {
                        download_assets: false,
                    }],
                    render: RenderRequest::Immediate,
                };
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
                    panel
                        .component_mut()
                        .replace(self.context_diagnostics.clone());
                }
            }
            None => {}
        }
        ComponentUpdate {
            effects: Vec::new(),
            render: update.render,
        }
    }

    fn update_review_confirmation(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let Some(Overlay::ReviewDownload(confirmation)) = &mut self.overlay else {
            return ComponentUpdate::none();
        };
        let update = confirmation.update(ReviewConfirmationEvent::Terminal(event));
        let Some(effect) = update.effects.into_iter().next() else {
            return ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            };
        };
        self.overlay = None;
        match effect {
            ReviewConfirmationEffect::Confirm => ComponentUpdate {
                effects: vec![RootEffect::Review {
                    download_assets: true,
                }],
                render: RenderRequest::Immediate,
            },
            ReviewConfirmationEffect::Dismiss => ComponentUpdate::render(RenderRequest::Immediate),
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

    fn open_effort(&mut self) -> ComponentUpdate<RootEffect> {
        self.overlay = Some(Overlay::Effort(Node::new(EffortSelector::new(
            self.composer.component().effort(),
            self.preferred_reasoning_mode == ReasoningMode::Pro,
        ))));
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
        if !self.fork_available {
            return ComponentUpdate::none();
        }
        self.overlay = None;
        ComponentUpdate {
            effects: vec![RootEffect::Fork],
            render: RenderRequest::Immediate,
        }
    }

    fn open_new_session(&mut self) -> ComponentUpdate<RootEffect> {
        if self.in_flight_turns > 0
            || self.in_flight_shells > 0
            || !self.queue.component().is_empty()
        {
            return ComponentUpdate::none();
        }
        self.overlay = None;
        self.interactive = false;
        let _ = self
            .composer
            .component_mut()
            .update(ComposerEvent::Activity {
                active: true,
                status: Some("Starting new session…".to_owned()),
                now: Instant::now(),
            });
        ComponentUpdate {
            effects: vec![RootEffect::NewSession],
            render: RenderRequest::Immediate,
        }
    }

    pub(super) fn load_sessions(&mut self) -> ComponentUpdate<RootEffect> {
        self.overlay = None;
        self.interactive = false;
        let _ = self
            .composer
            .component_mut()
            .update(ComposerEvent::Activity {
                active: true,
                status: Some("Loading sessions…".to_owned()),
                now: Instant::now(),
            });
        ComponentUpdate {
            effects: vec![RootEffect::LoadSessions],
            render: RenderRequest::Immediate,
        }
    }

    fn sessions_loaded(&mut self, sessions: Vec<SessionSummary>) -> ComponentUpdate<RootEffect> {
        self.interactive = true;
        let _ = self
            .composer
            .component_mut()
            .update(ComposerEvent::Activity {
                active: false,
                status: None,
                now: Instant::now(),
            });
        self.overlay = Some(Overlay::Sessions(Node::new(SessionPicker::new(sessions))));
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
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            Some(SessionPickerEffect::Resume(session_id)) => {
                self.overlay = None;
                self.interactive = false;
                let _ = self
                    .composer
                    .component_mut()
                    .update(ComposerEvent::Activity {
                        active: true,
                        status: Some("Resuming session…".to_owned()),
                        now: Instant::now(),
                    });
                ComponentUpdate {
                    effects: vec![RootEffect::ResumeSession(session_id)],
                    render: RenderRequest::Immediate,
                }
            }
            None => ComponentUpdate {
                effects: Vec::new(),
                render: update.render,
            },
        }
    }

    fn session_load_failed(&mut self, message: String) -> ComponentUpdate<RootEffect> {
        self.interactive = true;
        let _ = self
            .composer
            .component_mut()
            .update(ComposerEvent::Activity {
                active: false,
                status: None,
                now: Instant::now(),
            });
        self.notification = Some(Notification::plain(message, Color::Red));
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn new_session_failed(&mut self, message: String) -> ComponentUpdate<RootEffect> {
        self.interactive = true;
        let _ = self
            .composer
            .component_mut()
            .update(ComposerEvent::Activity {
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
        let update = self
            .composer
            .component_mut()
            .update(ComposerEvent::Activity {
                active: false,
                status: None,
                now: Instant::now(),
            });
        debug_assert!(update.changed);
        ComponentUpdate::render(RenderRequest::Immediate)
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
            EffortEffect::Apply(effort, pro) => {
                let reasoning_mode = if pro {
                    ReasoningMode::Pro
                } else {
                    ReasoningMode::Standard
                };
                let previous_reasoning_mode = self.preferred_reasoning_mode;
                self.preferred_reasoning_mode = reasoning_mode;
                if reasoning_mode != previous_reasoning_mode {
                    let state = if pro { "enabled" } else { "disabled" };
                    let suffix = if self.composer.component().reasoning_mode() != reasoning_mode {
                        " · start a new session to apply."
                    } else {
                        "."
                    };
                    let message = format!("Pro {state} for new sessions{suffix}");
                    self.notification = Some(Notification::plain(message, Color::Green));
                }
                self.transcript.component_mut().set_effort(effort);
                self.subagents.set_effort(effort);
                let _ = self
                    .composer
                    .component_mut()
                    .update(ComposerEvent::SetEffort(effort));
                ComponentUpdate {
                    effects: vec![RootEffect::SetEffort {
                        effort,
                        reasoning_mode,
                    }],
                    render: RenderRequest::Immediate,
                }
            }
        }
    }

    fn update_focus(&mut self) -> ComponentUpdate<RootEffect> {
        let focus_queue = !self.queue.component().focused() && !self.queue.component().is_empty();
        self.queue.component_mut().set_focused(focus_queue);
        let transcript = self.transcript.update(TranscriptEvent::BlurExpandables);
        ComponentUpdate::render(if focus_queue || transcript.render != RenderRequest::None {
            RenderRequest::Immediate
        } else {
            RenderRequest::None
        })
    }

    fn focus_composer(&mut self) {
        self.queue.component_mut().set_focused(false);
        let _ = self
            .transcript
            .component_mut()
            .update(TranscriptEvent::BlurExpandables);
    }

    fn update_queue(&mut self, event: Event) -> ComponentUpdate<RootEffect> {
        let update = self.queue.update(QueueEvent::Terminal(event));
        let mut effects = Vec::new();
        for effect in update.effects {
            match effect {
                QueueEffect::Blur => {}
                QueueEffect::Edit { index, text } => {
                    effects.push(RootEffect::OpenQueueEditor { index, text });
                }
                QueueEffect::Steer { id, prompt } => {
                    effects.push(RootEffect::Steer { id, prompt });
                }
            }
        }
        ComponentUpdate {
            effects,
            render: update.render,
        }
    }

    fn update_composer(
        &mut self,
        event: ComposerEvent,
        priority: RenderRequest,
    ) -> ComponentUpdate<RootEffect> {
        let update = self.composer.component_mut().update(event);
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
                if self.in_flight_turns > 0 || self.queue.component().has_pending_steer() =>
            {
                self.queue.component_mut().push(prompt);
                Vec::new()
            }
            Some(ComposerEffect::Submit(prompt)) => {
                self.in_flight_turns = self.in_flight_turns.saturating_add(1);
                vec![RootEffect::Submit(prompt)]
            }
            Some(ComposerEffect::RunShell(command)) => {
                self.in_flight_shells = self.in_flight_shells.saturating_add(1);
                vec![RootEffect::RunShell(command)]
            }
            Some(ComposerEffect::OpenDraftEditor) => vec![RootEffect::OpenDraftEditor],
            None => Vec::new(),
        };

        ComponentUpdate { effects, render }
    }

    fn discard_draft(&mut self) -> ComponentUpdate<RootEffect> {
        let Some(draft) = self.composer.component_mut().take_draft() else {
            return ComponentUpdate::none();
        };
        self.discarded_draft = Some(draft);
        self.show_draft_saved();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn restore_discarded_draft(&mut self) -> ComponentUpdate<RootEffect> {
        if !self.composer.component().draft().is_empty() {
            return ComponentUpdate::none();
        }
        let Some(draft) = self.discarded_draft.take() else {
            return ComponentUpdate::none();
        };
        self.composer.component_mut().restore_draft(draft);
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

    fn turn_finished(&mut self) -> ComponentUpdate<RootEffect> {
        self.in_flight_turns = self.in_flight_turns.saturating_sub(1);
        self.submit_next_queued()
    }

    fn turns_cancelled(&mut self) -> ComponentUpdate<RootEffect> {
        self.queue.component_mut().cancel_steers();
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn steer_admitted(&mut self, id: QueueId) -> ComponentUpdate<RootEffect> {
        let applied = self.queue.component_mut().steer_admitted(id);
        self.finish_applied_steer(applied)
    }

    fn steer_promoted(&mut self, id: QueueId) -> ComponentUpdate<RootEffect> {
        let _ = self.queue.component_mut().steer_promoted(id);
        self.in_flight_turns = self.in_flight_turns.saturating_add(1);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn steer_failed(&mut self, id: QueueId) -> ComponentUpdate<RootEffect> {
        self.queue.component_mut().steer_failed(id);
        self.submit_next_queued()
    }

    fn steer_applied(&mut self) -> ComponentUpdate<RootEffect> {
        let applied = self.queue.component_mut().steer_applied();
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

    fn restore_queued(&mut self, index: usize, text: String) -> ComponentUpdate<RootEffect> {
        if text.trim().is_empty() {
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if self.in_flight_turns == 0 && !self.queue.component().has_pending_steer() {
            self.in_flight_turns = 1;
            return ComponentUpdate {
                effects: vec![RootEffect::Submit(text.into())],
                render: RenderRequest::Immediate,
            };
        }
        self.queue.component_mut().restore(index, text);
        ComponentUpdate::render(RenderRequest::Immediate)
    }

    fn submit_next_queued(&mut self) -> ComponentUpdate<RootEffect> {
        if self.in_flight_turns > 0 || self.queue.component().has_pending_steer() {
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        let prompts = self.queue.component_mut().drain_ready();
        if prompts.is_empty() {
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        self.in_flight_turns = 1;
        ComponentUpdate {
            effects: vec![RootEffect::Submit(Submission::join(prompts))],
            render: RenderRequest::Immediate,
        }
    }

    fn update_transcript(&mut self, event: TranscriptEvent) -> ComponentUpdate<RootEffect> {
        let update = self.transcript.update(event);
        let mut render = update.render;
        for effect in update.effects {
            let composer = self
                .composer
                .component_mut()
                .update(ComposerEvent::Activity {
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
            effects: effort.effects.into_iter().chain(composer.effects).collect(),
            render: effort
                .render
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
            let _ = self
                .composer
                .component_mut()
                .update(ComposerEvent::ActiveSubagents {
                    count: active,
                    now: Instant::now(),
                });
        }
        if subagents_changed {
            result.render = result.render.max(RenderRequest::Immediate);
        }
        result
    }
}

impl Component for RootNode {
    type Event = RootEvent;
    type Effect = RootEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            RootEvent::Terminal(event) => self.update_terminal(event),
            RootEvent::PasteImage(data_url) => {
                if self.review_active || self.overlay.is_some() || self.queue.component().focused()
                {
                    ComponentUpdate::none()
                } else {
                    self.update_composer(
                        ComposerEvent::PasteImage(data_url),
                        RenderRequest::Immediate,
                    )
                }
            }
            #[cfg(test)]
            RootEvent::ContextTokens(tokens) => self.update_composer(
                ComposerEvent::ContextTokens(tokens),
                RenderRequest::Streaming,
            ),
            RootEvent::Transcript(record) => {
                let steer_applied = record.kind() == "run.steered";
                let turn_timer = turn_timer_event(&record);
                let observation = self.context_diagnostics.observe(&record);
                if let Some(Overlay::ContextDiagnostics(panel)) = &mut self.overlay {
                    panel
                        .component_mut()
                        .replace(self.context_diagnostics.clone());
                }
                let mut update = self.update_transcript(TranscriptEvent::Record(record));
                if let Some(event) = turn_timer {
                    let timer = self.update_composer(event, RenderRequest::Streaming);
                    update.effects.extend(timer.effects);
                    update.render = update.render.max(timer.render);
                }
                if let Some(tokens) = observation.completed_tokens {
                    let context = self.update_composer(
                        ComposerEvent::ContextTokens(tokens),
                        RenderRequest::Streaming,
                    );
                    update.effects.extend(context.effects);
                    update.render = update.render.max(context.render);
                }
                if steer_applied {
                    let applied = self.steer_applied();
                    update.effects.extend(applied.effects);
                    update.render = update.render.max(applied.render);
                }
                update
            }
            RootEvent::AgentStreamClosed => {
                let mut update = self.update_transcript(TranscriptEvent::AgentStreamClosed);
                let timer =
                    self.update_composer(ComposerEvent::TurnsCleared, RenderRequest::Immediate);
                update.effects.extend(timer.effects);
                update.render = update.render.max(timer.render);
                update
            }
            RootEvent::Subagent(update) => self.apply_subagent_update(update),
            RootEvent::ReplaceDraft(draft) => {
                self.update_composer(ComposerEvent::ReplaceDraft(draft), RenderRequest::Immediate)
            }
            RootEvent::ReviewStarted => {
                self.review_active = true;
                self.review_url = None;
                self.update_composer(
                    ComposerEvent::ReviewWaiting {
                        waiting: true,
                        status: None,
                        now: Instant::now(),
                    },
                    RenderRequest::Immediate,
                )
            }
            RootEvent::ReviewReady(url) => {
                self.review_url = Some(url.clone());
                self.update_composer(
                    ComposerEvent::ReviewWaiting {
                        waiting: true,
                        status: Some("Review ready · O reopen · C copy link".to_owned()),
                        now: Instant::now(),
                    },
                    RenderRequest::Immediate,
                )
            }
            RootEvent::ReviewFinished(markdown) => {
                self.review_active = false;
                self.review_url = None;
                let waiting = self.update_composer(
                    ComposerEvent::ReviewWaiting {
                        waiting: false,
                        status: None,
                        now: Instant::now(),
                    },
                    RenderRequest::Immediate,
                );
                let cursor = self.composer.component().cursor();
                let draft = self.composer.component().draft();
                let before = if draft[..cursor].is_empty() {
                    ""
                } else {
                    "\n\n"
                };
                let after = if draft[cursor..].is_empty() {
                    ""
                } else {
                    "\n\n"
                };
                let mut update = self.update_composer(
                    ComposerEvent::ReplaceRange {
                        range: cursor..cursor,
                        text: format!("{before}{markdown}{after}"),
                    },
                    RenderRequest::Immediate,
                );
                update.effects.extend(waiting.effects);
                update.render = update.render.max(waiting.render);
                update
            }
            RootEvent::ReviewCancelled => {
                self.review_active = false;
                self.review_url = None;
                self.notification = Some(Notification::plain(
                    "Review cancelled.".to_owned(),
                    Color::Yellow,
                ));
                self.update_composer(
                    ComposerEvent::ReviewWaiting {
                        waiting: false,
                        status: None,
                        now: Instant::now(),
                    },
                    RenderRequest::Immediate,
                )
            }
            RootEvent::ReviewFailed(message) => {
                self.review_active = false;
                self.review_url = None;
                self.notification = Some(Notification::plain(message, Color::Red));
                self.update_composer(
                    ComposerEvent::ReviewWaiting {
                        waiting: false,
                        status: None,
                        now: Instant::now(),
                    },
                    RenderRequest::Immediate,
                )
            }
            RootEvent::RestoreQueued { index, text } => self.restore_queued(index, text),
            RootEvent::WorkerTurnFinished => self.turn_finished(),
            RootEvent::ShellFinished => {
                self.in_flight_shells = self.in_flight_shells.saturating_sub(1);
                ComponentUpdate::none()
            }
            RootEvent::TurnsCancelled => self.turns_cancelled(),
            RootEvent::ForkReady => self.fork_ready(),
            RootEvent::NewSessionFailed(message) => self.new_session_failed(message),
            RootEvent::SessionsLoaded(sessions) => self.sessions_loaded(sessions),
            RootEvent::SessionLoadFailed(message) => self.session_load_failed(message),
            RootEvent::MemoriesLoaded(records) => {
                self.update_memory(MemoryBrowserEvent::Loaded(records))
            }
            RootEvent::MemoryLoadFailed(message) => {
                self.update_memory(MemoryBrowserEvent::LoadFailed(message))
            }
            RootEvent::MemoryDeleted { id } => {
                self.update_memory(MemoryBrowserEvent::Deleted { id })
            }
            RootEvent::MemoryDeleteFailed { error, conflict } => {
                self.update_memory(MemoryBrowserEvent::DeleteFailed { error, conflict })
            }
            RootEvent::SessionRestored {
                projection,
                effort,
                reasoning_mode,
                preferred_reasoning_mode,
                fast_mode,
                skills,
            } => {
                let workspace = self.workspace.clone();
                self.install_session_projection(
                    &workspace,
                    effort,
                    reasoning_mode,
                    preferred_reasoning_mode,
                    fast_mode,
                    *projection,
                );
                self.set_skills(skills);
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
            RootEvent::ConfirmReviewDownload => {
                self.overlay = Some(Overlay::ReviewDownload(Node::new(
                    ReviewDownloadConfirmation,
                )));
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
    if record.source() != "agent" {
        return None;
    }
    if record.kind() == "run.started" {
        let now = Instant::now();
        let now_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let elapsed_ms = u64::try_from(now_unix_ms)
            .unwrap_or(u64::MAX)
            .saturating_sub(record.recorded_at_unix_ms());
        return Some(ComposerEvent::TurnStarted {
            elapsed: Duration::from_millis(elapsed_ms),
            now,
        });
    }
    matches!(record.kind(), "run.completed" | "run.failed").then_some(ComposerEvent::TurnFinished)
}

fn render_notification(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
    message: &Line<'_>,
    color: Color,
) {
    if area.is_empty() {
        return;
    }
    let text_width = message.width();
    let width = u16::try_from(text_width.saturating_add(4)).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(message.clone())
        .centered()
        .wrap(Wrap { trim: true });
    let body_width = width.min(area.width).saturating_sub(2).max(1);
    let body_height = u16::try_from(text_width.div_ceil(usize::from(body_width)))
        .unwrap_or(u16::MAX)
        .max(1);
    let popup = Floating::new("", width, body_height.saturating_add(2), &[])
        .at_top()
        .colors(color, color)
        .render(frame, area, theme);
    frame.render_widget(paragraph, popup.body);
}

fn render_key_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    composer_area: Rect,
    theme: &Theme,
    action: ConfirmationAction,
) {
    const HEIGHT: u16 = 4;
    const WIDTH: u16 = 28;

    let available_height = composer_area.y.saturating_sub(area.y);
    if available_height < HEIGHT {
        return;
    }

    let width = WIDTH.min(composer_area.width).min(area.width);
    let gap = u16::from(available_height > HEIGHT);
    let popup = Rect {
        x: composer_area.right().saturating_sub(width).max(area.x),
        y: composer_area.y.saturating_sub(HEIGHT + gap),
        width,
        height: HEIGHT,
    };
    let title = Line::from(vec![
        Span::styled(
            format!(" {} ", action.title_key()),
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("then ", Style::default().fg(theme.muted())),
    ]);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border()))
        .title(title);
    let body = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(vec![
            confirmation_line(action.title_key(), action.action_label(), theme),
            confirmation_line(
                if action == ConfirmationAction::Exit {
                    "Esc"
                } else {
                    "Any other key"
                },
                "cancel",
                theme,
            ),
        ]),
        body,
    );
}

fn confirmation_line(key: &'static str, label: &'static str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            key,
            Style::default()
                .fg(theme.code_text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}"), Style::default().fg(theme.muted())),
    ])
}

fn clamp_to(position: Position, area: Rect) -> Position {
    Position::new(
        position.x.clamp(area.x, area.right().saturating_sub(1)),
        position.y.clamp(area.y, area.bottom().saturating_sub(1)),
    )
}

fn is_actions_trigger(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Char('/')
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn is_file_finder_trigger(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Char('@')
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn is_skill_picker_trigger(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Char('$')
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn is_picker_navigation(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(
            key.code,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Up | KeyCode::Down | KeyCode::Esc
        )
}

fn is_mention_edit(event: &Event) -> bool {
    match event {
        Event::Key(key) => {
            matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && (key.code == KeyCode::Backspace
                    || matches!(key.code, KeyCode::Char(_))
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT))
        }
        Event::Paste(_) => true,
        _ => false,
    }
}

fn mention_edit_continues_query(event: &Event, valid: fn(char) -> bool) -> bool {
    match event {
        Event::Key(key) if key.code == KeyCode::Backspace => true,
        Event::Key(key) => {
            matches!(key.code, KeyCode::Char(character) if valid(character))
        }
        Event::Paste(text) => text.chars().all(valid),
        _ => false,
    }
}

fn is_file_query_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '/')
}

fn is_skill_query_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '-'
}

fn is_focus_toggle(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
}

fn is_left_click_in(event: &Event, area: Rect) -> bool {
    if !is_left_click(event) {
        return false;
    }
    let Event::Mouse(mouse) = event else {
        unreachable!("left click helper only accepts mouse events");
    };
    area.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
}

fn is_left_click(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left)
    )
}

fn is_control_c(event: &Event) -> bool {
    is_control_key(event, 'c')
}

fn is_confirmation_key_repeat(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    if key.kind != KeyEventKind::Repeat {
        return false;
    }
    is_control_c(event) || is_escape(event)
}

fn is_key_release(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Release)
}

fn is_control_key(event: &Event, character: char) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Char(character)
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_escape(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Esc
        && key.modifiers.is_empty()
}

fn is_plain_enter(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Enter
        && key.modifiers.is_empty()
}

fn is_plain_key(event: &Event, character: char) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Char(character)
        && key.modifiers.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{
        Component, ComposerChromeTarget, ConfirmationAction, Overlay, RenderRequest, RootEffect,
        RootEvent, RootNode, SubagentOverlay, ThreadState, TranscriptEvent,
    };
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode},
        core::extensions::{
            Skill,
            memory::{MemoryKey, MemoryRecord},
            subagents::{AgentDescriptor, AgentId, AgentMessageUpdate, AgentStatus, AgentUpdate},
        },
        tui::{
            theme::{Theme, ThemeMode},
            transcript::{LocalEvent, TranscriptRecord, TurnId},
        },
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use nanocodex::agent::{
        events::{AgentEvent, AgentEventKind},
        input::{PromptInput, UserInput},
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Position,
        style::{Color, Modifier},
    };
    use semver::Version;
    use serde_json::{json, value::to_raw_value};
    use std::{
        fs,
        path::Path,
        sync::Arc,
        time::{Duration, Instant},
    };

    fn key(code: KeyCode, modifiers: KeyModifiers) -> super::RootEvent {
        super::RootEvent::Terminal(Event::Key(KeyEvent::new(code, modifiers)))
    }

    fn memory_record(id: i64, version: u64, content: &str) -> MemoryRecord {
        MemoryRecord {
            key: MemoryKey { id, version },
            content: content.to_owned(),
            created_at_ms: 0,
            updated_at_ms: 0,
            last_scanned_at_ms: None,
            scan_count: 0,
            last_used_at_ms: None,
            use_count: 0,
            probation_until_ms: None,
        }
    }

    fn key_with_kind(
        code: KeyCode,
        modifiers: KeyModifiers,
        kind: KeyEventKind,
    ) -> super::RootEvent {
        let mut key = KeyEvent::new(code, modifiers);
        key.kind = kind;
        super::RootEvent::Terminal(Event::Key(key))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> super::RootEvent {
        super::RootEvent::Terminal(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    }

    fn text_column(buffer: &ratatui::buffer::Buffer, row: u16, text: &str) -> u16 {
        let symbols = text
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>();
        let width = u16::try_from(symbols.len()).unwrap();
        (0..=buffer.area.width.saturating_sub(width))
            .find(|&column| {
                symbols.iter().enumerate().all(|(offset, symbol)| {
                    buffer[(column + u16::try_from(offset).unwrap(), row)].symbol() == symbol
                })
            })
            .expect("rendered text should be present")
    }

    fn render_root_text(root: &mut RootNode, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn run_steered() -> super::RootEvent {
        super::RootEvent::Transcript(Arc::new(TranscriptRecord::from_agent(
            1,
            1,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("test"),
                seq: 1,
                kind: AgentEventKind::RunSteered,
                payload: to_raw_value(&json!({
                    "steer_index": 1,
                    "instruction_bytes": 5,
                }))
                .unwrap()
                .into(),
            },
        )))
    }

    fn agent_record(
        sequence: u64,
        kind: AgentEventKind,
        payload: serde_json::Value,
    ) -> Arc<TranscriptRecord> {
        Arc::new(TranscriptRecord::from_agent(
            sequence,
            sequence,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("opaque-test-id"),
                seq: sequence,
                kind,
                payload: to_raw_value(&payload).unwrap().into(),
            },
        ))
    }

    #[test]
    fn context_diagnostics_tracks_restored_and_live_records() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let outbound = agent_record(
            1,
            AgentEventKind::ApiEvent,
            json!({
                "direction": "outbound",
                "phase": "generation",
                "event": {
                    "previous_response_id": "opaque-response-id",
                    "prompt_cache_key": "opaque-cache-key"
                }
            }),
        );
        root.restore_session(
            Path::new("/work"),
            ReasoningEffort::Medium,
            ReasoningMode::Standard,
            ReasoningMode::Standard,
            false,
            vec![outbound],
        );
        assert_eq!(
            root.context_diagnostics.continuation,
            Some(crate::tui::context::ContinuationMode::PreviousResponse)
        );

        let completed = agent_record(
            2,
            AgentEventKind::ModelCallCompleted,
            json!({
                "call_index": 1,
                "model": "gpt-5.6-sol",
                "attempt": 1,
                "connection_generation": 1,
                "status": "completed",
                "duration_ns": 1,
                "time_to_first_event_ns": 1,
                "time_to_first_output_ns": 1,
                "tool_calls": 0,
                "usage": {
                    "input_tokens": 1_000,
                    "input_tokens_details": {"cached_tokens": 800},
                    "output_tokens": 50,
                    "total_tokens": 1_050
                }
            }),
        );
        root.update(super::RootEvent::Transcript(completed));
        assert_eq!(root.context_diagnostics.usage.unwrap().cached_input, 800);

        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "debug context".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(root.overlay, Some(Overlay::ContextDiagnostics(_))));

        root.update(key(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(root.context_diagnostics.usage.unwrap().total, 1_050);
        root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(root.overlay.is_none());
    }

    #[test]
    fn restored_session_does_not_keep_historical_activity() {
        let mut projection = RootNode::project_session(
            ReasoningEffort::Medium,
            vec![
                agent_record(1, AgentEventKind::RunStarted, json!({})),
                agent_record(2, AgentEventKind::RunStarted, json!({})),
                agent_record(
                    3,
                    AgentEventKind::ToolCall,
                    json!({
                        "call_id": "orphaned-shell",
                        "tool": "exec_command",
                        "arguments": {"cmd": "sleep 100"},
                    }),
                ),
            ],
        );

        let restored = projection
            .transcript
            .update(TranscriptEvent::AgentStreamClosed);
        assert!(restored.effects.is_empty());

        let started = projection
            .transcript
            .update(TranscriptEvent::Record(agent_record(
                4,
                AgentEventKind::RunStarted,
                json!({}),
            )));
        assert_eq!(started.effects.len(), 1);
        assert!(started.effects[0].active);
        assert_eq!(started.effects[0].status.as_deref(), Some("Thinking…"));

        let completed = projection
            .transcript
            .update(TranscriptEvent::Record(agent_record(
                5,
                AgentEventKind::RunCompleted,
                json!({"duration_ns": 1_000_000}),
            )));
        assert_eq!(completed.effects.len(), 1);
        assert!(!completed.effects[0].active);
        assert!(completed.effects[0].status.is_none());
    }

    #[test]
    fn composer_is_anchored_to_the_bottom() {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 7)].symbol(), "╭");
        assert_eq!(buffer[(0, 11)].symbol(), "╰");
        assert_eq!(buffer[(0, 6)].symbol(), " ");
    }

    #[test]
    fn clicking_composer_chrome_opens_effort_and_subagents() {
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let top = root.composer_area.y;
        let effort_x = text_column(terminal.backend().buffer(), top, "medium");
        assert_eq!(
            root.composer
                .component()
                .chrome_target(Position::new(effort_x, top)),
            Some(ComposerChromeTarget::Effort)
        );

        root.update(mouse(
            MouseEventKind::Down(MouseButton::Left),
            effort_x,
            top,
        ));
        assert!(matches!(root.overlay, Some(Overlay::Effort(_))));

        root.overlay = None;
        root.update(super::RootEvent::Subagent(AgentUpdate::Added(
            AgentDescriptor {
                id: AgentId::new(1),
                session_id: "child".to_owned(),
                role: "worker".to_owned(),
                task: "work".to_owned(),
                parent: None,
            },
        )));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let subagents_x = text_column(terminal.backend().buffer(), top, "1 subagents");

        root.update(mouse(
            MouseEventKind::Down(MouseButton::Left),
            subagents_x,
            top,
        ));
        assert!(matches!(
            root.overlay,
            Some(Overlay::Subagents(SubagentOverlay::Tree))
        ));
    }

    #[test]
    fn root_messages_render_once_in_main_and_are_projected_into_child_transcripts() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::Subagent(AgentUpdate::Added(AgentDescriptor {
            id: AgentId::new(1),
            session_id: "child".to_owned(),
            role: "worker".to_owned(),
            task: "verify ordering".to_owned(),
            parent: None,
        })));
        root.update(RootEvent::Transcript(Arc::new(
            TranscriptRecord::from_agent(
                1,
                1,
                AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("test"),
                    seq: 1,
                    kind: AgentEventKind::ToolCall,
                    payload: to_raw_value(&json!({
                        "call_id": "message-1",
                        "tool": "send_agent_message",
                        "arguments": {
                            "agent_id": 1,
                            "message": "Please verify the ordering.",
                            "priority": "deferred",
                            "purpose": "coordinate"
                        }
                    }))
                    .unwrap()
                    .into(),
                },
            ),
        )));
        let message = serde_json::from_value::<AgentMessageUpdate>(json!({
            "message_id": 1,
            "thread": {
                "id": 1,
                "participants": [
                    {"kind": "root"},
                    {"kind": "agent", "agent_id": 1}
                ],
                "messages": [{
                    "id": 1,
                    "thread_id": 1,
                    "from": {"kind": "root"},
                    "to": 1,
                    "priority": "deferred",
                    "purpose": "coordinate",
                    "body": "Please verify the ordering."
                }]
            },
            "delivery": {"state": "delivered", "disposition": "started"}
        }))
        .unwrap();
        root.update(RootEvent::Subagent(AgentUpdate::Message(message)));

        let main = render_root_text(&mut root, 100, 20);

        let mut child = Terminal::new(TestBackend::new(100, 40)).unwrap();
        child
            .draw(|frame| {
                root.subagents.render_transcript(
                    AgentId::new(1),
                    frame,
                    frame.area(),
                    &Theme::default(),
                );
            })
            .unwrap();
        let child = child
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(main.matches("Message").count(), 1);
        assert!(child.contains("← Message  root → you"));
        assert!(child.contains("Please verify"));
        assert!(child.contains("ordering."));
    }

    #[test]
    fn peer_messages_are_projected_into_the_main_transcript() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        for (id, role) in [(1, "sender"), (2, "recipient")] {
            root.update(RootEvent::Subagent(AgentUpdate::Added(AgentDescriptor {
                id: AgentId::new(id),
                session_id: format!("child-{id}"),
                role: role.to_owned(),
                task: "coordinate with a peer".to_owned(),
                parent: None,
            })));
        }
        let message = serde_json::from_value::<AgentMessageUpdate>(json!({
            "message_id": 1,
            "thread": {
                "id": 1,
                "participants": [
                    {"kind": "agent", "agent_id": 1},
                    {"kind": "agent", "agent_id": 2}
                ],
                "messages": [{
                    "id": 1,
                    "thread_id": 1,
                    "from": {"kind": "agent", "agent_id": 1},
                    "to": 2,
                    "priority": "deferred",
                    "purpose": "coordinate",
                    "body": "Peer coordination is visible."
                }]
            },
            "delivery": {"state": "delivered", "disposition": "started"}
        }))
        .unwrap();

        root.update(RootEvent::Subagent(AgentUpdate::Message(message)));

        let main = render_root_text(&mut root, 100, 20);
        assert!(main.contains("← Message  #1 → #2"));
        assert!(main.contains("Peer coordination is visible."));
    }

    #[test]
    fn composer_hides_subagents_after_they_stop_running() {
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(super::RootEvent::Subagent(AgentUpdate::Added(
            AgentDescriptor {
                id: AgentId::new(1),
                session_id: "child".to_owned(),
                role: "worker".to_owned(),
                task: "work".to_owned(),
                parent: None,
            },
        )));
        root.update(super::RootEvent::Subagent(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Completed {
                output: json!({ "report": "done" }),
            },
        }));

        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(!rendered.contains("subagents"));
    }

    #[test]
    fn transcript_uses_the_space_above_the_composer() {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "hello transcript".to_owned(),
            },
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(record)));

        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert!((0..7).any(|y| buffer[(0, y)].symbol() == "┃"));
        assert_eq!(buffer[(0, 7)].symbol(), "╭");
    }

    #[test]
    fn clicking_a_pinned_prompt_reveals_its_transcript_entry() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let prompt = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "jump to this prompt".to_owned(),
            },
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(prompt)));
        root.update(super::RootEvent::Transcript(agent_record(
            2,
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": 1,
                "item_id": "answer",
                "phase": "final_answer",
                "text": (1..=40)
                    .map(|line| format!("answer {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            }),
        )));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        root.update(mouse(MouseEventKind::ScrollUp, 5, 1));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        assert_eq!(
            terminal.backend().buffer()[(5, 0)].bg,
            Theme::default().code_background()
        );

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), 5, 0));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let first_row = (0..40)
            .map(|column| terminal.backend().buffer()[(column, 0)].symbol())
            .collect::<String>();
        assert!(first_row.contains("jump to this prompt"));
        assert_ne!(
            terminal.backend().buffer()[(5, 0)].bg,
            Theme::default().code_background()
        );
    }

    #[test]
    fn clicking_a_transcript_link_requests_that_it_be_opened() {
        let mut terminal = Terminal::new(TestBackend::new(50, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let record = TranscriptRecord::from_agent(
            1,
            1,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("test"),
                seq: 1,
                kind: AgentEventKind::AssistantMessage,
                payload: to_raw_value(&json!({
                    "model_call_index": 1,
                    "item_id": "answer",
                    "phase": "final_answer",
                    "text": "Open [the site](https://example.com).",
                }))
                .unwrap()
                .into(),
            },
        );
        root.update(super::RootEvent::Transcript(Arc::new(record)));
        root.queue.component_mut().push("queued".to_owned());
        root.queue.component_mut().set_focused(true);
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (column, row) = (0..buffer.area.height)
            .find_map(|row| {
                let rendered = (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>();
                rendered
                    .find("the site")
                    .map(|column| (u16::try_from(column).unwrap(), row))
            })
            .expect("link label should be rendered");

        let down = root.update(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
        assert!(down.effects.is_empty());
        let up = root.update(mouse(MouseEventKind::Up(MouseButton::Left), column, row));

        assert_eq!(
            up.effects,
            [RootEffect::OpenLink("https://example.com".to_owned())]
        );
        assert!(!root.queue.component().focused());
    }

    #[test]
    fn submitting_a_prompt_returns_the_transcript_to_the_tail() {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        for sequence in 1..=20 {
            let record = TranscriptRecord::from_local(
                sequence,
                sequence,
                LocalEvent::UserSubmitted {
                    id: TurnId::new(sequence),
                    text: format!("old prompt {sequence}"),
                },
            )
            .unwrap();
            root.update(super::RootEvent::Transcript(Arc::new(record)));
        }
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        root.update(key(KeyCode::PageUp, KeyModifiers::NONE));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        for character in "new prompt".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            submitted.effects,
            [RootEffect::Submit("new prompt".to_owned().into())]
        );
        let record = TranscriptRecord::from_local(
            21,
            21,
            LocalEvent::UserSubmitted {
                id: TurnId::new(21),
                text: "new prompt".to_owned(),
            },
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(record)));

        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("new prompt"));
    }

    #[test]
    fn leading_slash_opens_actions_without_changing_the_draft() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

        let update = root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

        assert!(matches!(&root.overlay, Some(Overlay::Actions(_))));
        assert!(root.composer().draft().is_empty());
        assert_eq!(update.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn slash_after_prompt_text_remains_in_the_composer() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('a'), KeyModifiers::NONE));

        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "a/");
    }

    #[test]
    fn dollar_at_a_token_boundary_opens_skills_and_remains_in_the_draft() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.set_skills(
            vec![Skill::new(
                "autofix",
                "Review and repair a pull request until clean.",
            )]
            .into(),
        );
        for character in "use ".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let update = root.update(key(KeyCode::Char('$'), KeyModifiers::NONE));

        assert!(matches!(&root.overlay, Some(Overlay::Skills(_))));
        assert_eq!(root.composer().draft(), "use $");
        assert_eq!(update.render, RenderRequest::Immediate);
        let rendered = render_root_text(&mut root, 90, 20);
        assert!(rendered.contains("$autofix"));
        assert!(rendered.contains("Review and repair a pull request until clean."));
    }

    #[test]
    fn dollar_is_literal_without_available_skills_or_inside_a_token() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('$'), KeyModifiers::NONE));
        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "$");

        root.composer.component_mut().replace_draft(String::new());
        root.set_skills(vec![Skill::new("autofix", "Repair a pull request.")].into());
        for character in "price$5".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "price$5");
    }

    #[test]
    fn dollar_is_literal_in_shell_mode() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.set_skills(vec![Skill::new("autofix", "Repair a pull request.")].into());

        for character in "!echo $PATH".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "!echo $PATH");

        let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            submitted.effects,
            [RootEffect::RunShell("echo $PATH".to_owned())]
        );
    }

    fn assert_skill_selection(key_code: KeyCode) {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.set_skills(
            vec![
                Skill::new("autofix", "Repair a pull request."),
                Skill::new("open-docs", "Open documentation."),
            ]
            .into(),
        );
        for character in "use later".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        for _ in 0.."later".len() {
            root.update(key(KeyCode::Left, KeyModifiers::NONE));
        }
        for character in "$auto".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        root.update(key(key_code, KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "use $autofix later");
    }

    #[test]
    fn enter_selects_a_filtered_skill_at_the_composer_cursor() {
        assert_skill_selection(KeyCode::Enter);
    }

    #[test]
    fn tab_selects_a_filtered_skill_at_the_composer_cursor() {
        assert_skill_selection(KeyCode::Tab);
    }

    #[test]
    fn escape_preserves_a_literal_skill_query() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.set_skills(vec![Skill::new("autofix", "Repair a pull request.")].into());
        for character in "$auto".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        root.update(key(KeyCode::Esc, KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "$auto");
    }

    #[test]
    fn mouse_dismisses_mention_popovers_with_an_immediate_redraw() {
        let workspace = tempfile::tempdir().unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));

        let file_update = root.update(mouse(MouseEventKind::Moved, 0, 0));

        assert!(root.overlay.is_none());
        assert_eq!(file_update.render, RenderRequest::Immediate);

        root.set_skills(vec![Skill::new("autofix", "Repair a pull request.")].into());
        root.update(key(KeyCode::Char(' '), KeyModifiers::NONE));
        root.update(key(KeyCode::Char('$'), KeyModifiers::NONE));

        let skill_update = root.update(mouse(MouseEventKind::Moved, 0, 0));

        assert!(root.overlay.is_none());
        assert_eq!(skill_update.render, RenderRequest::Immediate);
    }

    #[test]
    fn at_at_a_token_boundary_opens_the_file_finder_and_remains_in_the_draft() {
        let workspace = tempfile::tempdir().unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
        for character in "inspect ".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let update = root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));

        assert!(matches!(&root.overlay, Some(Overlay::FileFinder(_))));
        assert_eq!(root.composer().draft(), "inspect @");
        assert_eq!(update.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn releasing_at_keeps_the_file_finder_open() {
        let workspace = tempfile::tempdir().unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));

        let update = root.update(key_with_kind(
            KeyCode::Char('@'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));

        assert!(matches!(&root.overlay, Some(Overlay::FileFinder(_))));
        assert!(update.effects.is_empty());
        assert_eq!(update.render, super::RenderRequest::None);
    }

    #[test]
    fn at_inside_a_token_is_inserted_without_opening_the_file_finder() {
        let workspace = tempfile::tempdir().unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
        for character in "name@example.com".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "name@example.com");
    }

    fn assert_file_selection(key_code: KeyCode) {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("notes.md"), "remember this").unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
        for character in "inspect ".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));
        for character in "notes".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        root.update(key(key_code, KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "inspect @notes.md ");
    }

    #[test]
    fn enter_selects_a_file_at_the_composer_cursor() {
        assert_file_selection(KeyCode::Enter);
    }

    #[test]
    fn tab_selects_a_file_at_the_composer_cursor() {
        assert_file_selection(KeyCode::Tab);
    }

    #[test]
    fn selecting_a_file_replaces_the_query_in_the_middle_of_a_draft() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("notes.md"), "remember this").unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
        for character in "inspect later".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        for _ in 0.."later".len() {
            root.update(key(KeyCode::Left, KeyModifiers::NONE));
        }
        for character in "@notes".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "inspect @notes.md later");
    }

    #[test]
    fn escape_preserves_a_literal_mention_and_backspace_removes_it() {
        let workspace = tempfile::tempdir().unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);

        root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));
        root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "@");

        root.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        root.update(key(KeyCode::Char('@'), KeyModifiers::NONE));
        root.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(root.overlay.is_none());
        assert!(root.composer().draft().is_empty());
    }

    #[test]
    fn mention_query_is_composer_text_and_space_closes_suggestions() {
        let workspace = tempfile::tempdir().unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);

        for character in "@someone ".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "@someone ");
    }

    #[test]
    fn escape_and_empty_backspace_close_actions_immediately() {
        for dismiss in [KeyCode::Esc, KeyCode::Backspace] {
            let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
            root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

            let update = root.update(key(dismiss, KeyModifiers::NONE));

            assert!(root.overlay.is_none());
            assert_eq!(update.render, super::RenderRequest::Immediate);
        }
    }

    #[test]
    fn control_c_requires_confirmation_while_actions_are_open() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

        let first = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(first.effects.is_empty());
        assert!(root.overlay.is_some());
        let rendered = render_root_text(&mut root, 60, 12);
        assert!(rendered.contains("Ctrl+C then"));
        assert!(rendered.contains("Ctrl+C Quit"));
        assert!(rendered.contains("Esc cancel"));

        let second = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(second.effects, [super::RootEffect::Shutdown]);
    }

    #[test]
    fn escape_cancels_a_pending_exit() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        let cancel = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let next_control_c = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(cancel.effects.is_empty());
        assert_eq!(cancel.render, super::RenderRequest::Immediate);
        assert!(next_control_c.effects.is_empty());
    }

    #[test]
    fn control_c_requires_two_distinct_presses() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        let release = root.update(key_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        ));
        let repeat = root.update(key_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        ));

        assert!(release.effects.is_empty());
        assert!(repeat.effects.is_empty());
        assert!(root.key_confirmation.is_some());

        let second_press = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(second_press.effects, [RootEffect::Shutdown]);
    }

    #[test]
    fn confirmation_floats_above_the_composer_top_right() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();

        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let popup_bottom = root.composer_area.y - 2;
        assert_eq!(
            buffer[(root.composer_area.right() - 28, popup_bottom)].symbol(),
            "╰"
        );
        assert_eq!(
            buffer[(root.composer_area.right() - 1, popup_bottom)].symbol(),
            "╯"
        );
        assert_eq!(
            buffer[(root.composer_area.right() - 1, root.composer_area.y)].symbol(),
            "╮"
        );
    }

    #[test]
    fn control_c_clears_the_focused_composer_before_shutting_down() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('h'), KeyModifiers::NONE));
        root.update(key(KeyCode::Char('i'), KeyModifiers::NONE));

        let clear = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(clear.effects.is_empty());
        assert_eq!(clear.render, super::RenderRequest::Immediate);
        assert!(root.composer().draft().is_empty());

        let confirmation = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(confirmation.effects.is_empty());
        assert!(render_root_text(&mut root, 60, 12).contains("Ctrl+C Quit"));

        let shutdown = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(shutdown.effects, [RootEffect::Shutdown]);
    }

    #[test]
    fn control_z_restores_the_last_cleared_draft() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReplaceDraft("first\nλright".to_owned()));
        for _ in 0..5 {
            root.update(key(KeyCode::Left, KeyModifiers::NONE));
        }

        root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(root.composer().draft().is_empty());
        assert!(root.discarded_draft.is_some());

        let restored = root.update(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        root.update(key(KeyCode::Char('|'), KeyModifiers::NONE));

        assert_eq!(restored.render, super::RenderRequest::Immediate);
        assert_eq!(root.composer().draft(), "first\nλ|right");
        assert!(root.discarded_draft.is_none());
    }

    #[test]
    fn restored_draft_keeps_pasted_images() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReplaceDraft("inspect ".to_owned()));
        root.update(RootEvent::PasteImage(
            "data:image/png;base64,restored".to_owned(),
        ));
        root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        root.update(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        let [RootEffect::Submit(prompt)] = submitted.effects.as_slice() else {
            panic!("restored draft should submit");
        };
        let PromptInput::Content(content) = prompt.agent_prompt().instruction else {
            panic!("restored image should produce multimodal input");
        };
        assert!(matches!(&content[0], UserInput::Text { text } if text == "inspect "));
        assert!(matches!(
            &content[1],
            UserInput::Image { image_url, .. } if image_url.ends_with("restored")
        ));
    }

    #[test]
    fn control_z_does_not_overwrite_a_nonempty_draft() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReplaceDraft("recover me".to_owned()));
        root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        root.update(RootEvent::ReplaceDraft("keep me".to_owned()));

        let update = root.update(key(KeyCode::Char('z'), KeyModifiers::CONTROL));

        assert_eq!(update.render, super::RenderRequest::None);
        assert_eq!(root.composer().draft(), "keep me");
        assert!(root.discarded_draft.is_some());
    }

    #[test]
    fn successful_session_replacement_preserves_the_displaced_draft() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReplaceDraft("continue later".to_owned()));

        root.reset_session(
            Path::new("/work"),
            ReasoningEffort::High,
            ReasoningMode::Standard,
            ReasoningMode::Standard,
        );
        root.update(key(KeyCode::Char('z'), KeyModifiers::CONTROL));

        assert_eq!(root.composer().draft(), "continue later");
    }

    #[test]
    fn double_escape_interrupts_without_shutting_down() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

        let first = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let rendered = render_root_text(&mut root, 60, 12);
        let second = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

        assert!(first.effects.is_empty());
        assert!(rendered.contains("Esc then"));
        assert!(rendered.contains("Esc Interrupt"));
        assert!(rendered.contains("Any other key cancel"));
        assert_eq!(second.effects, [RootEffect::CancelTurns]);
    }

    #[test]
    fn escape_requires_two_distinct_presses() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Esc, KeyModifiers::NONE));

        let release = root.update(key_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        let repeat = root.update(key_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));

        assert!(release.effects.is_empty());
        assert!(repeat.effects.is_empty());
        assert!(root.key_confirmation.is_some());

        let second_press = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(second_press.effects, [RootEffect::CancelTurns]);
    }

    #[test]
    fn tab_swaps_between_the_queue_and_composer() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        for character in "queued".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        root.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(root.queue.component().focused());
        root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(root.composer().draft().is_empty());

        root.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!root.queue.component().focused());
        root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));

        assert_eq!(root.composer().draft(), "x");
    }

    #[test]
    fn enter_in_an_empty_composer_steers_the_selected_queued_message() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("later".to_owned());
        root.queue.component_mut().push("steer now".to_owned());

        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            update.effects.as_slice(),
            [RootEffect::Steer { prompt, .. }] if prompt.display_text() == "steer now"
        ));
        assert!(!root.queue.component().focused());
        assert!(root.composer().draft().is_empty());
    }

    #[test]
    fn clicking_the_composer_returns_focus_to_it() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("queued".to_owned());
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        root.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(root.queue.component().focused());

        let down = root.update(mouse(MouseEventKind::Down(MouseButton::Left), 10, 9));
        let up = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 10, 9));

        assert!(!root.queue.component().focused());
        assert_eq!(down.render.max(up.render), super::RenderRequest::Immediate);
    }

    #[test]
    fn clicking_the_queue_keeps_focus_on_the_queue() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.queue.component_mut().push("queued".to_owned());
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let update = root.update(mouse(
            MouseEventKind::Down(MouseButton::Left),
            root.queue_area.x + 2,
            root.queue_area.y + 1,
        ));

        assert!(root.queue.component().focused());
        assert_eq!(update.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn clicking_empty_transcript_space_returns_focus_to_the_composer() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("queued".to_owned());
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        root.update(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(root.queue.component().focused());

        let update = root.update(mouse(MouseEventKind::Down(MouseButton::Left), 10, 2));

        assert!(!root.queue.component().focused());
        assert_eq!(update.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn active_turn_submissions_can_grow_the_queue_without_restoring_the_draft() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        for prompt in ["one", "two", "three", "four", "five"] {
            for character in prompt.chars() {
                root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
            }
            let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
            assert!(update.effects.is_empty());
        }

        assert_eq!(root.queue.component().len(), 5);
        assert!(root.composer().draft().is_empty());
        assert!(!root.queue.component().focused());
        assert!(root.notification.is_none());

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        assert_eq!(root.queue_area.width, 95);
        assert_eq!(root.queue_area.bottom(), root.composer_area.y);
    }

    #[test]
    fn shell_commands_bypass_the_agent_message_queue() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.update(super::RootEvent::ReplaceDraft("!pwd".to_owned()));

        let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(submitted.effects, [RootEffect::RunShell("pwd".to_owned())]);
        assert!(root.queue.component().is_empty());
        assert_eq!(root.in_flight_turns, 1);
    }

    #[test]
    fn finished_turns_batch_ready_queued_messages_in_order() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("first".to_owned());
        root.queue.component_mut().push("second".to_owned());

        let update = root.update(super::RootEvent::WorkerTurnFinished);
        assert_eq!(
            update.effects,
            [RootEffect::Submit("first\n\nsecond".to_owned().into())]
        );
        assert_eq!(root.in_flight_turns, 1);
        assert!(root.queue.component().is_empty());
    }

    #[test]
    fn editing_removes_a_message_until_the_editor_returns() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("original".to_owned());
        root.queue.component_mut().set_focused(true);

        let edit = root.update(key(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(
            edit.effects,
            [RootEffect::OpenQueueEditor {
                index: 0,
                text: "original".to_owned(),
            }]
        );
        assert!(root.queue.component().is_empty());

        root.update(super::RootEvent::RestoreQueued {
            index: 0,
            text: "edited".to_owned(),
        });
        assert_eq!(root.queue.component().len(), 1);
    }

    #[test]
    fn steer_completion_race_does_not_release_another_queued_message() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("later".to_owned());
        root.queue.component_mut().push("steer now".to_owned());
        root.queue.component_mut().set_focused(true);

        let steer = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let RootEffect::Steer { id, .. } = &steer.effects[0] else {
            panic!("enter should issue a steer");
        };
        let id = *id;
        let finished = root.update(super::RootEvent::WorkerTurnFinished);
        assert!(finished.effects.is_empty());
        assert_eq!(root.queue.component().len(), 2);

        root.update(super::RootEvent::SteerPromoted(id));
        let promoted_finished = root.update(super::RootEvent::WorkerTurnFinished);
        assert_eq!(
            promoted_finished.effects,
            [RootEffect::Submit("later".to_owned().into())]
        );
    }

    #[test]
    fn interrupt_drains_a_pending_steer_before_regular_queue_items() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("regular".to_owned());
        root.queue.component_mut().push("priority steer".to_owned());
        root.queue.component_mut().set_focused(true);
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        root.queue.component_mut().set_focused(false);

        root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let interrupt = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(interrupt.effects, [RootEffect::CancelTurns]);

        root.update(super::RootEvent::TurnsCancelled);
        let finished = root.update(super::RootEvent::WorkerTurnFinished);
        assert_eq!(
            finished.effects,
            [RootEffect::Submit(
                "priority steer\n\nregular".to_owned().into()
            )]
        );
    }

    #[test]
    fn applied_steer_after_interrupt_ack_is_not_submitted_again() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("later".to_owned());
        root.queue.component_mut().push("priority steer".to_owned());
        root.queue.component_mut().set_focused(true);
        let steer = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let RootEffect::Steer { id, .. } = &steer.effects[0] else {
            panic!("enter should issue a steer");
        };
        let id = *id;
        root.update(super::RootEvent::SteerAdmitted(id));

        root.update(super::RootEvent::TurnsCancelled);
        let applied = root.update(run_steered());
        let finished = root.update(super::RootEvent::WorkerTurnFinished);

        assert_eq!(
            applied.effects,
            [RootEffect::PersistSteer("priority steer".to_owned())]
        );
        assert_eq!(
            finished.effects,
            [RootEffect::Submit("later".to_owned().into())]
        );
    }

    #[test]
    fn steer_stays_queued_until_the_model_boundary_event() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("steer".to_owned());
        root.queue.component_mut().set_focused(true);
        let steer = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let RootEffect::Steer { id, .. } = &steer.effects[0] else {
            panic!("enter should issue a steer");
        };
        let id = *id;

        let admitted = root.update(super::RootEvent::SteerAdmitted(id));
        assert!(admitted.effects.is_empty());
        assert_eq!(root.queue.component().len(), 1);

        let applied = root.update(run_steered());
        assert_eq!(
            applied.effects,
            [RootEffect::PersistSteer("steer".to_owned())]
        );
        assert!(root.queue.component().is_empty());
    }

    #[test]
    fn model_boundary_before_worker_ack_is_reconciled_once() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.in_flight_turns = 1;
        root.queue.component_mut().push("steer".to_owned());
        root.queue.component_mut().set_focused(true);
        let steer = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let RootEffect::Steer { id, .. } = &steer.effects[0] else {
            panic!("enter should issue a steer");
        };
        let id = *id;

        let early_boundary = root.update(run_steered());
        assert!(early_boundary.effects.is_empty());
        assert_eq!(root.queue.component().len(), 1);

        let admitted = root.update(super::RootEvent::SteerAdmitted(id));
        assert_eq!(
            admitted.effects,
            [RootEffect::PersistSteer("steer".to_owned())]
        );
        assert!(root.queue.component().is_empty());
    }

    #[test]
    fn displaced_release_over_the_composer_copies_without_drag_events() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        for character in "copy me".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), 1, 8));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 7, 8));

        assert_eq!(update.effects, [RootEffect::Copy("copy me".to_owned())]);
        assert_eq!(update.render, super::RenderRequest::Immediate);
        assert!(!root.selection.is_active());
        assert!(root.notification.is_none());
        assert_eq!(root.composer().draft(), "copy me");
        root.update(super::RootEvent::NotifySuccess(
            "Copied selection to clipboard.".to_owned(),
        ));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Copied selection to clipboard."));
        let buffer = terminal.backend().buffer();
        let left = (40 - ("Copied selection to clipboard.".len() as u16 + 4)) / 2;
        assert_eq!(buffer[(left, 0)].symbol(), "╭");
        assert_eq!(buffer[(left, 0)].fg, ratatui::style::Color::Green);
        assert!(buffer[(left + 2, 1)].modifier.contains(Modifier::BOLD));

        let deadline = root.notification.as_ref().unwrap().deadline;
        root.update(super::RootEvent::AnimationFrame(deadline));
        assert!(root.notification.is_none());
    }

    #[test]
    fn narrow_notifications_keep_wrapped_action_text_visible() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(super::RootEvent::NotifySuccess(
            "Pro enabled for new sessions · start a new session to apply.".to_owned(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();

        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("start a new session"));
        assert!(rendered.contains("apply."));
    }

    #[test]
    fn update_available_uses_the_success_frame_and_styles_version_and_command() {
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let version = Version::new(1, 2, 3);
        let message = "Update available · v1.2.3 · run `tact update`";

        root.update(super::RootEvent::UpdateAvailable(version));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let message_width = unicode_width::UnicodeWidthStr::width(message) as u16;
        let left = (80 - (message_width + 4)) / 2;
        let text_start = left + 2;
        let prefix_width = unicode_width::UnicodeWidthStr::width("Update available · ") as u16;
        let version_width = unicode_width::UnicodeWidthStr::width("v1.2.3") as u16;
        let suffix_width = unicode_width::UnicodeWidthStr::width(" · run ") as u16;
        let version_start = text_start + prefix_width;
        let command_start = version_start + version_width + suffix_width;

        assert_eq!(buffer[(left, 0)].symbol(), "╭");
        assert_eq!(buffer[(left, 0)].fg, Color::Green);
        for column in text_start..version_start {
            assert_eq!(buffer[(column, 1)].fg, Color::Green);
            assert!(!buffer[(column, 1)].modifier.contains(Modifier::BOLD));
        }
        for column in version_start..version_start + version_width {
            assert_eq!(buffer[(column, 1)].fg, Color::Green);
            assert!(buffer[(column, 1)].modifier.contains(Modifier::BOLD));
        }
        for column in version_start + version_width..command_start {
            assert_eq!(buffer[(column, 1)].fg, Color::Green);
            assert!(!buffer[(column, 1)].modifier.contains(Modifier::BOLD));
        }
        for column in command_start..text_start + message_width {
            assert_eq!(buffer[(column, 1)].fg, Color::Reset);
            assert!(!buffer[(column, 1)].modifier.contains(Modifier::BOLD));
        }

        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(message));

        let deadline = root.notification.as_ref().unwrap().deadline;
        root.update(super::RootEvent::AnimationFrame(deadline));
        assert!(root.notification.is_none());
    }

    #[test]
    fn dragging_over_the_transcript_copies_visible_text() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "hello transcript".to_owned(),
            },
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(record)));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let row = (0..7)
            .find(|&row| terminal.backend().buffer()[(0, row)].symbol() == "┃")
            .unwrap();

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), 2, row));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        root.update(mouse(MouseEventKind::Drag(MouseButton::Left), 6, row));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 6, row));

        assert_eq!(update.effects, [RootEffect::Copy("hello".to_owned())]);
        assert!(!root.selection.is_active());
    }

    #[test]
    fn transcript_selection_copies_code_source_without_rendered_borders() {
        let mut terminal = Terminal::new(TestBackend::new(40, 14)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(super::RootEvent::Transcript(agent_record(
            1,
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": 1,
                "item_id": "answer",
                "phase": "final_answer",
                "text": "```rust\n    let answer = 42;\n```",
            }),
        )));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let row = (0..terminal.backend().buffer().area.height)
            .find(|&row| {
                (0..terminal.backend().buffer().area.width)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
                    .contains("    let answer = 42;")
            })
            .expect("code should be visible");

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), 0, row));
        root.update(mouse(MouseEventKind::Drag(MouseButton::Left), 39, row));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_ne!(buffer[(0, row)].bg, Color::Yellow);
        assert_eq!(buffer[(2, row)].bg, Color::Yellow);
        assert_ne!(buffer[(39, row)].bg, Color::Yellow);

        let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 39, row));
        assert_eq!(
            update.effects,
            [RootEffect::Copy("    let answer = 42;".to_owned())]
        );
    }

    #[test]
    fn transcript_selection_copies_original_markdown_syntax() {
        let mut terminal = Terminal::new(TestBackend::new(64, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(super::RootEvent::Transcript(agent_record(
            1,
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": 1,
                "item_id": "answer",
                "phase": "final_answer",
                "text": "**bold** and [site](https://example.com)",
            }),
        )));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let row = (0..terminal.backend().buffer().area.height)
            .find(|&row| {
                (0..terminal.backend().buffer().area.width)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
                    .contains("bold and site")
            })
            .expect("message should be visible");
        let start = text_column(terminal.backend().buffer(), row, "bold");
        let end = text_column(terminal.backend().buffer(), row, "site") + 3;

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), start, row));
        root.update(mouse(MouseEventKind::Drag(MouseButton::Left), end, row));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let destination = text_column(terminal.backend().buffer(), row, "https://example.com");
        assert_ne!(
            terminal.backend().buffer()[(destination, row)].bg,
            Color::Yellow
        );

        let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), end, row));
        assert_eq!(
            update.effects,
            [RootEffect::Copy(
                "**bold** and [site](https://example.com)".to_owned()
            )]
        );
    }

    #[test]
    fn partial_transcript_selection_does_not_copy_unmatched_markdown_delimiters() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(super::RootEvent::Transcript(agent_record(
            1,
            AgentEventKind::AssistantMessage,
            json!({
                "model_call_index": 1,
                "item_id": "answer",
                "phase": "final_answer",
                "text": "**bold**",
            }),
        )));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let row = (0..root.transcript_area.height)
            .find(|&row| {
                (0..40)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
                    .contains("bold")
            })
            .unwrap();
        let start = text_column(terminal.backend().buffer(), row, "bold");

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), start, row));
        root.update(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            start + 1,
            row,
        ));
        let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), start + 1, row));

        assert_eq!(update.effects, [RootEffect::Copy("bo".to_owned())]);
    }

    #[test]
    fn transcript_selection_survives_scrolling_beyond_the_viewport() {
        let mut terminal = Terminal::new(TestBackend::new(32, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        for sequence in 1..=10 {
            let record = TranscriptRecord::from_local(
                sequence,
                sequence,
                LocalEvent::UserSubmitted {
                    id: TurnId::new(sequence),
                    text: format!("prompt {sequence}"),
                },
            )
            .unwrap();
            root.update(super::RootEvent::Transcript(Arc::new(record)));
        }
        root.update(key(KeyCode::Home, KeyModifiers::CONTROL));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let start_row = (0..root.transcript_area.height)
            .find(|&row| {
                (0..32)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
                    .contains("prompt 1")
            })
            .expect("first prompt should be visible");

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), 0, start_row));
        root.update(mouse(MouseEventKind::ScrollDown, 4, start_row));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let end_row = (0..root.transcript_area.height)
            .rev()
            .find(|&row| terminal.backend().buffer()[(0, row)].symbol() == "┃")
            .expect("a later prompt should be visible");
        let end_column = (0..32)
            .rev()
            .find(|&column| terminal.backend().buffer()[(column, end_row)].symbol() != " ")
            .expect("prompt should contain text");
        root.update(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            end_column,
            end_row,
        ));
        let update = root.update(mouse(
            MouseEventKind::Up(MouseButton::Left),
            end_column,
            end_row,
        ));

        assert_eq!(
            update.effects,
            [RootEffect::Copy(
                "prompt 1\n\nprompt 2\n\nprompt 3\n\nprompt 4\n\nprompt 5".to_owned()
            )]
        );
    }

    #[test]
    fn dragging_at_the_viewport_edge_keeps_extending_the_selection() {
        let mut terminal = Terminal::new(TestBackend::new(32, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        for sequence in 1..=12 {
            let record = TranscriptRecord::from_local(
                sequence,
                sequence,
                LocalEvent::UserSubmitted {
                    id: TurnId::new(sequence),
                    text: format!("prompt {sequence}"),
                },
            )
            .unwrap();
            root.update(super::RootEvent::Transcript(Arc::new(record)));
        }
        root.update(key(KeyCode::Home, KeyModifiers::CONTROL));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let start_row = (0..root.transcript_area.height)
            .find(|&row| {
                (0..32)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
                    .contains("prompt 1")
            })
            .unwrap();
        let edge = root.transcript_area.bottom().saturating_sub(1);
        root.update(mouse(MouseEventKind::Down(MouseButton::Left), 2, start_row));
        root.update(mouse(MouseEventKind::Drag(MouseButton::Left), 31, edge));

        for _ in 0..4 {
            terminal
                .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
                .unwrap();
            let deadline = root
                .selection_auto_scroll
                .as_ref()
                .expect("edge drag should keep scrolling")
                .deadline;
            root.update(super::RootEvent::AnimationFrame(deadline));
        }
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 31, edge));
        let [RootEffect::Copy(text)] = update.effects.as_slice() else {
            panic!("edge drag should copy the semantic selection");
        };

        assert!(text.starts_with("prompt 1\n\n"));
        assert!(text.contains("prompt 6"));
        assert!(!text.contains('┃'));
        assert!(root.selection_auto_scroll.is_none());
    }

    #[test]
    fn composer_selection_scrolls_without_losing_offscreen_text() {
        let mut terminal = Terminal::new(TestBackend::new(32, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(super::RootEvent::ReplaceDraft(
            (1..=10)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let start_row = root.composer_content_area.bottom().saturating_sub(1);
        let start_column = text_column(terminal.backend().buffer(), start_row, "line 10");

        root.update(mouse(
            MouseEventKind::Down(MouseButton::Left),
            start_column + 6,
            start_row,
        ));
        root.update(mouse(MouseEventKind::ScrollUp, start_column, start_row));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let end_row = root.composer_content_area.y;
        let end_column = text_column(terminal.backend().buffer(), end_row, "line 2");
        root.update(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            end_column,
            end_row,
        ));
        let update = root.update(mouse(
            MouseEventKind::Up(MouseButton::Left),
            end_column,
            end_row,
        ));

        assert_eq!(
            update.effects,
            [RootEffect::Copy(
                (2..=10)
                    .map(|line| format!("line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )]
        );
    }

    #[test]
    fn transcript_selection_excludes_the_top_right_hint() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: ["copy this prompt"; 8].join("\n"),
            },
        )
        .unwrap();
        root.update(super::RootEvent::Transcript(Arc::new(record)));
        root.transcript.component_mut().focus_expandables();
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        root.update(mouse(MouseEventKind::Drag(MouseButton::Left), 39, 0));
        terminal
            .draw(|frame| root.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 39, 0));

        assert_eq!(
            update.effects,
            [RootEffect::Copy("copy this prompt".to_owned())]
        );
    }

    #[test]
    fn key_confirmation_expires_and_unrelated_input_resets_it() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let now = Instant::now();

        assert!(
            root.update_key_confirmation(ConfirmationAction::Interrupt, now)
                .effects
                .is_empty()
        );
        assert!(
            root.update_key_confirmation(
                ConfirmationAction::Interrupt,
                now + super::KEY_CONFIRMATION_TIMEOUT + Duration::from_millis(1),
            )
            .effects
            .is_empty()
        );
        root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            root.update(key(KeyCode::Esc, KeyModifiers::NONE))
                .effects
                .is_empty()
        );
    }

    #[test]
    fn expired_confirmation_is_removed_immediately() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let now = Instant::now();
        root.update_key_confirmation(ConfirmationAction::Exit, now);

        let update = root.update(super::RootEvent::AnimationFrame(
            now + super::KEY_CONFIRMATION_TIMEOUT,
        ));

        assert!(root.key_confirmation.is_none());
        assert_eq!(update.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn effort_action_opens_the_selector_and_applies_the_selection() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

        for character in "effort".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(&root.overlay, Some(Overlay::Effort(_))));

        root.update(key(KeyCode::Right, KeyModifiers::NONE));
        assert!(root.animation_deadline().is_some());
        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            update.effects,
            [RootEffect::SetEffort {
                effort: ReasoningEffort::High,
                reasoning_mode: ReasoningMode::Standard,
            }]
        );
        assert_eq!(root.composer().effort(), ReasoningEffort::High);
        assert!(root.overlay.is_none());

        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| root.render(frame, frame.area(), &theme))
            .unwrap();
        let plasma = terminal
            .backend()
            .buffer()
            .content()
            .chunks(80)
            .take(15)
            .flatten()
            .filter(|cell| cell.symbol() != " ")
            .collect::<Vec<_>>();
        assert!(!plasma.is_empty());
        assert!(
            plasma
                .iter()
                .all(|cell| matches!(cell.fg, Color::Yellow) || cell.fg == theme.code_text())
        );
    }

    #[test]
    fn pro_preference_does_not_change_the_running_session_mode() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "effort".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        root.update(key(KeyCode::Char('p'), KeyModifiers::NONE));
        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            update.effects,
            [RootEffect::SetEffort {
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Pro,
            }]
        );
        assert_eq!(root.composer().reasoning_mode(), ReasoningMode::Standard);
        let notification = root.notification.as_ref().unwrap();
        let message = notification
            .message
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            message,
            "Pro enabled for new sessions · start a new session to apply."
        );
        assert_eq!(notification.color, Color::Green);

        root.reset_session(
            Path::new("/work"),
            ReasoningEffort::Medium,
            ReasoningMode::Pro,
            ReasoningMode::Pro,
        );
        assert_eq!(root.composer().reasoning_mode(), ReasoningMode::Pro);

        root.open_effort();
        root.update(key(KeyCode::Char('p'), KeyModifiers::NONE));
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let notification = root.notification.as_ref().unwrap();
        let message = notification
            .message
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            message,
            "Pro disabled for new sessions · start a new session to apply."
        );
        assert_eq!(notification.color, Color::Green);
    }

    #[test]
    fn fast_mode_action_toggles_the_runtime_setting() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "fast mode".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let enabled = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(enabled.effects, [RootEffect::SetFastMode(true)]);
        assert!(root.composer().fast_mode());
        assert!(root.overlay.is_none());

        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "priority".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let disabled = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(disabled.effects, [RootEffect::SetFastMode(false)]);
        assert!(!root.composer().fast_mode());
    }

    #[test]
    fn theme_action_opens_the_selector_and_applies_the_selection() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "appearance".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(&root.overlay, Some(Overlay::Theme(_))));

        root.update(key(KeyCode::Down, KeyModifiers::NONE));
        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(update.effects, [RootEffect::SetTheme(ThemeMode::Light)]);
        assert!(root.overlay.is_none());
    }

    #[test]
    fn control_s_opens_effort_for_new_and_started_threads() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

        let opened = root.update(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(&root.overlay, Some(Overlay::Effort(_))));
        assert_eq!(opened.render, super::RenderRequest::Immediate);

        root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        root.thread = super::ThreadState::Started;
        let reopened = root.update(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(&root.overlay, Some(Overlay::Effort(_))));
        assert_eq!(reopened.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn control_o_toggles_transcript_expansion_globally() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

        let expanded = root.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let collapsed = root.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));

        assert_eq!(expanded.render, super::RenderRequest::Immediate);
        assert_eq!(collapsed.render, super::RenderRequest::Immediate);
        assert!(root.composer().draft().is_empty());
    }

    #[test]
    fn control_o_does_not_change_the_hidden_transcript_behind_an_overlay() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

        let update = root.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));

        assert_eq!(update.render, super::RenderRequest::None);
        assert!(matches!(root.overlay, Some(Overlay::Actions(_))));
    }

    #[test]
    fn escape_that_blurs_expandable_items_does_not_start_the_interrupt_chord() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.transcript.component_mut().focus_expandables();

        let blurred = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

        assert!(blurred.effects.is_empty());
        assert!(!root.transcript.component().expandables_focused());
        assert!(root.key_confirmation.is_none());

        let chord_started = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(chord_started.effects.is_empty());
        assert!(root.key_confirmation.is_some());
    }

    #[test]
    fn keybindings_action_opens_help_and_escape_closes_it() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "keyboard".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(&root.overlay, Some(Overlay::Keybindings(_))));

        root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(root.overlay.is_none());
    }

    #[test]
    fn resize_redraws_while_keybindings_help_is_open() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "keyboard".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        let update = root.update(super::RootEvent::Terminal(Event::Resize(100, 30)));

        assert_eq!(update.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn config_action_closes_the_menu_and_requests_the_external_editor() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "edit config".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(update.effects, [RootEffect::OpenConfigEditor]);
        assert_eq!(update.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn reload_config_action_closes_the_menu_and_requests_a_reload() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "refresh".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(update.effects, [RootEffect::ReloadConfig]);
        assert_eq!(update.render, super::RenderRequest::Immediate);
    }

    #[test]
    fn memory_action_loads_inspects_and_deletes_without_submitting_a_prompt() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.set_memory_enabled(true);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "remember".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let opened = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(opened.effects, [RootEffect::LoadMemories]);
        assert!(matches!(&root.overlay, Some(Overlay::Memory(_))));
        assert!(root.composer().draft().is_empty());

        root.update(RootEvent::MemoriesLoaded(vec![memory_record(
            7,
            3,
            "remember this",
        )]));
        let inspected = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(inspected.effects.is_empty());
        assert!(render_root_text(&mut root, 80, 28).contains("remember this"));

        root.update(key(KeyCode::Char('d'), KeyModifiers::NONE));
        let deleted = root.update(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(
            deleted.effects,
            [RootEffect::DeleteMemory(MemoryKey { id: 7, version: 3 })]
        );
        assert!(root.composer().draft().is_empty());

        root.update(RootEvent::MemoryDeleted { id: 7 });
        assert!(render_root_text(&mut root, 80, 20).contains("Memory is empty"));
    }

    #[test]
    fn memory_completions_are_ignored_after_the_browser_closes() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.set_memory_enabled(true);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "memory".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(root.overlay.is_none());

        for event in [
            RootEvent::MemoriesLoaded(vec![memory_record(1, 1, "stale")]),
            RootEvent::MemoryLoadFailed("stale load".to_owned()),
            RootEvent::MemoryDeleted { id: 1 },
            RootEvent::MemoryDeleteFailed {
                error: "stale delete".to_owned(),
                conflict: false,
            },
        ] {
            let update = root.update(event);
            assert!(update.effects.is_empty());
            assert_eq!(update.render, RenderRequest::None);
            assert!(root.overlay.is_none());
        }
    }

    #[test]
    fn memory_availability_survives_reset_and_fork_and_disabling_closes_the_browser() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.set_memory_enabled(true);
        root.reset_session(
            Path::new("/work"),
            ReasoningEffort::Low,
            ReasoningMode::Standard,
            ReasoningMode::Standard,
        );
        let fork = root.fork(Path::new("/work"), ReasoningEffort::Low);
        assert!(root.memory_enabled);
        assert!(fork.memory_enabled);

        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "memory".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(&root.overlay, Some(Overlay::Memory(_))));

        root.set_memory_enabled(false);
        assert!(!root.memory_enabled);
        assert!(root.overlay.is_none());
    }

    #[test]
    fn new_session_action_clears_the_completed_thread_after_runtime_replacement() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        for character in "old prompt".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        root.update(super::RootEvent::WorkerTurnFinished);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "clear".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let requested = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(requested.effects, [RootEffect::NewSession]);
        assert!(root.overlay.is_none());
        assert!(!root.interactive);

        root.reset_session(
            Path::new("/work"),
            ReasoningEffort::Medium,
            ReasoningMode::Standard,
            ReasoningMode::Standard,
        );

        assert!(root.interactive);
        assert!(matches!(root.thread, ThreadState::New));
        assert!(root.composer().draft().is_empty());
        assert_eq!(root.in_flight_turns, 0);
    }

    #[test]
    fn new_session_action_is_unavailable_while_work_is_active() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        for character in "active prompt".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "clear".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(update.effects.is_empty());
        assert!(matches!(&root.overlay, Some(Overlay::Actions(_))));
    }

    #[test]
    fn effort_action_remains_available_after_the_first_prompt() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('h'), KeyModifiers::NONE));
        root.update(key(KeyCode::Char('i'), KeyModifiers::NONE));

        let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            submitted.effects,
            [RootEffect::Submit("hi".to_owned().into())]
        );

        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "effort".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let update = root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(update.effects.is_empty());
        assert!(matches!(&root.overlay, Some(Overlay::Effort(_))));
    }

    #[test]
    fn review_feedback_is_inserted_without_replacing_the_draft() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(super::RootEvent::ReplaceDraft("existing draft".to_owned()));
        root.update(super::RootEvent::ReviewStarted);

        root.update(super::RootEvent::ReviewFinished(
            "## Review: Approved".to_owned(),
        ));

        assert_eq!(
            root.composer().draft(),
            "existing draft\n\n## Review: Approved"
        );
        assert!(!root.review_active);
    }

    #[test]
    fn review_failure_uses_the_red_notification() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReviewStarted);

        root.update(RootEvent::ReviewFailed(
            "The folder must be a git repository.".to_owned(),
        ));

        let notification = root.notification.as_ref().unwrap();
        let message = notification
            .message
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(message, "The folder must be a git repository.");
        assert_eq!(notification.color, Color::Red);
        assert!(!root.review_active);
    }

    #[test]
    fn review_waiting_is_shown_in_the_composer_instead_of_the_transcript() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReviewStarted);
        root.update(RootEvent::Transcript(agent_record(
            1,
            AgentEventKind::RunStarted,
            json!({}),
        )));
        root.update(RootEvent::Transcript(agent_record(
            2,
            AgentEventKind::RunCompleted,
            json!({}),
        )));

        let rendered = render_root_text(&mut root, 100, 20);
        assert!(rendered.contains("Waiting for review"));
        assert!(!rendered.contains("Waiting for browser review"));
        assert!(!rendered.contains("Preparing review overview"));

        root.update(RootEvent::ReviewCancelled);
        assert!(!render_root_text(&mut root, 100, 20).contains("Waiting for review"));
    }

    #[test]
    fn review_suspends_composer_input_without_clearing_the_draft() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReplaceDraft("keep this draft".to_owned()));
        root.update(RootEvent::ReviewStarted);

        let typed = root.update(key(KeyCode::Char('x'), KeyModifiers::NONE));
        let submitted = root.update(key(KeyCode::Enter, KeyModifiers::NONE));
        let quit = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let pasted = root.update(RootEvent::PasteImage(
            "data:image/png;base64,abc".to_owned(),
        ));

        assert!(typed.effects.is_empty());
        assert!(submitted.effects.is_empty());
        assert!(quit.effects.is_empty());
        assert!(pasted.effects.is_empty());
        assert_eq!(root.composer().draft(), "keep this draft");
    }

    #[test]
    fn review_ready_exposes_a_reopen_action_without_unlocking_input() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReviewStarted);
        root.update(RootEvent::ReviewReady(
            "http://127.0.0.1:4321/review".to_owned(),
        ));

        let rendered = render_root_text(&mut root, 100, 20);
        let update = root.update(key(KeyCode::Char('o'), KeyModifiers::NONE));
        let copy = root.update(key(KeyCode::Char('c'), KeyModifiers::NONE));

        assert!(rendered.contains("Review ready"));
        assert!(rendered.contains("O reopen"));
        assert!(rendered.contains("C copy link"));
        assert!(!rendered.contains("http://127.0.0.1:4321/review"));
        assert_eq!(
            update.effects,
            [RootEffect::OpenLink(
                "http://127.0.0.1:4321/review".to_owned()
            )]
        );
        assert_eq!(
            copy.effects,
            [RootEffect::Copy("http://127.0.0.1:4321/review".to_owned())]
        );
        assert!(root.review_active);
    }

    #[test]
    fn escape_twice_cancels_an_active_review() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReviewStarted);

        let first = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let second = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

        assert!(first.effects.is_empty());
        assert_eq!(second.effects, [RootEffect::CancelReview]);
    }

    #[test]
    fn control_c_twice_exits_during_an_active_review() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReviewStarted);

        let first = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let second = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(first.effects.is_empty());
        assert_eq!(second.effects, [RootEffect::Shutdown]);
    }

    #[test]
    fn control_f_can_fork_during_an_active_review() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReviewStarted);

        let update = root.update(key(KeyCode::Char('f'), KeyModifiers::CONTROL));

        assert_eq!(update.effects, [RootEffect::Fork]);
    }

    #[test]
    fn fork_does_not_inherit_the_active_review() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(RootEvent::ReviewStarted);

        let mut fork = root.fork(Path::new("/work"), ReasoningEffort::Medium);

        assert!(!fork.review_active);
        assert!(!render_root_text(&mut fork, 100, 20).contains("Waiting for review"));
    }
}
