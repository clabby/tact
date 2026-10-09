//! Application-level ownership of every live session pane and the terminal layout.
//!
//! Any number of panes can be live. At most two are drawn: the primary pane and, after a fork,
//! a secondary pane beside it. The focused pane is the shared active session that the web
//! interface follows. Background panes keep receiving records and keep their composer, queue,
//! and in-flight work; they are only not drawn.
//!
//! This node is also the single writer of shared state: [`AppNode::publish_changes`] compares
//! each live session with what was last published and sends only real changes.

use super::{
    composer::LiveSessions,
    confirmation::{Confirmation, ConfirmationEffect, ConfirmationEvent},
    node::{Component, ComponentUpdate, RenderRequest},
    root::{DraftReset, PaneCommand, RestoredSessionProjection, RootEffect, RootEvent, RootNode},
    sessions::{LiveSession, SessionsEffect, SessionsEvent, SessionsOverlay},
    subagents::subagent_record,
};
use crate::{
    app::{
        config::{ReasoningEffort, ReasoningMode, Speed, TuiConfig},
        model,
        theme::{ColorScheme, Theme, ThemeMode},
    },
    core::{
        context::ContextBudget,
        extensions::Skill,
        pane::PaneId,
        prompt::QueueId,
        protocol::{
            Busy, Command, CommandError, Draft, DraftImage, Origin, Publication, Publisher,
            QueuedPrompt, SessionInfo,
        },
        session::{RecentPrompt, SessionSummary},
        transcript::TranscriptRecord,
    },
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use nanocodex::HarnessModel as Model;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders},
};
use semver::Version;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};
use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord, MemorySource};
use tact_subagents::{AgentUpdate, SubagentRoster};
use unicode_width::UnicodeWidthStr;

const SPLIT_HINT: &str = " mouse: focus · Ctrl+C: clear · Ctrl+C×2: close ";
const MIN_SPLIT_HINT_WIDTH: u16 = 60;
const UNTITLED_SESSION: &str = "New session";

pub(crate) enum AppEvent {
    ContextBudget {
        pane: PaneId,
        budget: ContextBudget,
    },
    Terminal(Event),
    PasteImage(String),
    Transcript {
        pane: PaneId,
        record: Arc<TranscriptRecord>,
    },
    AgentStreamClosed(PaneId),
    Subagent {
        pane: PaneId,
        update: AgentUpdate,
    },
    EditorDraft {
        pane: PaneId,
        draft: String,
    },
    HandoffReady {
        pane: PaneId,
        prompt: String,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        speed: Speed,
        model: Model,
        skills: Arc<[Skill]>,
    },
    HandoffCancelled(PaneId),
    HandoffFailed {
        pane: PaneId,
        error: String,
    },
    CompactionFinished(PaneId),
    WorkerTurnFinished {
        pane: PaneId,
        terminal_expected: bool,
    },
    ShellFinished(PaneId),
    TurnsCancelled(PaneId),
    SteerAdmitted {
        pane: PaneId,
        id: QueueId,
    },
    SteerPromoted {
        pane: PaneId,
        id: QueueId,
    },
    SteerFailed {
        pane: PaneId,
        id: QueueId,
    },
    ForkReady {
        pane: PaneId,
    },
    ForkFailed {
        pane: PaneId,
        error: String,
    },
    /// A pane opened for a new or resumed session could not start it.
    OpenFailed {
        pane: PaneId,
        error: String,
    },
    NewSessionReady {
        pane: PaneId,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        speed: Speed,
        model: Model,
        draft_reset: DraftReset,
        skills: Arc<[Skill]>,
    },
    NewSessionFailed {
        pane: PaneId,
        error: String,
    },
    SessionsLoaded {
        pane: PaneId,
        sessions: Vec<SessionSummary>,
    },
    RecentPromptsLoaded {
        pane: PaneId,
        session_id: String,
        prompts: Vec<RecentPrompt>,
    },
    RecentPromptLoadFailed {
        pane: PaneId,
        error: String,
    },
    SessionLoadFailed {
        pane: PaneId,
        error: String,
    },
    MemoriesLoaded {
        pane: PaneId,
        access: MemoryAccess,
        records: Vec<MemoryRecord>,
    },
    MemoryLoadFailed {
        pane: PaneId,
        source: MemorySource,
        access: Option<MemoryAccess>,
        error: String,
    },
    MemoryDeleted {
        pane: PaneId,
        key: MemoryKey,
    },
    MemoryDeleteFailed {
        pane: PaneId,
        error: String,
        conflict: bool,
    },
    SessionRestored {
        pane: PaneId,
        projection: Box<RestoredSessionProjection>,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        preferred_reasoning_mode: ReasoningMode,
        speed: Speed,
        model: Model,
        skills: Arc<[Skill]>,
    },
    EffortUpdateFailed {
        pane: PaneId,
        effort: ReasoningEffort,
        error: String,
    },
    NotifyError {
        pane: PaneId,
        error: String,
    },
    NotifySuccess {
        pane: PaneId,
        message: String,
    },
    ShowWebQr {
        pane: PaneId,
        link: String,
    },
    ConfirmWebInstall {
        pane: PaneId,
    },
    UpdateAvailable(Version),
    ConfigReloaded {
        pane: PaneId,
        theme: Theme,
        tui: TuiConfig,
        preferred_reasoning_mode: ReasoningMode,
        memory_enabled: bool,
        message: String,
    },
    ConfigReloadFailed {
        pane: PaneId,
        error: String,
    },
    SystemThemeChanged(ColorScheme),
    AnimationFrame(Instant),
}

#[derive(Debug, PartialEq)]
pub(crate) enum AppEffect {
    Pane {
        pane: PaneId,
        effect: RootEffect,
    },
    OpenFork {
        pane: PaneId,
        parent: PaneId,
    },
    /// Configures a fresh agent for a pane created by [`AppNode::begin_open`].
    StartSession {
        pane: PaneId,
        model: Model,
    },
    ClosePane(PaneId),
    SetTheme(ThemeMode),
    Shutdown,
}

enum AppOverlay {
    Sessions(SessionsOverlay),
    Quit(Confirmation),
}

#[derive(Clone, Debug, PartialEq)]
struct Settings {
    model: Model,
    effort: ReasoningEffort,
    reasoning_mode: ReasoningMode,
    speed: Speed,
}

impl Settings {
    fn of(root: &RootNode) -> Self {
        let composer = root.composer();
        Self {
            model: composer.model(),
            effort: composer.effort(),
            reasoning_mode: composer.reasoning_mode(),
            speed: composer.speed(),
        }
    }
}

/// The last published state of a live session.
struct Published {
    session: String,
    draft: Draft,
    queue: Vec<QueuedPrompt>,
    settings: Settings,
    busy: Busy,
    /// `None` until first published, so a newly opened session announces both.
    context: Option<ContextBudget>,
    subagents: Option<SubagentRoster>,
}

struct Pane {
    root: RootNode,
    /// Orders panes by when they were opened.
    sequence: u64,
    /// Absent while the pane's session is still opening.
    published: Option<Published>,
    /// Work finished while the pane was not the active session.
    unread: bool,
}

pub(crate) struct AppNode {
    theme: Theme,
    workspace: PathBuf,
    panes: HashMap<PaneId, Pane>,
    /// Drawn alone, or on the left of a split.
    primary: Option<PaneId>,
    /// Drawn on the right of a split.
    secondary: Option<PaneId>,
    /// The pane receiving keyboard input: the shared active session.
    focus: PaneId,
    /// Every pane, from least to most recently active.
    recency: Vec<PaneId>,
    main_area: Rect,
    fork_area: Rect,
    next_pane: u64,
    max_live_sessions: usize,
    overlay: Option<AppOverlay>,
    publisher: Option<Publisher>,
    /// The last published active session.
    active: Option<String>,
}

impl AppNode {
    pub(crate) fn new(theme: Theme, workspace: PathBuf, mut root: RootNode) -> Self {
        root.set_theme_mode(theme.mode());
        let mut app = Self {
            theme,
            workspace,
            panes: HashMap::from([(
                PaneId::Main,
                Pane {
                    root,
                    sequence: 0,
                    published: None,
                    unread: false,
                },
            )]),
            primary: Some(PaneId::Main),
            secondary: None,
            focus: PaneId::Main,
            recency: vec![PaneId::Main],
            main_area: Rect::default(),
            fork_area: Rect::default(),
            next_pane: 1,
            max_live_sessions: 8,
            overlay: None,
            publisher: None,
            active: None,
        };
        app.refresh_layout();
        app
    }

    /// Mirrors every later change to shared state through `publisher`.
    pub(crate) fn attach_publisher(&mut self, publisher: Publisher) {
        self.publisher = Some(publisher);
    }

    pub(crate) fn set_max_live_sessions(&mut self, limit: usize) {
        self.max_live_sessions = limit;
        self.refresh_layout();
    }

    pub(crate) fn open_resume_selector(&mut self) -> ComponentUpdate<AppEffect> {
        let pane = self.focus;
        let Some(entry) = self.panes.get_mut(&pane) else {
            return ComponentUpdate::none();
        };
        let update = entry.root.load_sessions();
        self.map_root_update(pane, update)
    }

