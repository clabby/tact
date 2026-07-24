//! Root layout and component event routing.

use super::{
    actions::{Action, ActionAvailability, ActionsEffect, ActionsEvent, ActionsMenu},
    composer::{Composer, ComposerChromeTarget, ComposerEffect, ComposerEvent},
    effort::{EffortEffect, EffortEvent, EffortSelector},
    file_finder::{FileFinder, FileFinderEffect, FileFinderEvent},
    floating::Floating,
    keybindings::{KeybindingsEffect, KeybindingsEvent, KeybindingsHelp},
    node::{Component, ComponentUpdate, Node, RenderRequest},
    queue::{MessageQueue, QueueEffect, QueueEvent, QueueId},
    selection::{Selection, Surface},
    session_picker::{SessionPicker, SessionPickerEffect, SessionPickerEvent},
    subagents::{SubagentEffect, SubagentOverlay, SubagentTree},
    theme_selector::{ThemeSelector, ThemeSelectorEffect, ThemeSelectorEvent},
    transcript::{Transcript, TranscriptEvent},
};
use crate::{
    config::ReasoningEffort,
    subagents::AgentUpdate,
    tui::{
        context::completed_transcript_tokens,
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
    widgets::{Paragraph, Wrap},
};
use semver::Version;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const ESCAPE_CHORD_TIMEOUT: Duration = Duration::from_millis(500);
const BREADCRUMB_DURATION: Duration = Duration::from_secs(10);

struct Notification {
    message: Line<'static>,
    color: Color,
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
    ContextTokens(u64),
    Transcript(Arc<TranscriptRecord>),
    AgentStreamClosed,
    Subagent(AgentUpdate),
    ReplaceDraft(String),
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
    SessionRestored {
        records: Vec<Arc<TranscriptRecord>>,
        effort: ReasoningEffort,
        fast_mode: bool,
    },
    NotifyError(String),
    NotifySuccess(String),
    UpdateAvailable(Version),
    SteerAdmitted(QueueId),
    SteerPromoted(QueueId),
    SteerFailed {
        id: QueueId,
    },
    AnimationFrame(Instant),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RootEffect {
    Submit(Submission),
    RunShell(String),
    OpenDraftEditor,
    OpenQueueEditor { index: usize, text: String },
    OpenConfigEditor,
    OpenLink(String),
    ReloadConfig,
    NewSession,
    LoadSessions,
    ResumeSession(String),
    Steer { id: QueueId, prompt: Submission },
    PersistSteer(String),
    Copy(String),
    SetEffort(ReasoningEffort),
    SetFastMode(bool),
    SetMaxSubagents(usize),
    SetTheme(ThemeMode),
    Fork,
    CancelTurns,
    Shutdown,
}

enum Overlay {
    Actions(Node<ActionsMenu>),
    Effort(Node<EffortSelector>),
    Theme(Node<ThemeSelector>),
    FileFinder(FileMention),
    Keybindings(Node<KeybindingsHelp>),
    Sessions(Node<SessionPicker>),
    Subagents(SubagentOverlay),
}

struct FileMention {
    finder: Node<FileFinder>,
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
    escape_deadline: Option<Instant>,
    notification: Option<Notification>,
    selection: Selection,
    transcript_area: Rect,
    composer_area: Rect,
    composer_content_area: Rect,
    queue_area: Rect,
    in_flight_turns: usize,
    in_flight_shells: usize,
    fork_available: bool,
    interactive: bool,
    theme_mode: ThemeMode,
    subagents: SubagentTree,
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
            escape_deadline: None,
            notification: None,
            selection: Selection::default(),
            transcript_area: Rect::default(),
            composer_area: Rect::default(),
            composer_content_area: Rect::default(),
            queue_area: Rect::default(),
            in_flight_turns: 0,
            in_flight_shells: 0,
            fork_available: true,
            interactive: true,
            theme_mode: ThemeMode::Auto,
            subagents: SubagentTree::new(thinking),
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
        root.set_max_subagents(self.subagents.max_subagents());
        root.thread = ThreadState::Started;
        root.fork_available = false;
        root.theme_mode = self.theme_mode;
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

    pub(crate) fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.theme_mode = mode;
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        let _ = self
            .composer
            .component_mut()
            .update(ComposerEvent::SetFastMode(enabled));
    }

    pub(crate) fn set_max_subagents(&mut self, limit: usize) {
        self.subagents.set_max_subagents(limit);
    }

    pub(crate) fn reset_session(&mut self, workspace: &Path, thinking: ReasoningEffort) {
        let fork_available = self.fork_available;
        let theme_mode = self.theme_mode;
        let max_subagents = self.subagents.max_subagents();
        *self = Self::new(workspace, thinking);
        self.fork_available = fork_available;
        self.theme_mode = theme_mode;
        self.set_max_subagents(max_subagents);
    }

    pub(crate) fn restore_session(
        &mut self,
        workspace: &Path,
        thinking: ReasoningEffort,
        fast_mode: bool,
        records: Vec<Arc<TranscriptRecord>>,
    ) {
        self.reset_session(workspace, thinking);
        self.set_fast_mode(fast_mode);
        for record in records {
            if let Some(tokens) = completed_transcript_tokens(&record) {
                let _ = self
                    .composer
                    .component_mut()
                    .update(ComposerEvent::ContextTokens(tokens));
            }
            let _ = self
                .transcript
                .component_mut()
                .update(TranscriptEvent::Record(record));
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
            self.escape_deadline,
            self.notification.as_ref().map(|notice| notice.deadline),
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
            composer_area
        };
        self.transcript.render(frame, transcript_area, theme);
        self.queue.render(frame, queue_area, theme);
        self.composer.component_mut().render_focused(
            frame,
            composer_area,
            theme,
            focused
                && !self.transcript.component().tools_focused()
                && !self.queue.component().focused(),
        );
        if let Some(overlay) = &mut self.overlay {
            match overlay {
                Overlay::Actions(actions) => actions.render(frame, area, theme),
                Overlay::Effort(selector) => selector.render(frame, area, theme),
                Overlay::Theme(selector) => selector.render(frame, area, theme),
                Overlay::FileFinder(mention) => mention.finder.render(frame, area, theme),
                Overlay::Keybindings(help) => help.render(frame, area, theme),
                Overlay::Sessions(picker) => picker.render(frame, area, theme),
                Overlay::Subagents(SubagentOverlay::Tree) => {
                    self.subagents.render_tree(frame, area, theme);
                }
                Overlay::Subagents(SubagentOverlay::Transcript(id)) => {
                    self.subagents.render_transcript(*id, frame, area, theme);
                }
            }
        }
        if let Some(selection_area) = self.selection_area() {
            self.selection
                .capture_and_render(frame.buffer_mut(), selection_area);
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
    }

    fn update_terminal(&mut self, mut event: Event) -> ComponentUpdate<RootEffect> {
        if matches!(event, Event::Resize(_, _)) {
            self.selection.clear();
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if is_control_c(&event) {
            if self.overlay.is_none()
                && !self.queue.component().focused()
                && !self.transcript.component().tools_focused()
                && !self.composer.component().draft().is_empty()
            {
                return self.update_composer(
                    ComposerEvent::ReplaceDraft(String::new()),
                    RenderRequest::Immediate,
                );
            }
            return ComponentUpdate {
                effects: vec![RootEffect::Shutdown],
                render: RenderRequest::None,
            };
        }
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
                return ComponentUpdate::render(RenderRequest::Immediate);
            }
            if self.queue.component().focused() {
                return self.update_queue(event);
            }
            if self.transcript.component().tools_focused() {
                self.escape_deadline = None;
                return self.update_transcript(TranscriptEvent::BlurTools);
            }
            return self.update_escape_chord(Instant::now());
        }
        if let Some(update) = self.update_selection_mouse(&mut event) {
            return update;
        }
        if let Some(destination) = self.transcript.component().link_destination(&event) {
            return ComponentUpdate {
                effects: vec![RootEffect::OpenLink(destination.to_string())],
                render: RenderRequest::None,
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
        self.escape_deadline = None;
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
                .update(TranscriptEvent::BlurTools);
            return ComponentUpdate::render(RenderRequest::Immediate);
        }
        if is_left_click_in(&event, self.composer_area) {
            let queue_was_focused = self.queue.component().focused();
            self.queue.component_mut().set_focused(false);
            if self.transcript.component().tools_focused() {
                return self.update_transcript(TranscriptEvent::BlurTools);
            }
            if queue_was_focused {
                return ComponentUpdate::render(RenderRequest::Immediate);
            }
        }
        if let Some(command) = self.transcript.component().tool_command(&event) {
            self.queue.component_mut().set_focused(false);
            return self.update_transcript(TranscriptEvent::Tool(command));
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
                && self.queue.component().is_empty();
            self.overlay = Some(Overlay::Actions(Node::new(ActionsMenu::new(
                ActionAvailability {
                    new_session: new_session_enabled,
                    fork: self.fork_available,
                    fast_mode: self.composer.component().fast_mode(),
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
        if self.transcript.component().tools_focused() {
            return ComponentUpdate::none();
        }
        self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate)
    }

    fn update_selection_mouse(&mut self, event: &mut Event) -> Option<ComponentUpdate<RootEffect>> {
        let Event::Mouse(mouse) = event else {
            return None;
        };
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let (surface, area) = self.surface_at(position)?;
                self.selection.begin(surface, clamp_to(position, area));
                Some(ComponentUpdate::render(RenderRequest::Immediate))
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let area = self.selection_area()?;
                self.selection.drag(position, area);
                Some(ComponentUpdate::render(RenderRequest::Immediate))
            }
            MouseEventKind::Up(MouseButton::Left)
                if self.selection.is_active() || self.selection.is_pending() =>
            {
                let area = self.selection_area()?;
                if !self.selection.finish(position, area) {
                    mouse.kind = MouseEventKind::Down(MouseButton::Left);
                    return None;
                }
                Some(ComponentUpdate {
                    effects: self
                        .selection
                        .take_text()
                        .map(RootEffect::Copy)
                        .into_iter()
                        .collect(),
                    render: RenderRequest::Immediate,
                })
            }
            _ => None,
        }
    }

    fn surface_at(&self, position: Position) -> Option<(Surface, Rect)> {
        if self.composer_content_area.contains(position) {
            return Some((Surface::Composer, self.composer_content_area));
        }
        self.transcript_area
            .contains(position)
            .then_some((Surface::Transcript, self.transcript_area))
    }

    fn selection_area(&self) -> Option<Rect> {
        match self.selection.surface()? {
            Surface::Transcript => Some(self.transcript_area),
            Surface::Composer => Some(self.composer_content_area),
        }
    }

    fn update_escape_chord(&mut self, now: Instant) -> ComponentUpdate<RootEffect> {
        if self.escape_deadline.is_some_and(|deadline| now <= deadline) {
            self.escape_deadline = None;
            return ComponentUpdate {
                effects: vec![RootEffect::CancelTurns],
                render: RenderRequest::None,
            };
        }
        self.escape_deadline = Some(now + ESCAPE_CHORD_TIMEOUT);
        ComponentUpdate::none()
    }

    fn update_overlay(&mut self, event: Event, now: Instant) -> ComponentUpdate<RootEffect> {
        match &self.overlay {
            Some(Overlay::Actions(_)) => self.update_actions(event),
            Some(Overlay::Effort(_)) => self.update_effort(EffortEvent::Terminal { event, now }),
            Some(Overlay::Theme(_)) => {
                self.update_theme_selector(ThemeSelectorEvent::Terminal(event))
            }
            Some(Overlay::FileFinder(_)) => self.update_file_finder(event),
            Some(Overlay::Keybindings(_)) => self.update_keybindings(event),
            Some(Overlay::Sessions(_)) => self.update_session_picker(event),
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
            Some(SubagentEffect::Dismiss) => self.overlay = None,
            Some(SubagentEffect::Inspect(id)) => {
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

        if is_file_mention_edit(&event) {
            let keep_open = file_mention_edit_continues_query(&event);
            let update =
                self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
            let query = if keep_open {
                self.file_mention_query(start)
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

        if !is_file_finder_navigation(&event) {
            self.overlay = None;
            if is_escape(&event) {
                return ComponentUpdate::render(RenderRequest::Immediate);
            }
            return self.update_composer(ComposerEvent::Terminal(event), RenderRequest::Immediate);
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

    fn file_mention_query(&self, start: usize) -> Option<String> {
        let composer = self.composer.component();
        composer
            .draft()
            .get(start..composer.cursor())?
            .strip_prefix('@')
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
            None => {}
        }
        ComponentUpdate {
            effects: Vec::new(),
            render: update.render,
        }
    }

    fn open_effort(&mut self) -> ComponentUpdate<RootEffect> {
        self.overlay = Some(Overlay::Effort(Node::new(EffortSelector::new(
            self.composer.component().effort(),
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

    fn load_sessions(&mut self) -> ComponentUpdate<RootEffect> {
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
            EffortEffect::Apply(effort) => {
                self.transcript.component_mut().set_effort(effort);
                self.subagents.set_effort(effort);
                let _ = self
                    .composer
                    .component_mut()
                    .update(ComposerEvent::SetEffort(effort));
                ComponentUpdate {
                    effects: vec![RootEffect::SetEffort(effort)],
                    render: RenderRequest::Immediate,
                }
            }
        }
    }

    fn update_focus(&mut self) -> ComponentUpdate<RootEffect> {
        let focus_queue = !self.queue.component().focused() && !self.queue.component().is_empty();
        self.queue.component_mut().set_focused(focus_queue);
        let transcript = self.transcript.update(TranscriptEvent::BlurTools);
        ComponentUpdate::render(if focus_queue || transcript.render != RenderRequest::None {
            RenderRequest::Immediate
        } else {
            RenderRequest::None
        })
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
        if self.escape_deadline.is_some_and(|deadline| now >= deadline) {
            self.escape_deadline = None;
        }
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
                .max(notification),
        }
    }

    fn apply_subagent_update(&mut self, update: AgentUpdate) -> ComponentUpdate<RootEffect> {
        let previous_active = self.subagents.active_count();
        if !self.subagents.apply(update) {
            return ComponentUpdate::none();
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
        ComponentUpdate::render(RenderRequest::Immediate)
    }
}

impl Component for RootNode {
    type Event = RootEvent;
    type Effect = RootEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        match event {
            RootEvent::Terminal(event) => self.update_terminal(event),
            RootEvent::PasteImage(data_url) => {
                if self.overlay.is_some() || self.queue.component().focused() {
                    ComponentUpdate::none()
                } else {
                    self.update_composer(
                        ComposerEvent::PasteImage(data_url),
                        RenderRequest::Immediate,
                    )
                }
            }
            RootEvent::ContextTokens(tokens) => self.update_composer(
                ComposerEvent::ContextTokens(tokens),
                RenderRequest::Streaming,
            ),
            RootEvent::Transcript(record) => {
                let steer_applied = record.kind() == "run.steered";
                let mut update = self.update_transcript(TranscriptEvent::Record(record));
                if steer_applied {
                    let applied = self.steer_applied();
                    update.effects.extend(applied.effects);
                    update.render = update.render.max(applied.render);
                }
                update
            }
            RootEvent::AgentStreamClosed => {
                self.update_transcript(TranscriptEvent::AgentStreamClosed)
            }
            RootEvent::Subagent(update) => self.apply_subagent_update(update),
            RootEvent::ReplaceDraft(draft) => {
                self.update_composer(ComposerEvent::ReplaceDraft(draft), RenderRequest::Immediate)
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
            RootEvent::SessionRestored {
                records,
                effort,
                fast_mode,
            } => {
                let workspace = self.workspace.clone();
                self.restore_session(&workspace, effort, fast_mode, records);
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
    let popup = Floating::new("", width, 3, &[])
        .at_top()
        .colors(color, color)
        .render(frame, area, theme);
    frame.render_widget(
        Paragraph::new(message.clone())
            .centered()
            .wrap(Wrap { trim: true }),
        popup.body,
    );
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

fn is_file_finder_navigation(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(
            key.code,
            KeyCode::Enter | KeyCode::Up | KeyCode::Down | KeyCode::Esc
        )
}

fn is_file_mention_edit(event: &Event) -> bool {
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

fn file_mention_edit_continues_query(event: &Event) -> bool {
    match event {
        Event::Key(key) if key.code == KeyCode::Backspace => true,
        Event::Key(key) => {
            matches!(key.code, KeyCode::Char(character) if is_file_query_character(character))
        }
        Event::Paste(text) => text.chars().all(is_file_query_character),
        _ => false,
    }
}

fn is_file_query_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '/')
}

fn is_focus_toggle(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
}

fn is_left_click_in(event: &Event, area: Rect) -> bool {
    let Event::Mouse(mouse) = event else {
        return false;
    };
    mouse.kind == MouseEventKind::Down(MouseButton::Left)
        && area.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
}

fn is_control_c(event: &Event) -> bool {
    is_control_key(event, 'c')
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

#[cfg(test)]
mod tests {
    use super::{
        Component, ComposerChromeTarget, Overlay, RootEffect, RootNode, SubagentOverlay,
        ThreadState,
    };
    use crate::{
        config::ReasoningEffort,
        subagents::{AgentDescriptor, AgentId, AgentOrigin, AgentStatus, AgentUpdate},
        tui::{
            theme::{Theme, ThemeMode},
            transcript::{LocalEvent, TranscriptRecord, TurnId},
        },
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nanocodex::{AgentEvent, AgentEventKind};
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
                .unwrap(),
            },
        )))
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
                origin: AgentOrigin::Spawn,
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
    fn composer_hides_subagents_after_they_stop_running() {
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(super::RootEvent::Subagent(AgentUpdate::Added(
            AgentDescriptor {
                id: AgentId::new(1),
                session_id: "child".to_owned(),
                role: "worker".to_owned(),
                task: "work".to_owned(),
                origin: AgentOrigin::Spawn,
                parent: None,
            },
        )));
        root.update(super::RootEvent::Subagent(AgentUpdate::Status {
            id: AgentId::new(1),
            status: AgentStatus::Completed {
                report: "done".to_owned(),
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
                .unwrap(),
            },
        );
        root.update(super::RootEvent::Transcript(Arc::new(record)));
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
    fn at_inside_a_token_is_inserted_without_opening_the_file_finder() {
        let workspace = tempfile::tempdir().unwrap();
        let mut root = RootNode::new(workspace.path(), ReasoningEffort::Medium);
        for character in "name@example.com".chars() {
            root.update(key(KeyCode::Char(character), KeyModifiers::NONE));
        }

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "name@example.com");
    }

    #[test]
    fn selecting_a_file_inserts_its_relative_path_at_the_composer_cursor() {
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

        root.update(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(root.overlay.is_none());
        assert_eq!(root.composer().draft(), "inspect @notes.md ");
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
    fn control_c_still_shuts_down_while_actions_are_open() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.update(key(KeyCode::Char('/'), KeyModifiers::NONE));

        let update = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(update.effects, [super::RootEffect::Shutdown]);
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

        let shutdown = root.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(shutdown.effects, [RootEffect::Shutdown]);
    }

    #[test]
    fn double_escape_interrupts_without_shutting_down() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);

        let first = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        let second = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

        assert!(first.effects.is_empty());
        assert_eq!(second.effects, [RootEffect::CancelTurns]);
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

        root.update(mouse(MouseEventKind::Down(MouseButton::Left), 10, 9));
        let update = root.update(mouse(MouseEventKind::Up(MouseButton::Left), 10, 9));

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
    fn escape_chord_expires_and_unrelated_input_resets_it() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        let now = Instant::now();

        assert!(root.update_escape_chord(now).effects.is_empty());
        assert!(
            root.update_escape_chord(now + Duration::from_millis(501))
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
            [RootEffect::SetEffort(ReasoningEffort::High)]
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
    fn escape_that_blurs_tools_does_not_start_the_interrupt_chord() {
        let mut root = RootNode::new(Path::new("/work"), ReasoningEffort::Medium);
        root.transcript.component_mut().focus_tools();

        let blurred = root.update(key(KeyCode::Esc, KeyModifiers::NONE));

        assert!(blurred.effects.is_empty());
        assert!(!root.transcript.component().tools_focused());
        assert!(root.escape_deadline.is_none());

        let chord_started = root.update(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(chord_started.effects.is_empty());
        assert!(root.escape_deadline.is_some());
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

        root.reset_session(Path::new("/work"), ReasoningEffort::Medium);

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
}