    pub(crate) fn update(&mut self, event: AppEvent) -> ComponentUpdate<AppEffect> {
        match event {
            AppEvent::Terminal(event) => self.update_terminal(event),
            AppEvent::PasteImage(data_url) => {
                if self.overlay.is_some() {
                    return ComponentUpdate::none();
                }
                self.update_root(self.focus, RootEvent::PasteImage(data_url))
            }
            AppEvent::ContextBudget { pane, budget } => {
                self.update_root(pane, RootEvent::ContextBudget(budget))
            }
            AppEvent::Transcript { pane, record } => {
                if let (Some(publisher), Some(published)) = (
                    &self.publisher,
                    self.panes
                        .get(&pane)
                        .and_then(|entry| entry.published.as_ref()),
                ) {
                    publisher.publish(Publication::Record {
                        session: published.session.clone(),
                        record: Arc::clone(&record),
                    });
                }
                self.update_root(pane, RootEvent::Transcript(record))
            }
            AppEvent::AgentStreamClosed(pane) => {
                self.update_root(pane, RootEvent::AgentStreamClosed)
            }
            AppEvent::Subagent {
                pane,
                update: AgentUpdate::Event { id, event },
            } => {
                let record = subagent_record(event);
                if let (Some(publisher), Some(published)) = (
                    &self.publisher,
                    self.panes
                        .get(&pane)
                        .and_then(|entry| entry.published.as_ref()),
                ) {
                    publisher.publish(Publication::SubagentRecord {
                        session: published.session.clone(),
                        agent: id,
                        record: Arc::clone(&record),
                    });
                }
                self.update_root(pane, RootEvent::SubagentRecord { id, record })
            }
            AppEvent::Subagent { pane, update } => {
                if let (AgentUpdate::Message(message), Some(publisher), Some(published)) = (
                    &update,
                    &self.publisher,
                    self.panes
                        .get(&pane)
                        .and_then(|entry| entry.published.as_ref()),
                ) {
                    publisher.publish(Publication::Message {
                        session: published.session.clone(),
                        update: message.clone(),
                    });
                }
                self.update_root(pane, RootEvent::Subagent(update))
            }
            AppEvent::EditorDraft { pane, draft } => {
                self.update_root(pane, RootEvent::ReplaceDraft(draft))
            }
            AppEvent::HandoffReady {
                pane,
                prompt,
                effort,
                reasoning_mode,
                speed,
                model,
                skills,
            } => {
                self.reset_session(
                    pane,
                    effort,
                    reasoning_mode,
                    speed,
                    model,
                    DraftReset::Clear,
                    skills,
                );
                self.update_root(pane, RootEvent::HandoffFinished(prompt))
            }
            AppEvent::HandoffCancelled(pane) => self.update_root(pane, RootEvent::HandoffCancelled),
            AppEvent::HandoffFailed { pane, error } => {
                self.update_root(pane, RootEvent::HandoffFailed(error))
            }
            AppEvent::CompactionFinished(pane) => {
                self.update_root(pane, RootEvent::CompactionFinished)
            }
            AppEvent::WorkerTurnFinished {
                pane,
                terminal_expected,
            } => self.update_root(pane, RootEvent::WorkerTurnFinished { terminal_expected }),
            AppEvent::ShellFinished(pane) => self.update_root(pane, RootEvent::ShellFinished),
            AppEvent::TurnsCancelled(pane) => self.update_root(pane, RootEvent::TurnsCancelled),
            AppEvent::SteerAdmitted { pane, id } => {
                self.update_root(pane, RootEvent::SteerAdmitted(id))
            }
            AppEvent::SteerPromoted { pane, id } => {
                self.update_root(pane, RootEvent::SteerPromoted(id))
            }
            AppEvent::SteerFailed { pane, id } => {
                self.update_root(pane, RootEvent::SteerFailed { id })
            }
            AppEvent::ForkReady { pane } => self.update_root(pane, RootEvent::ForkReady),
            AppEvent::ForkFailed { pane, error } => {
                self.abandon_pane(pane, format!("Could not fork session: {error}"))
            }
            AppEvent::OpenFailed { pane, error } => {
                self.abandon_pane(pane, format!("Could not open session: {error}"))
            }
            AppEvent::NewSessionReady {
                pane,
                effort,
                reasoning_mode,
                speed,
                model,
                draft_reset,
                skills,
            } => {
                if !self.panes.contains_key(&pane) {
                    return ComponentUpdate::none();
                }
                self.reset_session(
                    pane,
                    effort,
                    reasoning_mode,
                    speed,
                    model,
                    draft_reset,
                    skills,
                );
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            AppEvent::NewSessionFailed { pane, error } => {
                self.update_root(pane, RootEvent::NewSessionFailed(error))
            }
            AppEvent::SessionsLoaded { pane, sessions } => {
                self.update_root(pane, RootEvent::SessionsLoaded(sessions))
            }
            AppEvent::RecentPromptsLoaded {
                pane,
                session_id,
                prompts,
            } => self.update_root(
                pane,
                RootEvent::RecentPromptsLoaded {
                    session_id,
                    prompts,
                },
            ),
            AppEvent::RecentPromptLoadFailed { pane, error } => {
                self.update_root(pane, RootEvent::RecentPromptLoadFailed(error))
            }
            AppEvent::SessionLoadFailed { pane, error } => {
                self.update_root(pane, RootEvent::SessionLoadFailed(error))
            }
            AppEvent::MemoriesLoaded {
                pane,
                access,
                records,
            } => self.update_root(pane, RootEvent::MemoriesLoaded { access, records }),
            AppEvent::MemoryLoadFailed {
                pane,
                source,
                access,
                error,
            } => self.update_root(
                pane,
                RootEvent::MemoryLoadFailed {
                    source,
                    access,
                    error,
                },
            ),
            AppEvent::MemoryDeleted { pane, key } => {
                self.update_root(pane, RootEvent::MemoryDeleted { key })
            }
            AppEvent::MemoryDeleteFailed {
                pane,
                error,
                conflict,
            } => self.update_root(pane, RootEvent::MemoryDeleteFailed { error, conflict }),
            AppEvent::SessionRestored {
                pane,
                projection,
                effort,
                reasoning_mode,
                preferred_reasoning_mode,
                speed,
                model,
                skills,
            } => self.update_root(
                pane,
                RootEvent::SessionRestored {
                    projection,
                    effort,
                    reasoning_mode,
                    preferred_reasoning_mode,
                    speed,
                    model,
                    skills,
                },
            ),
            AppEvent::EffortUpdateFailed {
                pane,
                effort,
                error,
            } => self.update_root(pane, RootEvent::EffortUpdateFailed { effort, error }),
            AppEvent::NotifyError { pane, error } => {
                self.update_root(pane, RootEvent::NotifyError(error))
            }
            AppEvent::NotifySuccess { pane, message } => {
                self.update_root(pane, RootEvent::NotifySuccess(message))
            }
            AppEvent::ShowWebQr { pane, link } => {
                self.update_root(pane, RootEvent::ShowQrCode(link))
            }
            AppEvent::ConfirmWebInstall { pane } => {
                self.update_root(pane, RootEvent::ConfirmWebInstall)
            }
            AppEvent::UpdateAvailable(version) => {
                self.update_root(self.focus, RootEvent::UpdateAvailable(version))
            }
            AppEvent::ConfigReloaded {
                pane,
                theme,
                tui,
                preferred_reasoning_mode,
                memory_enabled,
                message,
            } => {
                self.theme.replace_from_config(theme);
                let mode = self.theme.mode();
                for root in self.roots_mut() {
                    root.set_theme_mode(mode);
                    root.set_tui_config(tui);
                }
                self.set_preferred_reasoning_mode(preferred_reasoning_mode);
                self.set_memory_enabled(false);
                self.set_memory_enabled(memory_enabled);
                self.update_root(pane, RootEvent::NotifySuccess(message))
            }
            AppEvent::ConfigReloadFailed { pane, error } => {
                self.update_root(pane, RootEvent::NotifyError(error))
            }
            AppEvent::SystemThemeChanged(scheme) => {
                if self.theme.set_system_scheme(scheme) {
                    ComponentUpdate::render(RenderRequest::Immediate)
                } else {
                    ComponentUpdate::none()
                }
            }
            AppEvent::AnimationFrame(now) => {
                self.update_panes(&self.displayed(), || RootEvent::AnimationFrame(now))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reset_session(
        &mut self,
        pane: PaneId,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        speed: Speed,
        model: Model,
        draft_reset: DraftReset,
        skills: Arc<[Skill]>,
    ) {
        let Some(root) = self.pane_mut(pane) else {
            return;
        };
        let workspace = root.workspace().to_owned();
        let preferred_reasoning_mode = root.preferred_reasoning_mode();
        root.reset_session(
            &workspace,
            effort,
            reasoning_mode,
            preferred_reasoning_mode,
            draft_reset,
        );
        root.set_speed(speed);
        root.set_model(model);
        root.set_skills(skills);
        self.refresh_layout();
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.is_empty() {
            self.main_area = Rect::default();
            self.fork_area = Rect::default();
            return;
        }
        let focus = self.focus;
        match (self.primary, self.secondary) {
            (Some(primary), Some(secondary)) => {
                let divider_x = area.x + area.width.saturating_sub(1) / 2;
                self.main_area = Rect::new(
                    area.x,
                    area.y,
                    divider_x.saturating_sub(area.x),
                    area.height,
                );
                self.fork_area = Rect::new(
                    divider_x.saturating_add(1),
                    area.y,
                    area.right().saturating_sub(divider_x.saturating_add(1)),
                    area.height,
                );
                let hint_height = u16::from(Self::split_hint_visible(area));
                for (pane, pane_area) in [(primary, self.main_area), (secondary, self.fork_area)] {
                    let content = Rect {
                        y: pane_area.y.saturating_add(hint_height),
                        height: pane_area.height.saturating_sub(hint_height),
                        ..pane_area
                    };
                    if let Some(entry) = self.panes.get_mut(&pane) {
                        entry
                            .root
                            .render_focused(frame, content, &self.theme, focus == pane);
                    }
                }
                frame.render_widget(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(self.theme.border())),
                    Rect::new(divider_x, area.y, 1, area.height),
                );
                self.render_split_hint(frame, area, divider_x);
            }
            (primary, _) => {
                self.main_area = area;
                self.fork_area = Rect::default();
                if let Some(entry) = primary.and_then(|pane| self.panes.get_mut(&pane)) {
                    entry
                        .root
                        .render_focused(frame, area, &self.theme, primary == Some(focus));
                }
            }
        }
        match &mut self.overlay {
            Some(AppOverlay::Sessions(overlay)) => overlay.render(frame, area, &self.theme),
            Some(AppOverlay::Quit(confirmation)) => confirmation.render(frame, area, &self.theme),
            None => {}
        }
    }

    pub(crate) fn root(&self, pane: PaneId) -> Option<&RootNode> {
        self.panes.get(&pane).map(|entry| &entry.root)
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        self.displayed()
            .into_iter()
            .filter_map(|pane| self.root(pane)?.animation_deadline())
            .min()
    }

    fn update_terminal(&mut self, event: Event) -> ComponentUpdate<AppEffect> {
        if matches!(event, Event::FocusGained) {
            self.refresh_terminal_images();
        }
        if matches!(event, Event::Resize(_, _)) {
            let panes = self.panes.keys().copied().collect::<Vec<_>>();
            let mut update = self.update_panes(&panes, || RootEvent::Terminal(event.clone()));
            update.render = RenderRequest::Immediate;
            return update;
        }
        if self.overlay.is_some() {
            return self.update_overlay(event);
        }
        if self.secondary.is_some() && is_control_c(&event) {
            let pane = self.focus;
            let Some(entry) = self.panes.get_mut(&pane) else {
                return ComponentUpdate::none();
            };
            let update = entry.root.update(RootEvent::Terminal(event));
            if !matches!(update.effects.as_slice(), [RootEffect::Shutdown]) {
                return self.map_root_update(pane, update);
            }
            self.remove_pane(pane);
            return ComponentUpdate {
                effects: vec![AppEffect::ClosePane(pane)],
                render: RenderRequest::Immediate,
            };
        }
        if let Event::Mouse(mouse) = &event
            && matches!(mouse.kind, MouseEventKind::Down(_))
        {
            let position = Position::new(mouse.column, mouse.row);
            let clicked = if self.fork_area.contains(position) {
                self.secondary
            } else if self.main_area.contains(position) {
                self.primary
            } else {
                None
            };
            if let Some(pane) = clicked {
                self.activate(pane);
            }
        }
        self.update_root(self.focus, RootEvent::Terminal(event))
    }

    fn update_overlay(&mut self, event: Event) -> ComponentUpdate<AppEffect> {
        match &mut self.overlay {
            Some(AppOverlay::Sessions(overlay)) => {
                let update = overlay.update(SessionsEvent::Terminal(event));
                let Some(effect) = update.effects.into_iter().next() else {
                    return ComponentUpdate::render(update.render);
                };
                self.update_sessions_overlay(effect)
            }
            Some(AppOverlay::Quit(confirmation)) => {
                let update = confirmation.update(ConfirmationEvent::Terminal(event));
                let Some(effect) = update.effects.into_iter().next() else {
                    return ComponentUpdate::render(update.render);
                };
                self.overlay = None;
                ComponentUpdate {
                    effects: match effect {
                        ConfirmationEffect::Confirm => vec![AppEffect::Shutdown],
                        ConfirmationEffect::Dismiss => Vec::new(),
                    },
                    render: RenderRequest::Immediate,
                }
            }
            None => ComponentUpdate::none(),
        }
    }

    fn update_sessions_overlay(&mut self, effect: SessionsEffect) -> ComponentUpdate<AppEffect> {
        match effect {
            SessionsEffect::Cancel => {
                self.overlay = None;
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            SessionsEffect::Activate(pane) => {
                self.overlay = None;
                self.activate(pane);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            SessionsEffect::New => {
                self.overlay = None;
                let model = self.root(self.focus).map(|root| root.composer().model());
                match (model, self.begin_open("Starting new session…")) {
                    (Some(model), Ok(pane)) => ComponentUpdate {
                        effects: vec![AppEffect::StartSession { pane, model }],
                        render: RenderRequest::Immediate,
                    },
                    (_, Err(error)) => self.notify_error(error.to_string()),
                    (None, Ok(_)) => {
                        unreachable!("a focused pane exists while the overlay is open")
                    }
                }
            }
            SessionsEffect::Close(pane) => match self.close_pane(pane, false) {
                Ok(update) => {
                    self.refresh_sessions_overlay();
                    update
                }
                Err(CommandError::TurnRunning) => {
                    self.notify_error("Interrupt the session before closing it.".to_owned())
                }
                Err(error) => self.notify_error(format!("Could not close the session: {error}")),
            },
        }
    }

    fn notify_error(&mut self, error: String) -> ComponentUpdate<AppEffect> {
        self.update_root(self.focus, RootEvent::NotifyError(error))
    }

    fn open_sessions_overlay(&mut self) {
        let overlay = SessionsOverlay::new(self.live_sessions(), self.new_session_availability());
        self.overlay = Some(AppOverlay::Sessions(overlay));
    }

    fn refresh_sessions_overlay(&mut self) {
        let sessions = self.live_sessions();
        let availability = self.new_session_availability();
        if let Some(AppOverlay::Sessions(overlay)) = &mut self.overlay {
            overlay.set_sessions(sessions, availability);
        }
    }

    fn new_session_availability(&self) -> Result<(), String> {
        self.can_open()
            .map_err(|_| format!("at most {} live sessions", self.max_live_sessions))
    }

    fn live_sessions(&self) -> Vec<LiveSession> {
        let displayed = self.displayed();
        let mut panes = self.panes.iter().collect::<Vec<_>>();
        panes.sort_by_key(|(_, entry)| entry.sequence);
        panes
            .into_iter()
            .map(|(&pane, entry)| {
                let root = &entry.root;
                let busy = root.busy();
                LiveSession {
                    pane,
                    title: root.title().unwrap_or(UNTITLED_SESSION).to_owned(),
                    model: model::name(root.composer().model()).to_owned(),
                    running: busy.turns > 0 || busy.shells > 0,
                    unread: entry.unread,
                    has_draft: !root.shared_draft().trim().is_empty(),
                    displayed: displayed.contains(&pane),
                    focused: pane == self.focus,
                }
            })
            .collect()
    }

    pub(crate) fn refresh_terminal_images(&mut self) {
        for root in self.roots_mut() {
            root.refresh_terminal_images();
        }
    }

    fn render_split_hint(&self, frame: &mut Frame<'_>, area: Rect, divider_x: u16) {
        let width = u16::try_from(SPLIT_HINT.width()).unwrap_or(u16::MAX);
        if !Self::split_hint_visible(area) {
            return;
        }

        let x = divider_x.saturating_sub(width / 2).max(area.x);
        frame.buffer_mut().set_string(
            x,
            area.y,
            SPLIT_HINT,
            Style::default()
                .fg(self.theme.muted())
                .add_modifier(Modifier::DIM),
        );
    }

    fn split_hint_visible(area: Rect) -> bool {
        area.width >= MIN_SPLIT_HINT_WIDTH
            && area.height >= 2
            && SPLIT_HINT.width() <= usize::from(area.width)
    }

    fn displayed(&self) -> Vec<PaneId> {
        self.primary.into_iter().chain(self.secondary).collect()
    }

    fn update_panes(
        &mut self,
        panes: &[PaneId],
        event: impl Fn() -> RootEvent,
    ) -> ComponentUpdate<AppEffect> {
        let mut merged = ComponentUpdate::none();
        for &pane in panes {
            merged.merge(self.update_root(pane, event()));
        }
        merged
    }

    fn update_root(&mut self, pane: PaneId, event: RootEvent) -> ComponentUpdate<AppEffect> {
        let Some(root) = self.pane_mut(pane) else {
            return ComponentUpdate::none();
        };
        let update = root.update(event);
        self.map_root_update(pane, update)
    }

    fn map_root_update(
        &mut self,
        pane: PaneId,
        update: ComponentUpdate<RootEffect>,
    ) -> ComponentUpdate<AppEffect> {
        let mut effects = Vec::with_capacity(update.effects.len());
        let mut render = update.render;
        for effect in update.effects {
            match effect {
                RootEffect::Fork => {
                    if let Some(fork) = self.begin_split_fork(pane) {
                        effects.push(AppEffect::OpenFork {
                            pane: fork,
                            parent: pane,
                        });
                    }
                }
                RootEffect::OpenSessions => {
                    self.open_sessions_overlay();
                    render = RenderRequest::Immediate;
                }
                RootEffect::Shutdown => {
                    let running = self.running_background_sessions();
                    if running == 0 {
                        effects.push(AppEffect::Shutdown);
                    } else {
                        self.overlay = Some(AppOverlay::Quit(
                            Confirmation::quit_with_running_sessions(running),
                        ));
                        render = RenderRequest::Immediate;
                    }
                }
                RootEffect::SetTheme(mode) => {
                    self.set_theme_mode(mode);
                    effects.push(AppEffect::SetTheme(mode));
                }
                effect => effects.push(AppEffect::Pane { pane, effect }),
            }
        }
        ComponentUpdate { effects, render }
    }

    fn running_background_sessions(&self) -> usize {
        let displayed = self.displayed();
        self.panes
            .iter()
            .filter(|(pane, entry)| {
                let busy = entry.root.busy();
                !displayed.contains(pane) && (busy.turns > 0 || busy.shells > 0)
            })
            .count()
    }

    fn roots_mut(&mut self) -> impl Iterator<Item = &mut RootNode> {
        self.panes.values_mut().map(|entry| &mut entry.root)
    }

    fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.theme.set_mode(mode);
        for root in self.roots_mut() {
            root.set_theme_mode(mode);
        }
    }

    pub(crate) fn set_claude_enabled(&mut self, enabled: bool) {
        for root in self.roots_mut() {
            root.set_claude_enabled(enabled);
        }
    }

    pub(crate) fn set_max_subagents(&mut self, limit: usize) {
        for root in self.roots_mut() {
            root.set_max_subagents(limit);
        }
    }

    pub(crate) fn set_preferred_reasoning_mode(&mut self, mode: ReasoningMode) {
        for root in self.roots_mut() {
            root.set_preferred_reasoning_mode(mode);
        }
    }

    pub(crate) fn set_memory_enabled(&mut self, enabled: bool) {
        for root in self.roots_mut() {
            root.set_memory_enabled(enabled);
        }
    }

    pub(crate) fn can_open(&self) -> Result<(), CommandError> {
        if self.panes.len() >= self.max_live_sessions {
            return Err(CommandError::TooManySessions);
        }
        Ok(())
    }

    fn insert_pane(&mut self, pane: PaneId, root: RootNode) {
        let sequence = self.next_pane;
        self.next_pane = self.next_pane.saturating_add(1);
        self.panes.insert(
            pane,
            Pane {
                root,
                sequence,
                published: None,
                unread: false,
            },
        );
    }

    fn fork_root(&self, parent: PaneId) -> Option<RootNode> {
        let parent = self.root(parent)?;
        Some(parent.fork(parent.workspace(), parent.composer().effort()))
    }

    /// The keyboard fork opens beside its parent, so it needs the split to be free.
    fn begin_split_fork(&mut self, parent: PaneId) -> Option<PaneId> {
        if self.primary != Some(parent) || self.secondary.is_some() || self.can_open().is_err() {
            return None;
        }
        let root = self.fork_root(parent)?;
        let pane = PaneId::Fork(self.next_pane);
        self.insert_pane(pane, root);
        self.secondary = Some(pane);
        self.activate(pane);
        Some(pane)
    }

    /// Opens a fork of `parent` as the active session. The caller asks the worker to fork.
    pub(crate) fn begin_fork(&mut self, parent: PaneId) -> Result<PaneId, CommandError> {
        self.can_open()?;
        let root = self.fork_root(parent).ok_or(CommandError::UnknownSession)?;
        let pane = PaneId::Fork(self.next_pane);
        self.insert_pane(pane, root);
        self.activate(pane);
        Ok(pane)
    }

    pub(crate) fn set_pane_workspace(&mut self, pane: PaneId, workspace: PathBuf) {
        if let Some(root) = self.pane_mut(pane) {
            root.set_workspace(workspace);
        }
    }

    /// Opens an empty, non-interactive pane as the active session. The caller starts its session
    /// and reports [`AppEvent::NewSessionReady`], [`AppEvent::SessionRestored`], or
    /// [`AppEvent::OpenFailed`].
    pub(crate) fn begin_open(&mut self, status: &str) -> Result<PaneId, CommandError> {
        self.can_open()?;
        let template = self.root(self.focus).ok_or_else(|| {
            CommandError::Failed("no session to inherit settings from".to_owned())
        })?;
        let mut root = template.sibling(&self.workspace);
        root.begin_opening(status);
        let pane = PaneId::Opened(self.next_pane);
        self.insert_pane(pane, root);
        self.activate(pane);
        Ok(pane)
    }

    /// Makes `pane` the active session. A background pane takes the focused slot and the pane it
    /// displaces keeps running in the background.
    pub(crate) fn activate(&mut self, pane: PaneId) -> bool {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return false;
        };
        entry.unread = false;
        if self.primary != Some(pane) && self.secondary != Some(pane) {
            if self.secondary.is_some() && self.secondary == Some(self.focus) {
                self.secondary = Some(pane);
            } else {
                self.primary = Some(pane);
            }
        }
        self.focus = pane;
        self.recency.retain(|recent| *recent != pane);
        self.recency.push(pane);
        self.refresh_layout();
        true
    }

    /// Closes `pane` unless work is in flight and `force` is unset.
    pub(crate) fn close_pane(
        &mut self,
        pane: PaneId,
        force: bool,
    ) -> Result<ComponentUpdate<AppEffect>, CommandError> {
        let busy = self.root(pane).ok_or(CommandError::UnknownSession)?.busy();
        if !force && (busy.turns > 0 || busy.shells > 0) {
            return Err(CommandError::TurnRunning);
        }
        if self.panes.len() == 1 {
            return Err(CommandError::Invalid(
                "the last session cannot be closed; quit Tact instead".to_owned(),
            ));
        }
        self.remove_pane(pane);
        Ok(ComponentUpdate {
            effects: vec![AppEffect::ClosePane(pane)],
            render: RenderRequest::Immediate,
        })
    }

    fn abandon_pane(&mut self, pane: PaneId, message: String) -> ComponentUpdate<AppEffect> {
        self.remove_pane(pane);
        if self.panes.is_empty() {
            return ComponentUpdate {
                effects: vec![AppEffect::Shutdown],
                render: RenderRequest::Immediate,
            };
        }
        self.notify_error(message)
    }

    /// Forgets `pane`. When it was the active session, the most recently active remaining pane
    /// becomes active, taking the freed slot if it was in the background.
    fn remove_pane(&mut self, pane: PaneId) {
        let Some(removed) = self.panes.remove(&pane) else {
            return;
        };
        if let (Some(publisher), Some(published)) = (&self.publisher, removed.published) {
            publisher.publish(Publication::Closed {
                session: published.session,
            });
        }
        self.recency.retain(|recent| *recent != pane);
        let freed_secondary = if self.primary == Some(pane) {
            self.primary = self.secondary.take();
            self.primary.is_some()
        } else if self.secondary == Some(pane) {
            self.secondary = None;
            true
        } else {
            false
        };
        let next = self.recency.last().copied();
        if self.focus == pane
            && let Some(next) = next
        {
            if self.primary.is_none() {
                self.primary = Some(next);
            } else if freed_secondary && self.primary != Some(next) {
                self.secondary = Some(next);
            }
            self.activate(next);
        } else if self.primary.is_none()
            && let Some(next) = next
        {
            self.primary = Some(next);
            self.focus = next;
        }
        self.refresh_layout();
    }

    /// Re-derives per-pane state that depends on the layout and the live-session count.
    fn refresh_layout(&mut self) {
        let can_split = self.secondary.is_none() && self.can_open().is_ok();
        let primary = self.primary;
        for (&pane, entry) in &mut self.panes {
            entry
                .root
                .set_fork_available(can_split && primary == Some(pane));
        }
        self.refresh_sessions_overlay();
    }

    /// The primary pane: the session reported on exit.
    pub(crate) fn main_pane(&self) -> Option<PaneId> {
        self.primary
    }

    pub(crate) fn active_pane(&self) -> PaneId {
        self.focus
    }

    pub(crate) fn pane_for_session(&self, session: &str) -> Option<PaneId> {
        self.panes.iter().find_map(|(&pane, entry)| {
            entry
                .published
                .as_ref()
                .is_some_and(|published| published.session == session)
                .then_some(pane)
        })
    }

    fn pane_mut(&mut self, pane: PaneId) -> Option<&mut RootNode> {
        self.panes.get_mut(&pane).map(|entry| &mut entry.root)
    }

    /// Records that `pane` now hosts `session` and announces it. A pane whose session was
    /// replaced in place announces the previous session as closed first; its draft revision keeps
    /// increasing.
    pub(crate) fn session_opened(
        &mut self,
        pane: PaneId,
        session: String,
        records: Vec<Arc<TranscriptRecord>>,
    ) {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return;
        };
        let previous = entry.published.take();
        let root = &entry.root;
        let settings = Settings::of(root);
        let published = Published {
            session,
            draft: Draft {
                rev: previous.as_ref().map_or(0, |previous| previous.draft.rev),
                text: root.shared_draft().to_owned(),
                images: draft_images(root),
            },
            queue: queued_prompts(root),
            settings: settings.clone(),
            busy: root.busy(),
            context: None,
            subagents: None,
        };
        if let Some(publisher) = &self.publisher {
            if let Some(previous) = previous {
                publisher.publish(Publication::Closed {
                    session: previous.session,
                });
            }
            publisher.publish(Publication::Opened {
                info: SessionInfo {
                    id: published.session.clone(),
                    workspace: root.workspace().to_owned(),
                    model: settings.model.to_string(),
                    effort: settings.effort,
                    reasoning_mode: settings.reasoning_mode,
                    speed: settings.speed,
                },
                records,
                draft: published.draft.clone(),
                queue: published.queue.clone(),
                busy: published.busy,
            });
        }
        entry.published = Some(published);
    }

    /// Publishes every change to shared state since the last call, attributing draft edits to
    /// `origin`, and refreshes the terminal's own summary of live sessions.
    pub(crate) fn publish_changes(&mut self, origin: Origin) -> RenderRequest {
        let Self {
            panes,
            publisher,
            focus,
            active,
            ..
        } = self;
        let publisher = publisher.as_ref();
        let publish = |publication| {
            if let Some(publisher) = publisher {
                publisher.publish(publication);
            }
        };
        let mut running = 0;
        for (&pane, entry) in panes.iter_mut() {
            let root = &entry.root;
            let busy = root.busy();
            let is_running = busy.turns > 0 || busy.shells > 0;
            running += usize::from(is_running);
            let Some(published) = &mut entry.published else {
                continue;
            };
            let draft = root.shared_draft();
            let images_changed = !images_match(&published.draft.images, root);
            if published.draft.text != draft || images_changed {
                published.draft.rev = published.draft.rev.saturating_add(1);
                draft.clone_into(&mut published.draft.text);
                if images_changed {
                    published.draft.images = draft_images(root);
                }
                publish(Publication::Draft {
                    session: published.session.clone(),
                    draft: published.draft.clone(),
                    origin,
                });
            }
            if !queue_matches(&published.queue, root) {
                published.queue = queued_prompts(root);
                publish(Publication::Queue {
                    session: published.session.clone(),
                    items: published.queue.clone(),
                });
            }
            let settings = Settings::of(root);
            if published.settings != settings {
                publish(Publication::Settings {
                    session: published.session.clone(),
                    model: settings.model.to_string(),
                    effort: settings.effort,
                    reasoning_mode: settings.reasoning_mode,
                    speed: settings.speed,
                });
                published.settings = settings;
            }
            let context = root.composer().context_budget();
            if published.context != Some(context) {
                published.context = Some(context);
                publish(Publication::Context {
                    session: published.session.clone(),
                    budget: context,
                });
            }
            let roster = root.subagent_roster();
            if published.subagents.as_ref() != Some(roster) {
                published.subagents = Some(roster.clone());
                publish(Publication::Subagents {
                    session: published.session.clone(),
                    roster: roster.clone(),
                });
            }
            if published.busy != busy {
                let was_running = published.busy.turns > 0 || published.busy.shells > 0;
                if was_running && !is_running && pane != *focus {
                    entry.unread = true;
                }
                published.busy = busy;
                publish(Publication::Busy {
                    session: published.session.clone(),
                    busy,
                });
            }
        }
        if let Some(session) = panes
            .get(focus)
            .and_then(|entry| entry.published.as_ref())
            .map(|published| &published.session)
            && active.as_ref() != Some(session)
        {
            *active = Some(session.clone());
            publish(Publication::Active {
                session: session.clone(),
            });
        }
        let summary = (panes.len() > 1).then_some(LiveSessions {
            live: panes.len(),
            running,
        });
        let mut render = RenderRequest::None;
        for entry in panes.values_mut() {
            render = render.max(entry.root.set_live_sessions(summary));
        }
        render
    }

    /// Applies a web command that does not open a session, with the same effects and
    /// preconditions as the equivalent keypress on the target pane.
    pub(crate) fn remote_command(
        &mut self,
        command: Command,
    ) -> Result<ComponentUpdate<AppEffect>, CommandError> {
        let (session, command) = match command {
            Command::Activate { session } => {
                let pane = self.live_pane(&session)?;
                self.activate(pane);
                return Ok(ComponentUpdate::render(RenderRequest::Immediate));
            }
            Command::Close { session, force } => {
                let pane = self.live_pane(&session)?;
                return self.close_pane(pane, force);
            }
            Command::Open(_) => {
                return Err(CommandError::Invalid(
                    "opening a session needs the event loop".to_owned(),
                ));
            }
            Command::Submit {
                session,
                rev,
                queue,
            } => {
                let pane = self.live_pane(&session)?;
                let published = self.panes[&pane]
                    .published
                    .as_ref()
                    .expect("live panes have published state");
                if published.draft.rev != rev {
                    return Err(CommandError::DraftChanged);
                }
                (session, PaneCommand::Submit { queue })
            }
            Command::SetDraft { session, text } => (session, PaneCommand::SetDraft(text)),
            Command::Interrupt { session } => (session, PaneCommand::Interrupt),
            Command::Steer { session, queue_id } => {
                (session, PaneCommand::Steer(QueueId::new(queue_id)))
            }
            Command::Dequeue { session, queue_id } => {
                (session, PaneCommand::Dequeue(QueueId::new(queue_id)))
            }
            Command::Compact { session } => (session, PaneCommand::Compact),
            Command::SetModel { session, model } => {
                let model = model::parse(&model).map_err(CommandError::Invalid)?;
                (session, PaneCommand::SetModel(model))
            }
            Command::SetEffort { session, effort } => (session, PaneCommand::SetEffort(effort)),
            Command::SetReasoningMode { session, mode } => {
                (session, PaneCommand::SetReasoningMode(mode))
            }
            Command::SetSpeed { session, speed } => (session, PaneCommand::SetSpeed(speed)),
            Command::EditQueued {
                session,
                queue_id,
                text,
            } => (
                session,
                PaneCommand::EditQueued(QueueId::new(queue_id), text),
            ),
            Command::AttachImage { session, data_url } => {
                (session, PaneCommand::AttachImage(data_url))
            }
            Command::Reflect {
                session,
                instructions,
            } => (session, PaneCommand::Reflect(instructions)),
            Command::Handoff { session } => (session, PaneCommand::Handoff),
            Command::ReloadConfig
            | Command::WriteConfig { .. }
            | Command::DeleteMemory { .. }
            | Command::SetMaxSubagents { .. } => {
                return Err(CommandError::Invalid(
                    "process-wide commands need the event loop".to_owned(),
                ));
            }
        };
        let pane = self.live_pane(&session)?;
        let root = self.pane_mut(pane).expect("live panes have a root");
        let update = root.remote_command(command)?;
        Ok(self.map_root_update(pane, update))
    }

    fn live_pane(&self, session: &str) -> Result<PaneId, CommandError> {
        self.pane_for_session(session)
            .ok_or(CommandError::UnknownSession)
    }
}

fn queued_prompts(root: &RootNode) -> Vec<QueuedPrompt> {
    root.queued_prompts()
        .map(|entry| QueuedPrompt {
            id: entry.id.get(),
            text: entry.text.to_owned(),
            steering: entry.steering,
        })
        .collect()
}

fn draft_images(root: &RootNode) -> Vec<DraftImage> {
    root.shared_draft_images()
        .map(|(marker, data_url)| DraftImage {
            marker: marker.to_owned(),
            data_url: Arc::from(data_url),
        })
        .collect()
}

/// Markers identify images: a marker is never reused for a different image within a draft, so
/// comparing markers avoids comparing image data on every change.
fn images_match(published: &[DraftImage], root: &RootNode) -> bool {
    let mut current = root.shared_draft_images();
    published.iter().all(|image| {
        current
            .next()
            .is_some_and(|(marker, _)| image.marker == marker)
    }) && current.next().is_none()
}

fn queue_matches(published: &[QueuedPrompt], root: &RootNode) -> bool {
    let mut current = root.queued_prompts();
    published.iter().all(|item| {
        current.next().is_some_and(|entry| {
            item.id == entry.id.get() && item.text == entry.text && item.steering == entry.steering
        })
    }) && current.next().is_none()
}

fn is_control_c(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::{
        super::SessionListKind, AppEffect, AppEvent, AppNode, DraftReset, RootEffect, RootEvent,
        RootNode, SPLIT_HINT,
    };
    use crate::{
        app::{
            config::{ReasoningEffort, ReasoningMode, Speed, TuiConfig},
            theme::{ColorScheme, Theme, ThemeMode},
        },
        core::{
            pane::PaneId,
            transcript::{LocalEvent, TranscriptRecord, TurnId, UserSubmitted},
        },
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nanocodex::{HarnessModel as Model, Model as CodexModel};
    use ratatui::{Terminal, backend::TestBackend};
    use semver::Version;
    use std::{num::NonZeroU16, path::PathBuf, sync::Arc};
    use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord};

    fn app() -> AppNode {
        let workspace = PathBuf::from("/workspace");
        let root = RootNode::new(&workspace, ReasoningEffort::Low);
        AppNode::new(Theme::default(), workspace, root)
    }

    fn control(character: char) -> AppEvent {
        AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL,
        )))
    }

    #[test]
    fn model_session_replacement_preserves_the_draft_in_place() {
        let mut app = app();
        app.update(AppEvent::EditorDraft {
            pane: PaneId::Main,
            draft: "send with luna".to_owned(),
        });

        app.update(AppEvent::NewSessionReady {
            pane: PaneId::Main,
            effort: ReasoningEffort::Low,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            model: Model::Codex(CodexModel::Luna),
            draft_reset: DraftReset::Preserve,
            skills: Arc::from([]),
        });

        let root = app.root(PaneId::Main).unwrap();
        assert_eq!(root.composer().draft(), "send with luna");
        assert_eq!(root.composer().model(), Model::Codex(CodexModel::Luna));
    }

    #[test]
    fn replacing_claude_with_codex_resets_context_capacity() {
        let mut app = app();
        app.pane_mut(PaneId::Main)
            .unwrap()
            .set_model(Model::Claude(nanocodex::ClaudeModel::Opus55));
        app.update(AppEvent::ContextBudget {
            pane: PaneId::Main,
            budget: crate::core::context::ContextBudget {
                active_tokens: 0,
                window_tokens: 1_000_000,
            },
        });
        assert!(rendered(&mut app, 80, 12).contains("0%/1m"));
        app.update(AppEvent::NewSessionReady {
            pane: PaneId::Main,
            effort: ReasoningEffort::Medium,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            model: Model::Codex(CodexModel::Sol),
            draft_reset: DraftReset::Preserve,
            skills: Arc::from([]),
        });
        assert!(rendered(&mut app, 80, 12).contains("0%/272k"));
    }

    #[test]
    fn model_session_replacement_preserves_the_preferred_reasoning_mode() {
        let mut app = app();
        app.set_preferred_reasoning_mode(ReasoningMode::Pro);

        app.update(AppEvent::NewSessionReady {
            pane: PaneId::Main,
            effort: ReasoningEffort::High,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            model: Model::Codex(CodexModel::Astra),
            draft_reset: DraftReset::Preserve,
            skills: Arc::from([]),
        });

        let root = app.root(PaneId::Main).unwrap();
        assert_eq!(root.composer().reasoning_mode(), ReasoningMode::Standard);
        assert_eq!(root.preferred_reasoning_mode(), ReasoningMode::Pro);
    }

    fn memory_record(id: i64, content: &str) -> MemoryRecord {
        MemoryRecord {
            key: MemoryKey::local(id, 1),
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

    fn open_memory(app: &mut AppNode, pane: PaneId) -> super::ComponentUpdate<AppEffect> {
        app.focus = pane;
        app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        ))));
        for character in "memory".chars() {
            app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            ))));
        }
        app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))))
    }

    fn rendered(app: &mut AppNode, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn auto_theme_redraws_for_system_changes_but_explicit_modes_ignore_them() {
        let mut app = app();

        let update = app.update(AppEvent::SystemThemeChanged(ColorScheme::Light));
        assert_eq!(update.render, super::RenderRequest::Immediate);
        assert_eq!(
            app.theme.code_background(),
            ratatui::style::Color::Rgb(0xEE, 0xEE, 0xEE)
        );

        app.set_theme_mode(ThemeMode::Dark);
        let update = app.update(AppEvent::SystemThemeChanged(ColorScheme::Dark));
        assert_eq!(update.render, super::RenderRequest::None);
        assert_eq!(
            app.theme.code_background(),
            ratatui::style::Color::Rgb(0x26, 0x26, 0x26)
        );
    }

    #[test]
    fn update_available_routes_to_the_primary_notification() {
        let mut app = app();

        let update = app.update(AppEvent::UpdateAvailable(Version::new(1, 2, 3)));

        assert_eq!(update.render, super::RenderRequest::Immediate);
        assert!(
            rendered(&mut app, 80, 12).contains("Update available · v1.2.3 · run `tact update`")
        );
    }

    #[test]
    fn resume_selector_starts_loading_sessions_in_the_primary_pane() {
        let mut app = app();

        let update = app.open_resume_selector();

        assert!(matches!(
            update.effects.as_slice(),
            [AppEffect::Pane {
                pane: PaneId::Main,
                effect: RootEffect::LoadSessions(SessionListKind::Resume),
            }]
        ));
        assert_eq!(update.render, super::RenderRequest::Immediate);
        assert!(rendered(&mut app, 80, 12).contains("Loading sessions…"));
    }

    #[test]
    fn fork_immediately_renders_the_primary_transcript_in_both_panes() {
        let mut app = app();
        let record = TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(1),
                text: "inherited history".to_owned(),
            }),
        )
        .unwrap();
        app.update(AppEvent::Transcript {
            pane: PaneId::Main,
            record: Arc::new(record),
        });

        let update = app.update(control('t'));

        assert!(matches!(
            update.effects.as_slice(),
            [AppEffect::OpenFork {
                pane: PaneId::Fork(1),
                parent: PaneId::Main,
            }]
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(rendered.matches("inherited history").count(), 2);
        assert!(app.root(PaneId::Fork(1)).is_some());
    }

    #[test]
    fn fork_inherits_the_primary_context_usage() {
        let mut app = app();
        app.update(AppEvent::ContextBudget {
            pane: PaneId::Main,
            budget: crate::core::context::ContextBudget {
                active_tokens: 136_000,
                window_tokens: 272_000,
            },
        });

        app.update(control('t'));

        assert_eq!(
            app.root(PaneId::Fork(1))
                .unwrap()
                .composer()
                .context_tokens(),
            136_000
        );
    }

    #[test]
    fn handoff_starts_a_fresh_session_with_an_editable_draft_and_selected_model() {
        let mut app = app();

        let update = app.update(AppEvent::HandoffReady {
            pane: PaneId::Main,
            prompt: "Continue from the validated parser design.".to_owned(),
            effort: ReasoningEffort::High,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            model: Model::Codex(CodexModel::Luna),
            skills: Arc::from([]),
        });

        assert!(update.effects.is_empty());
        let root = app.root(PaneId::Main).expect("the main pane should remain");
        assert_eq!(
            root.composer().draft(),
            "Continue from the validated parser design."
        );
        assert_eq!(root.composer().model(), Model::Codex(CodexModel::Luna));
    }

    #[test]
    fn fork_effort_changes_do_not_change_the_primary_composer() {
        let mut app = app();
        app.update(control('t'));
        app.update(AppEvent::ForkReady {
            pane: PaneId::Fork(1),
        });
        app.update(control('s'));
        app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        ))));

        let update = app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))));

        assert!(matches!(
            update.effects.as_slice(),
            [AppEffect::Pane {
                pane: PaneId::Fork(1),
                effect: super::RootEffect::SetEffort {
                    effort: ReasoningEffort::Medium,
                    reasoning_mode: ReasoningMode::Standard,
                },
            }]
        ));
        assert_eq!(
            app.root(PaneId::Main).unwrap().composer().effort(),
            ReasoningEffort::Low
        );
        assert_eq!(
            app.root(PaneId::Fork(1)).unwrap().composer().effort(),
            ReasoningEffort::Medium
        );
    }

    #[test]
    fn preferred_reasoning_mode_is_shared_without_changing_running_sessions() {
        let mut app = app();
        app.update(control('t'));
        app.update(AppEvent::ForkReady {
            pane: PaneId::Fork(1),
        });

        app.set_preferred_reasoning_mode(ReasoningMode::Pro);

        for pane in [PaneId::Main, PaneId::Fork(1)] {
            let root = app.root(pane).unwrap();
            assert_eq!(root.preferred_reasoning_mode(), ReasoningMode::Pro);
            assert_eq!(root.composer().reasoning_mode(), ReasoningMode::Standard);
        }
    }

    #[test]
    fn split_view_shows_mouse_focus_and_close_hint() {
        let mut app = app();
        assert!(!rendered(&mut app, 100, 20).contains(SPLIT_HINT.trim()));

        app.update(control('t'));
        let rendered = rendered(&mut app, 100, 20);

        assert!(rendered.contains(SPLIT_HINT.trim()));
    }

    #[test]
    fn split_hint_is_hidden_at_narrow_widths() {
        let mut app = app();
        app.update(control('t'));

        let rendered = rendered(&mut app, 40, 10);

        assert!(!rendered.contains(SPLIT_HINT.trim()));
    }

    #[test]
    fn control_c_closes_only_the_focused_pane_when_multiplexed() {
        let mut app = app();
        app.update(control('t'));

        let confirmation = app.update(control('c'));

        assert!(confirmation.effects.is_empty());
        assert!(app.root(PaneId::Fork(1)).is_some());

        let close = app.update(control('c'));

        assert!(matches!(
            close.effects.as_slice(),
            [AppEffect::ClosePane(PaneId::Fork(1))]
        ));
        assert!(app.root(PaneId::Main).is_some());
        assert!(app.root(PaneId::Fork(1)).is_none());
    }

    #[test]
    fn control_c_clears_the_focused_fork_composer_before_closing_it() {
        let mut app = app();
        app.update(control('t'));
        app.update(AppEvent::ForkReady {
            pane: PaneId::Fork(1),
        });
        app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        ))));
        app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::NONE,
        ))));

        let update = app.update(control('c'));

        assert!(update.effects.is_empty());
        assert_eq!(update.render, super::RenderRequest::Immediate);
        let fork = app.root(PaneId::Fork(1)).expect("fork should remain open");
        assert!(fork.composer().draft().is_empty());

        let confirmation = app.update(control('c'));

        assert!(confirmation.effects.is_empty());
        assert!(app.root(PaneId::Fork(1)).is_some());

        let close = app.update(control('c'));

        assert!(matches!(
            close.effects.as_slice(),
            [AppEffect::ClosePane(PaneId::Fork(1))]
        ));
        assert!(app.root(PaneId::Fork(1)).is_none());
    }

    #[test]
    fn clicking_a_pane_focuses_the_session_that_control_c_closes() {
        let mut app = app();
        app.update(control('t'));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.update(AppEvent::Terminal(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));

        let confirmation = app.update(control('c'));

        assert!(confirmation.effects.is_empty());
        assert!(app.root(PaneId::Main).is_some());

        let close = app.update(control('c'));

        assert!(matches!(
            close.effects.as_slice(),
            [AppEffect::ClosePane(PaneId::Main)]
        ));
        assert!(app.root(PaneId::Main).is_none());
        assert!(app.root(PaneId::Fork(1)).is_some());
    }

    #[test]
    fn closing_the_primary_promotes_the_fork() {
        let mut app = app();
        app.update(control('t'));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.update(AppEvent::Terminal(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));
        app.update(control('c'));

        let close = app.update(control('c'));

        assert!(matches!(
            close.effects.as_slice(),
            [AppEffect::ClosePane(PaneId::Main)]
        ));
        assert_eq!(app.main_pane(), Some(PaneId::Fork(1)));
        assert!(!rendered(&mut app, 100, 20).contains(SPLIT_HINT.trim()));

        app.update(control('c'));
        let shutdown = app.update(control('c'));

        assert!(matches!(shutdown.effects.as_slice(), [AppEffect::Shutdown]));
    }

    #[test]
    fn promoted_fork_can_open_another_fork() {
        let mut app = app();
        app.update(control('t'));
        app.update(AppEvent::ForkReady {
            pane: PaneId::Fork(1),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.update(AppEvent::Terminal(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));
        app.update(control('c'));
        app.update(control('c'));

        let fork = app.update(control('t'));

        assert!(matches!(
            fork.effects.as_slice(),
            [AppEffect::OpenFork {
                pane: PaneId::Fork(2),
                parent: PaneId::Fork(1),
            }]
        ));
        assert_eq!(app.main_pane(), Some(PaneId::Fork(1)));
        assert!(app.root(PaneId::Fork(2)).is_some());
    }

    #[test]
    fn promoted_fork_refreshes_an_open_actions_menu() {
        let mut app = app();
        app.update(control('t'));
        app.update(AppEvent::ForkReady {
            pane: PaneId::Fork(1),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        ))));
        app.update(AppEvent::Terminal(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));
        app.update(control('c'));
        app.update(control('c'));
        for character in "btw".chars() {
            app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            ))));
        }

        let fork = app.update(AppEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))));

        assert!(matches!(
            fork.effects.as_slice(),
            [AppEffect::OpenFork {
                pane: PaneId::Fork(2),
                parent: PaneId::Fork(1),
            }]
        ));
    }

    #[test]
    fn failed_pending_fork_shuts_down_after_its_parent_closes() {
        let mut app = app();
        app.update(control('t'));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.update(AppEvent::Terminal(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));
        app.update(control('c'));
        app.update(control('c'));

        let update = app.update(AppEvent::ForkFailed {
            pane: PaneId::Fork(1),
            error: "fork failed".to_owned(),
        });

        assert!(matches!(update.effects.as_slice(), [AppEffect::Shutdown]));
    }

    #[test]
    fn a_second_fork_is_unavailable_until_the_first_closes() {
        let mut app = app();
        app.update(control('t'));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.update(AppEvent::Terminal(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));

        let update = app.update(control('t'));

        assert!(update.effects.is_empty());
        assert!(app.root(PaneId::Fork(1)).is_some());
        assert!(app.root(PaneId::Fork(2)).is_none());
    }

    #[test]
    fn fork_failure_closes_the_pending_pane_and_surfaces_the_error() {
        let mut app = app();
        app.update(control('t'));

        app.update(AppEvent::ForkFailed {
            pane: PaneId::Fork(1),
            error: "no safe checkpoint".to_owned(),
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Could not fork session: no safe checkpoint"));
        assert!(app.root(PaneId::Fork(1)).is_none());
    }

    #[test]
    fn memory_operations_and_completions_keep_their_pane_identity() {
        let mut app = app();
        app.set_memory_enabled(true);
        app.update(control('t'));
        app.update(AppEvent::ForkReady {
            pane: PaneId::Fork(1),
        });

        for pane in [PaneId::Main, PaneId::Fork(1)] {
            let update = open_memory(&mut app, pane);
            assert!(matches!(
                update.effects.as_slice(),
                [AppEffect::Pane {
                    pane: effect_pane,
                    effect: super::RootEffect::LoadMemories,
                }] if *effect_pane == pane
            ));
        }

        app.update(AppEvent::MemoriesLoaded {
            pane: PaneId::Main,
            access: MemoryAccess::Local,
            records: vec![memory_record(1, "main pane memory")],
        });
        app.update(AppEvent::MemoriesLoaded {
            pane: PaneId::Fork(1),
            access: MemoryAccess::Local,
            records: vec![memory_record(2, "fork pane memory")],
        });
        let output = rendered(&mut app, 120, 24);

        assert_eq!(output.matches("main pane memory").count(), 1);
        assert_eq!(output.matches("fork pane memory").count(), 1);
    }

    #[test]
    fn config_reload_changes_mouse_scrolling_in_main_and_fork() {
        let mut app = app();
        for sequence in 1..=40 {
            let record = TranscriptRecord::from_local(
                sequence,
                sequence,
                LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(sequence),
                    text: format!("scroll line {sequence:02}"),
                }),
            )
            .unwrap();
            app.update(AppEvent::Transcript {
                pane: PaneId::Main,
                record: Arc::new(record),
            });
        }
        app.update(control('t'));
        app.update(AppEvent::ForkReady {
            pane: PaneId::Fork(1),
        });
        app.update(AppEvent::ConfigReloaded {
            pane: PaneId::Main,
            theme: Theme::default(),
            tui: TuiConfig {
                mouse_scroll_lines: NonZeroU16::new(1).unwrap(),
            },
            preferred_reasoning_mode: ReasoningMode::Standard,
            memory_enabled: false,
            message: "updated scroll speed".to_owned(),
        });
        drop(rendered(&mut app, 100, 20));
        for pane in [PaneId::Main, PaneId::Fork(1)] {
            app.update_root(
                pane,
                RootEvent::Terminal(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 1,
                    row: 1,
                    modifiers: KeyModifiers::NONE,
                })),
            );
        }
        let text = rendered(&mut app, 100, 20);
        // One row up keeps the last message visible in both panes; three rows hides it.
        assert_eq!(text.matches("scroll line 40").count(), 2, "{text}");
    }

    #[test]
    fn config_reload_updates_memory_action_availability_for_every_root() {
        let mut app = app();
        app.update(control('t'));
        app.update(AppEvent::ForkReady {
            pane: PaneId::Fork(1),
        });

        app.update(AppEvent::ConfigReloaded {
            pane: PaneId::Main,
            theme: Theme::default(),
            tui: TuiConfig::default(),
            preferred_reasoning_mode: ReasoningMode::Standard,
            memory_enabled: true,
            message: "enabled memory".to_owned(),
        });
        for pane in [PaneId::Main, PaneId::Fork(1)] {
            assert!(matches!(
                open_memory(&mut app, pane).effects.as_slice(),
                [AppEffect::Pane {
                    pane: effect_pane,
                    effect: super::RootEffect::LoadMemories,
                }] if *effect_pane == pane
            ));
        }

        app.update(AppEvent::ConfigReloaded {
            pane: PaneId::Main,
            theme: Theme::default(),
            tui: TuiConfig::default(),
            preferred_reasoning_mode: ReasoningMode::Standard,
            memory_enabled: false,
            message: "disabled memory".to_owned(),
        });
        for pane in [PaneId::Main, PaneId::Fork(1)] {
            assert!(open_memory(&mut app, pane).effects.is_empty());
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::{AppEffect, AppEvent, AppNode, DraftReset, RootEffect, RootNode};
    use crate::{
        app::{
            config::{ReasoningEffort, ReasoningMode, Speed},
            theme::Theme,
        },
        core::{
            pane::PaneId,
            prompt::Submission,
            protocol::{Command, CommandError, Draft, OpenSpec, Origin, Publication, Reply},
        },
        web::bridge::{self, WebEnd},
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use nanocodex::{HarnessModel as Model, Model as CodexModel};
    use std::{path::PathBuf, sync::Arc};

    struct Harness {
        app: AppNode,
        web: WebEnd,
    }

    impl Harness {
        /// One live session "main" whose opening publications are already drained.
        fn new() -> Self {
            let workspace = PathBuf::from("/workspace");
            let root = RootNode::new(&workspace, ReasoningEffort::Low);
            let mut app = AppNode::new(Theme::default(), workspace, root);
            let (loop_end, web) = bridge::bridge();
            app.attach_publisher(loop_end.publisher);
            app.session_opened(PaneId::Main, "main".to_owned(), Vec::new());
            app.publish_changes(Origin::Terminal);
            let mut harness = Self { app, web };
            harness.publications();
            harness
        }

        fn publications(&mut self) -> Vec<Publication> {
            let mut publications = Vec::new();
            while let Ok(publication) = self.web.publications.try_recv() {
                publications.push(publication);
            }
            publications
        }

        fn key(&mut self, code: KeyCode) -> Vec<AppEffect> {
            self.app
                .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                    code,
                    KeyModifiers::NONE,
                ))))
                .effects
        }

        fn type_text(&mut self, text: &str) {
            for character in text.chars() {
                self.key(KeyCode::Char(character));
            }
        }

        /// Opens a live background-capable session named `session` as the active pane.
        fn open(&mut self, session: &str) -> PaneId {
            let pane = self.app.begin_open("Starting new session…").unwrap();
            self.app.update(AppEvent::NewSessionReady {
                pane,
                effort: ReasoningEffort::Low,
                reasoning_mode: ReasoningMode::Standard,
                speed: Speed::Standard,
                model: Model::Codex(CodexModel::Luna),
                draft_reset: DraftReset::Clear,
                skills: Arc::from([]),
            });
            self.app
                .session_opened(pane, session.to_owned(), Vec::new());
            self.app.publish_changes(Origin::Terminal);
            self.publications();
            pane
        }

        fn command(&mut self, command: Command) -> Result<Vec<AppEffect>, CommandError> {
            let effects = self
                .app
                .remote_command(command)
                .map(|update| update.effects);
            self.app.publish_changes(Origin::Web(7));
            effects
        }

        fn draft(&self, pane: PaneId) -> &str {
            self.app.root(pane).unwrap().composer().draft()
        }
    }

    fn submitted(effects: &[AppEffect]) -> Option<&str> {
        effects.iter().find_map(|effect| match effect {
            AppEffect::Pane {
                effect: RootEffect::Submit(prompt),
                ..
            } => Some(prompt.display_text()),
            _ => None,
        })
    }

    fn drafts(publications: &[Publication]) -> Vec<(String, u64, String, Origin)> {
        publications
            .iter()
            .filter_map(|publication| match publication {
                Publication::Draft {
                    session,
                    draft: Draft { rev, text, .. },
                    origin,
                } => Some((session.clone(), *rev, text.clone(), *origin)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn terminal_typing_and_web_drafts_are_mirrored_with_their_origin() {
        let mut harness = Harness::new();
        harness.type_text("hi");
        harness.app.publish_changes(Origin::Terminal);
        // Keystrokes between two loop iterations coalesce into one revision.
        assert_eq!(
            drafts(&harness.publications()),
            [("main".to_owned(), 1, "hi".to_owned(), Origin::Terminal)]
        );

        harness
            .command(Command::SetDraft {
                session: "main".to_owned(),
                text: "from the web".to_owned(),
            })
            .unwrap();
        assert_eq!(harness.draft(PaneId::Main), "from the web");
        assert_eq!(
            drafts(&harness.publications()),
            [(
                "main".to_owned(),
                2,
                "from the web".to_owned(),
                Origin::Web(7)
            )]
        );
    }

    #[test]
    fn stale_submissions_are_refused_without_sending() {
        let mut harness = Harness::new();
        harness
            .command(Command::SetDraft {
                session: "main".to_owned(),
                text: "first".to_owned(),
            })
            .unwrap();
        harness.type_text("!");
        harness.app.publish_changes(Origin::Terminal);
        harness.publications();

        assert_eq!(
            harness.command(Command::Submit {
                session: "main".to_owned(),
                rev: 1,
                queue: false,
            }),
            Err(CommandError::DraftChanged)
        );
        assert_eq!(harness.draft(PaneId::Main), "first!");

        let effects = harness
            .command(Command::Submit {
                session: "main".to_owned(),
                rev: 2,
                queue: false,
            })
            .unwrap();
        assert_eq!(submitted(&effects), Some("first!"));
        assert_eq!(harness.draft(PaneId::Main), "");
    }

    #[test]
    fn terminal_local_commands_are_refused_and_keep_the_draft() {
        let mut harness = Harness::new();
        harness
            .command(Command::SetDraft {
                session: "main".to_owned(),
                text: "/copy 2".to_owned(),
            })
            .unwrap();
        let rev = drafts(&harness.publications()).last().unwrap().1;
        assert_eq!(
            harness.command(Command::Submit {
                session: "main".to_owned(),
                rev,
                queue: false,
            }),
            Err(CommandError::NotAvailableRemotely)
        );
        assert_eq!(harness.draft(PaneId::Main), "/copy 2");
    }

    /// Each web command ends in the same effect as its keyboard equivalent from the same state.
    #[test]
    fn web_commands_match_their_keyboard_equivalents() {
        let session = || "main".to_owned();
        let running = |harness: &mut Harness| {
            harness.type_text("work");
            harness.key(KeyCode::Enter);
            harness.publications();
        };
        let effects = |effects: Vec<AppEffect>| -> Vec<RootEffect> {
            effects
                .into_iter()
                .filter_map(|effect| match effect {
                    AppEffect::Pane { effect, .. } => Some(effect),
                    _ => None,
                })
                .collect()
        };

        // Interrupt: Esc twice.
        let mut keyboard = Harness::new();
        running(&mut keyboard);
        keyboard.key(KeyCode::Esc);
        let expected = effects(keyboard.key(KeyCode::Esc));
        let mut web = Harness::new();
        assert_eq!(
            web.command(Command::Interrupt { session: session() }),
            Err(CommandError::NothingRunning)
        );
        running(&mut web);
        let actual = effects(
            web.command(Command::Interrupt { session: session() })
                .unwrap(),
        );
        assert_eq!(actual, expected);
        assert_eq!(actual, [RootEffect::CancelTurns]);

        // Submitting while a turn runs steers it, or queues the prompt when asked to.
        for queue in [false, true] {
            let mut web = Harness::new();
            running(&mut web);
            web.command(Command::SetDraft {
                session: session(),
                text: "next".to_owned(),
            })
            .unwrap();
            let rev = drafts(&web.publications()).last().unwrap().1;
            let effects = web
                .command(Command::Submit {
                    session: session(),
                    rev,
                    queue,
                })
                .unwrap();
            assert!(submitted(&effects).is_none());
            assert_eq!(
                effects.iter().any(|effect| matches!(
                    effect,
                    AppEffect::Pane {
                        effect: RootEffect::Steer { .. },
                        ..
                    }
                )),
                !queue
            );
            assert!(web.publications().iter().any(|publication| matches!(
                publication,
                Publication::Queue { items, .. } if items.len() == 1 && items[0].text == "next"
            )));
        }
        let mut web = Harness::new();
        running(&mut web);

        // Compact is refused while a turn runs, like the disabled action.
        assert_eq!(
            web.command(Command::Compact { session: session() }),
            Err(CommandError::TurnRunning)
        );
        let mut idle = Harness::new();
        assert_eq!(
            effects(
                idle.command(Command::Compact { session: session() })
                    .unwrap()
            ),
            [RootEffect::Compact]
        );

        // Effort: the picker and the command emit the same effect.
        let mut keyboard = Harness::new();
        keyboard
            .app
            .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
            ))));
        keyboard.key(KeyCode::Right);
        let expected = effects(keyboard.key(KeyCode::Enter));
        let mut web = Harness::new();
        let actual = effects(
            web.command(Command::SetEffort {
                session: session(),
                effort: ReasoningEffort::Medium,
            })
            .unwrap(),
        );
        assert_eq!(actual, expected);

        // Speed: the command emits the picker's effect.
        let mut web = Harness::new();
        assert_eq!(
            effects(
                web.command(Command::SetSpeed {
                    session: session(),
                    speed: Speed::Fast,
                })
                .unwrap()
            ),
            [RootEffect::SetSpeed(Speed::Fast)]
        );

        // Unknown sessions are refused.
        assert_eq!(
            web.command(Command::Interrupt {
                session: "missing".to_owned(),
            }),
            Err(CommandError::UnknownSession)
        );
    }

    #[test]
    fn background_sessions_keep_their_draft_and_finish_turns() {
        let mut harness = Harness::new();
        harness.type_text("main draft");
        let opened = harness.open("second");
        assert_eq!(harness.app.active_pane(), opened);
        assert_eq!(harness.app.main_pane(), Some(opened));
        assert_eq!(harness.draft(PaneId::Main), "main draft");

        harness.type_text("work");
        let effects = harness.key(KeyCode::Enter);
        assert_eq!(submitted(&effects), Some("work"));
        harness
            .command(Command::Activate {
                session: "main".to_owned(),
            })
            .unwrap();
        assert!(harness.publications().iter().any(|publication| matches!(
            publication,
            Publication::Active { session } if session == "main"
        )));

        // The background session finishes its turn and is marked unread.
        harness.app.update(AppEvent::WorkerTurnFinished {
            pane: opened,
            terminal_expected: false,
        });
        harness.app.publish_changes(Origin::Terminal);
        let sessions = harness.app.live_sessions();
        let second = sessions
            .iter()
            .find(|session| session.pane == opened)
            .unwrap();
        assert!(second.unread);
        assert!(!second.running);
        assert!(!second.displayed);
    }

    #[test]
    fn closing_the_active_pane_activates_the_most_recent_other_pane() {
        let mut harness = Harness::new();
        let second = harness.open("second");
        let third = harness.open("third");
        harness
            .command(Command::Activate {
                session: "second".to_owned(),
            })
            .unwrap();
        harness
            .command(Command::Activate {
                session: "third".to_owned(),
            })
            .unwrap();
        harness.publications();

        let effects = harness
            .command(Command::Close {
                session: "third".to_owned(),
                force: false,
            })
            .unwrap();
        assert!(matches!(effects.as_slice(), [AppEffect::ClosePane(pane)] if *pane == third));
        assert_eq!(harness.app.active_pane(), second);
        assert_eq!(harness.app.main_pane(), Some(second));
        let publications = harness.publications();
        assert!(publications.iter().any(|publication| matches!(
            publication,
            Publication::Closed { session } if session == "third"
        )));
        assert!(publications.iter().any(|publication| matches!(
            publication,
            Publication::Active { session } if session == "second"
        )));
    }

    #[test]
    fn running_sessions_close_only_when_forced_and_the_last_one_stays() {
        let mut harness = Harness::new();
        harness.open("second");
        harness.type_text("work");
        harness.key(KeyCode::Enter);
        let close = |force| Command::Close {
            session: "second".to_owned(),
            force,
        };
        assert_eq!(
            harness.command(close(false)),
            Err(CommandError::TurnRunning)
        );
        assert!(harness.command(close(true)).is_ok());
        assert!(matches!(
            harness.command(Command::Close {
                session: "main".to_owned(),
                force: true,
            }),
            Err(CommandError::Invalid(_))
        ));
    }

    #[test]
    fn every_open_path_obeys_the_live_session_limit() {
        let mut harness = Harness::new();
        harness.app.set_max_live_sessions(1);
        assert_eq!(
            harness.app.begin_open("Starting new session…"),
            Err(CommandError::TooManySessions)
        );
        assert_eq!(
            harness.app.begin_fork(PaneId::Main),
            Err(CommandError::TooManySessions)
        );
        let fork = harness
            .app
            .update(AppEvent::Terminal(Event::Key(KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL,
            ))));
        assert!(fork.effects.is_empty());
    }

    #[test]
    fn fork_inherits_the_parents_workspace_and_new_sessions_use_the_default() {
        let mut app = Harness::new().app;
        app.set_pane_workspace(PaneId::Main, PathBuf::from("/other/checkout"));
        let fork = app.begin_fork(PaneId::Main).unwrap();
        assert_eq!(
            app.root(fork).unwrap().workspace(),
            PathBuf::from("/other/checkout")
        );
        let fresh = app.begin_open("Starting").unwrap();
        assert_eq!(
            app.root(fresh).unwrap().workspace(),
            PathBuf::from("/workspace")
        );
    }

    #[test]
    fn web_forks_take_the_focused_slot_and_keep_the_parent_in_the_background() {
        let mut harness = Harness::new();
        let fork = harness.app.begin_fork(PaneId::Main).unwrap();
        assert_eq!(harness.app.active_pane(), fork);
        assert_eq!(harness.app.main_pane(), Some(fork));
        assert!(harness.app.root(PaneId::Main).is_some());
    }

    #[test]
    fn sessions_action_lists_and_activates_live_sessions() {
        let mut harness = Harness::new();
        let second = harness.open("second");
        harness.type_text("/");
        harness.type_text("switch");
        harness.key(KeyCode::Enter);
        let render = |app: &mut AppNode| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let overlay = render(&mut harness.app);
        assert!(overlay.contains("Sessions"), "{overlay}");
        assert!(overlay.contains("focused"), "{overlay}");

        assert_eq!(harness.app.active_pane(), second);
        harness.key(KeyCode::Down);
        harness.key(KeyCode::Enter);
        assert_eq!(harness.app.active_pane(), PaneId::Main);
        assert!(render(&mut harness.app).contains("2 sessions, 0 running"));
    }

    #[test]
    fn opened_sessions_announce_their_state() {
        let mut harness = Harness::new();
        let pane = harness.app.begin_open("Starting new session…").unwrap();
        harness.app.publish_changes(Origin::Terminal);
        assert!(
            harness.publications().is_empty(),
            "a session is announced only once it is live"
        );
        harness.app.update(AppEvent::NewSessionReady {
            pane,
            effort: ReasoningEffort::High,
            reasoning_mode: ReasoningMode::Standard,
            speed: Speed::Standard,
            model: Model::Codex(CodexModel::Sol),
            draft_reset: DraftReset::Clear,
            skills: Arc::from([]),
        });
        harness
            .app
            .session_opened(pane, "new".to_owned(), Vec::new());
        harness.app.publish_changes(Origin::Terminal);
        let publications = harness.publications();
        assert!(matches!(
            publications.as_slice(),
            [
                Publication::Opened { info, .. },
                Publication::Context { .. },
                Publication::Subagents { .. },
                Publication::Active { session },
            ] if info.id == "new" && info.effort == ReasoningEffort::High && session == "new"
        ));
        assert_eq!(
            harness.command(Command::Open(OpenSpec::New {
                model: None,
                workspace: None
            })),
            Err(CommandError::Invalid(
                "opening a session needs the event loop".to_owned()
            ))
        );
        let _ = (Reply::Done, Submission::text(String::new()));
    }
}

#[cfg(test)]
mod parity_tests {
    use super::{AppEffect, AppEvent, AppNode, RootEffect, RootNode};
    use crate::{
        app::{
            config::{ReasoningEffort, ReasoningMode},
            theme::Theme,
        },
        core::{
            context::ContextBudget,
            pane::PaneId,
            protocol::{Command, CommandError, DraftImage, Origin, Publication},
        },
        web::bridge::{self, WebEnd},
    };
    use nanocodex::{
        ClaudeModel, HarnessModel as Model, Model as CodexModel,
        ReasoningMode as NanocodexReasoningMode, Thinking,
        agent::{
            events::{AgentEvent, AgentEventKind},
            input::{PromptInput, UserInput},
        },
    };
    use serde_json::{json, value::to_raw_value};
    use std::{path::PathBuf, sync::Arc};
    use tact_subagents::{AgentDescriptor, AgentId, AgentUpdate};

    const IMAGE: &str = "data:image/png;base64,iVBORw0KGgo=";

    struct Harness {
        app: AppNode,
        web: WebEnd,
    }

    impl Harness {
        fn new() -> Self {
            let workspace = PathBuf::from("/workspace");
            let root = RootNode::new(&workspace, ReasoningEffort::Low);
            let mut app = AppNode::new(Theme::default(), workspace, root);
            let (loop_end, web) = bridge::bridge();
            app.attach_publisher(loop_end.publisher);
            app.session_opened(PaneId::Main, "main".to_owned(), Vec::new());
            app.publish_changes(Origin::Terminal);
            let mut harness = Self { app, web };
            harness.publications();
            harness
        }

        fn publications(&mut self) -> Vec<Publication> {
            let mut publications = Vec::new();
            while let Ok(publication) = self.web.publications.try_recv() {
                publications.push(publication);
            }
            publications
        }

        fn command(&mut self, command: Command) -> Result<Vec<RootEffect>, CommandError> {
            let effects = self.app.remote_command(command).map(|update| {
                update
                    .effects
                    .into_iter()
                    .filter_map(|effect| match effect {
                        AppEffect::Pane { effect, .. } => Some(effect),
                        _ => None,
                    })
                    .collect()
            });
            self.app.publish_changes(Origin::Web(7));
            effects
        }

        fn set_draft(&mut self, text: &str) {
            self.command(Command::SetDraft {
                session: "main".to_owned(),
                text: text.to_owned(),
            })
            .unwrap();
        }
    }

    fn draft_images(publications: &[Publication]) -> Vec<Vec<DraftImage>> {
        publications
            .iter()
            .filter_map(|publication| match publication {
                Publication::Draft { draft, .. } => Some(draft.images.clone()),
                _ => None,
            })
            .collect()
    }

    fn queue_ids(publications: &[Publication]) -> Vec<(u64, String)> {
        publications
            .iter()
            .rev()
            .find_map(|publication| match publication {
                Publication::Queue { items, .. } => Some(
                    items
                        .iter()
                        .map(|item| (item.id, item.text.clone()))
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn attached_images_are_shared_draft_state() {
        let mut harness = Harness::new();
        assert!(matches!(
            harness.command(Command::AttachImage {
                session: "main".to_owned(),
                data_url: "https://example.com/cat.png".to_owned(),
            }),
            Err(CommandError::Invalid(_))
        ));

        harness.set_draft("look ");
        harness.publications();
        harness
            .command(Command::AttachImage {
                session: "main".to_owned(),
                data_url: IMAGE.to_owned(),
            })
            .unwrap();
        let marker = DraftImage {
            marker: "[Image #1]".to_owned(),
            data_url: Arc::from(IMAGE),
        };
        assert_eq!(
            harness.app.root(PaneId::Main).unwrap().shared_draft(),
            "look [Image #1]"
        );
        assert_eq!(
            draft_images(&harness.publications()),
            [vec![marker.clone()]]
        );

        harness.set_draft("please look at [Image #1] closely");
        assert_eq!(draft_images(&harness.publications()), [vec![marker]]);

        harness.set_draft("never mind");
        assert_eq!(draft_images(&harness.publications()), [Vec::new()]);
    }

    #[test]
    fn submitting_a_draft_with_an_attached_image_sends_the_image() {
        let mut harness = Harness::new();
        harness.set_draft("look ");
        harness
            .command(Command::AttachImage {
                session: "main".to_owned(),
                data_url: IMAGE.to_owned(),
            })
            .unwrap();
        harness.set_draft("look at [Image #1] closely");
        let effects = harness
            .command(Command::Submit {
                session: "main".to_owned(),
                rev: 3,
                queue: false,
            })
            .unwrap();

        let prompt = effects
            .iter()
            .find_map(|effect| match effect {
                RootEffect::Submit(prompt) => Some(prompt.agent_prompt()),
                _ => None,
            })
            .expect("the draft is submitted");
        let PromptInput::Content(content) = prompt.instruction else {
            panic!("a draft with an image is sent as content");
        };
        assert!(content.iter().any(|part| matches!(
            part,
            UserInput::Image { image_url, .. } if image_url == IMAGE
        )));
    }

    #[test]
    fn reflection_handoff_and_queue_edits_obey_terminal_preconditions() {
        let mut harness = Harness::new();
        let effects = harness
            .command(Command::Reflect {
                session: "main".to_owned(),
                instructions: "focus on tests".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [RootEffect::Reflect(prompt)] if prompt.display_text() == "focus on tests"
        ));
        for command in [
            Command::Reflect {
                session: "main".to_owned(),
                instructions: String::new(),
            },
            Command::Handoff {
                session: "main".to_owned(),
            },
        ] {
            assert_eq!(harness.command(command), Err(CommandError::TurnRunning));
        }

        harness.set_draft("queued while running");
        harness
            .command(Command::Submit {
                session: "main".to_owned(),
                rev: 1,
                queue: true,
            })
            .unwrap();
        let [(id, text)] = queue_ids(&harness.publications())
            .try_into()
            .expect("one queued prompt");
        assert_eq!(text, "queued while running");
        harness
            .command(Command::EditQueued {
                session: "main".to_owned(),
                queue_id: id,
                text: "edited remotely".to_owned(),
            })
            .unwrap();
        assert_eq!(
            queue_ids(&harness.publications()),
            [(id, "edited remotely".to_owned())]
        );
        assert_eq!(
            harness.command(Command::EditQueued {
                session: "main".to_owned(),
                queue_id: id + 1,
                text: "missing".to_owned(),
            }),
            Err(CommandError::UnknownQueueItem)
        );

        let mut idle = Harness::new();
        let effects = idle
            .command(Command::Handoff {
                session: "main".to_owned(),
            })
            .unwrap();
        assert_eq!(effects, [RootEffect::Handoff]);
        assert_eq!(
            idle.command(Command::Compact {
                session: "main".to_owned(),
            }),
            Err(CommandError::TurnRunning)
        );
        assert_eq!(
            idle.command(Command::Interrupt {
                session: "main".to_owned(),
            }),
            Ok(vec![RootEffect::CancelHandoff])
        );
    }

    #[test]
    fn reasoning_modes_follow_the_model_catalog() {
        let mut harness = Harness::new();
        let effects = harness
            .command(Command::SetReasoningMode {
                session: "main".to_owned(),
                mode: ReasoningMode::Pro,
            })
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [RootEffect::SetEffort { .. }, RootEffect::SetModel(_)]
        ));

        harness
            .app
            .pane_mut(PaneId::Main)
            .unwrap()
            .set_model(Model::Claude(ClaudeModel::Sonnet55));
        assert!(matches!(
            harness.command(Command::SetReasoningMode {
                session: "main".to_owned(),
                mode: ReasoningMode::Pro,
            }),
            Err(CommandError::Invalid(_))
        ));
    }

    #[test]
    fn process_wide_commands_are_left_to_the_event_loop() {
        let mut harness = Harness::new();
        for command in [Command::ReloadConfig, Command::SetMaxSubagents { limit: 3 }] {
            assert!(matches!(
                harness.command(command),
                Err(CommandError::Invalid(_))
            ));
        }
    }

    #[test]
    fn context_budget_and_subagents_are_published() {
        let mut harness = Harness::new();
        let budget = ContextBudget {
            active_tokens: 1_200,
            window_tokens: 200_000,
        };
        harness.app.update(AppEvent::ContextBudget {
            pane: PaneId::Main,
            budget,
        });
        harness.app.update(AppEvent::Subagent {
            pane: PaneId::Main,
            update: AgentUpdate::Added(AgentDescriptor {
                id: AgentId::new(1),
                session_id: "child".to_owned(),
                model: Model::Codex(CodexModel::Sol),
                thinking: Thinking::Medium,
                reasoning_mode: NanocodexReasoningMode::Standard,
                role: "worker".to_owned(),
                task: "trace".to_owned(),
                parent: None,
            }),
        });
        harness.app.publish_changes(Origin::Terminal);
        let publications = harness.publications();
        assert!(publications.iter().any(|publication| matches!(
            publication,
            Publication::Context { session, budget: published }
                if session == "main" && *published == budget
        )));
        assert!(publications.iter().any(|publication| matches!(
            publication,
            Publication::Subagents { session, roster }
                if session == "main" && roster.agents.len() == 1
        )));

        harness.app.update(AppEvent::Subagent {
            pane: PaneId::Main,
            update: AgentUpdate::Event {
                id: AgentId::new(1),
                event: AgentEvent {
                    protocol_version: 1,
                    request_id: Arc::from("child"),
                    seq: 1,
                    kind: AgentEventKind::AssistantMessage,
                    payload: to_raw_value(&json!({ "text": "done" })).unwrap().into(),
                },
            },
        });
        assert!(harness.publications().iter().any(|publication| matches!(
            publication,
            Publication::SubagentRecord { session, agent, .. }
                if session == "main" && *agent == AgentId::new(1)
        )));
    }
}
