//! Application-level ownership for the primary and optional forked panes.

use super::{
    node::{ComponentUpdate, Node, RenderRequest},
    queue::QueueId,
    root::{DraftReset, RestoredSessionProjection, RootEffect, RootEvent, RootNode},
};
use crate::{
    app::config::{ReasoningEffort, ReasoningMode},
    core::extensions::Skill,
    tui::{
        pane::PaneId,
        session::{RecentPrompt, SessionSummary},
        theme::{ColorScheme, Theme, ThemeMode},
        transcript::TranscriptRecord,
    },
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use nanocodex::Model;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders},
};
use semver::Version;
use std::{path::PathBuf, sync::Arc, time::Instant};
use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord, MemorySource};
use tact_subagents::AgentUpdate;
use unicode_width::UnicodeWidthStr;

const SPLIT_HINT: &str = " mouse: focus · Ctrl+C: clear · Ctrl+C×2: close ";
const MIN_SPLIT_HINT_WIDTH: u16 = 60;

pub(crate) enum AppEvent {
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
    ReviewFinished {
        pane: PaneId,
        markdown: String,
    },
    ReviewStarted(PaneId),
    ReviewReady {
        pane: PaneId,
        url: String,
    },
    ReviewCancelled(PaneId),
    ReviewFailed {
        pane: PaneId,
        error: String,
    },
    HandoffReady {
        pane: PaneId,
        prompt: String,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        fast_mode: bool,
        skills: Arc<[Skill]>,
    },
    HandoffCancelled(PaneId),
    HandoffFailed {
        pane: PaneId,
        error: String,
    },
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
    NewSessionReady {
        pane: PaneId,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
        fast_mode: bool,
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
        fast_mode: bool,
        model: Model,
        skills: Arc<[Skill]>,
    },
    NotifyError {
        pane: PaneId,
        error: String,
    },
    NotifySuccess {
        pane: PaneId,
        message: String,
    },
    ConfirmReviewDownload {
        pane: PaneId,
    },
    UpdateAvailable(Version),
    ConfigReloaded {
        pane: PaneId,
        theme: Theme,
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

pub(crate) enum AppEffect {
    Pane { pane: PaneId, effect: RootEffect },
    OpenFork { pane: PaneId, parent: PaneId },
    ClosePane(PaneId),
    SetTheme(ThemeMode),
    Shutdown,
}

pub(crate) struct AppNode {
    theme: Theme,
    workspace: PathBuf,
    main: Option<(PaneId, Node<RootNode>)>,
    fork: Option<(PaneId, Node<RootNode>)>,
    focus: PaneId,
    main_area: Rect,
    fork_area: Rect,
    next_fork: u64,
}

impl AppNode {
    pub(crate) fn new(theme: Theme, workspace: PathBuf, mut root: RootNode) -> Self {
        root.set_theme_mode(theme.mode());
        Self {
            theme,
            workspace,
            main: Some((PaneId::Main, Node::new(root))),
            fork: None,
            focus: PaneId::Main,
            main_area: Rect::default(),
            fork_area: Rect::default(),
            next_fork: 1,
        }
    }

    pub(crate) fn open_resume_selector(&mut self) -> ComponentUpdate<AppEffect> {
        let Some((pane, root)) = self.main.as_mut() else {
            return ComponentUpdate::none();
        };
        let pane = *pane;
        let update = root.component_mut().load_sessions();
        self.map_root_update(pane, update)
    }

    pub(crate) fn update(&mut self, event: AppEvent) -> ComponentUpdate<AppEffect> {
        match event {
            AppEvent::Terminal(event) => self.update_terminal(event),
            AppEvent::PasteImage(data_url) => {
                self.update_root(self.focus, RootEvent::PasteImage(data_url))
            }
            AppEvent::Transcript { pane, record } => {
                self.update_root(pane, RootEvent::Transcript(record))
            }
            AppEvent::AgentStreamClosed(pane) => {
                self.update_root(pane, RootEvent::AgentStreamClosed)
            }
            AppEvent::Subagent { pane, update } => {
                self.update_root(pane, RootEvent::Subagent(update))
            }
            AppEvent::EditorDraft { pane, draft } => {
                self.update_root(pane, RootEvent::ReplaceDraft(draft))
            }
            AppEvent::ReviewFinished { pane, markdown } => {
                self.update_root(pane, RootEvent::ReviewFinished(markdown))
            }
            AppEvent::ReviewStarted(pane) => self.update_root(pane, RootEvent::ReviewStarted),
            AppEvent::ReviewReady { pane, url } => {
                self.update_root(pane, RootEvent::ReviewReady(url))
            }
            AppEvent::ReviewCancelled(pane) => self.update_root(pane, RootEvent::ReviewCancelled),
            AppEvent::ReviewFailed { pane, error } => {
                self.update_root(pane, RootEvent::ReviewFailed(error))
            }
            AppEvent::HandoffReady {
                pane,
                prompt,
                effort,
                reasoning_mode,
                fast_mode,
                skills,
            } => {
                let workspace = self.workspace.clone();
                {
                    let Some(root) = self.pane_mut(pane) else {
                        return ComponentUpdate::none();
                    };
                    root.component_mut().reset_session(
                        &workspace,
                        effort,
                        reasoning_mode,
                        reasoning_mode,
                        DraftReset::Clear,
                    );
                    root.component_mut().set_fast_mode(fast_mode);
                    root.component_mut().set_skills(skills);
                }
                self.update_root(pane, RootEvent::HandoffFinished(prompt))
            }
            AppEvent::HandoffCancelled(pane) => self.update_root(pane, RootEvent::HandoffCancelled),
            AppEvent::HandoffFailed { pane, error } => {
                self.update_root(pane, RootEvent::HandoffFailed(error))
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
                self.remove_pane(pane);
                let Some(target) = self.main_pane() else {
                    return ComponentUpdate {
                        effects: vec![AppEffect::Shutdown],
                        render: RenderRequest::Immediate,
                    };
                };
                self.update_root(
                    target,
                    RootEvent::NotifyError(format!("Could not fork session: {error}")),
                )
            }
            AppEvent::NewSessionReady {
                pane,
                effort,
                reasoning_mode,
                fast_mode,
                model,
                draft_reset,
                skills,
            } => {
                let workspace = self.workspace.clone();
                let Some(root) = self.pane_mut(pane) else {
                    return ComponentUpdate::none();
                };
                root.component_mut().reset_session(
                    &workspace,
                    effort,
                    reasoning_mode,
                    reasoning_mode,
                    draft_reset,
                );
                root.component_mut().set_fast_mode(fast_mode);
                root.component_mut().set_model(model);
                root.component_mut().set_skills(skills);
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
                fast_mode,
                model,
                skills,
            } => self.update_root(
                pane,
                RootEvent::SessionRestored {
                    projection,
                    effort,
                    reasoning_mode,
                    preferred_reasoning_mode,
                    fast_mode,
                    model,
                    skills,
                },
            ),
            AppEvent::NotifyError { pane, error } => {
                self.update_root(pane, RootEvent::NotifyError(error))
            }
            AppEvent::NotifySuccess { pane, message } => {
                self.update_root(pane, RootEvent::NotifySuccess(message))
            }
            AppEvent::ConfirmReviewDownload { pane } => {
                self.update_root(pane, RootEvent::ConfirmReviewDownload)
            }
            AppEvent::UpdateAvailable(version) => {
                let pane = self.main_pane().unwrap_or(self.focus);
                self.update_root(pane, RootEvent::UpdateAvailable(version))
            }
            AppEvent::ConfigReloaded {
                pane,
                theme,
                preferred_reasoning_mode,
                memory_enabled,
                message,
            } => {
                self.theme.replace_from_config(theme);
                let mode = self.theme.mode();
                if let Some((_, main)) = &mut self.main {
                    main.component_mut().set_theme_mode(mode);
                }
                if let Some((_, fork)) = &mut self.fork {
                    fork.component_mut().set_theme_mode(mode);
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
            AppEvent::AnimationFrame(now) => self.update_all(RootEvent::AnimationFrame(now)),
        }
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.is_empty() {
            self.main_area = Rect::default();
            self.fork_area = Rect::default();
            return;
        }
        let Some((fork_pane, fork)) = &mut self.fork else {
            self.main_area = area;
            self.fork_area = Rect::default();
            if let Some((main_pane, main)) = &mut self.main {
                main.component_mut().render_focused(
                    frame,
                    area,
                    &self.theme,
                    self.focus == *main_pane,
                );
            }
            return;
        };

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
        let main_content = Rect {
            y: self.main_area.y.saturating_add(hint_height),
            height: self.main_area.height.saturating_sub(hint_height),
            ..self.main_area
        };
        let fork_content = Rect {
            y: self.fork_area.y.saturating_add(hint_height),
            height: self.fork_area.height.saturating_sub(hint_height),
            ..self.fork_area
        };
        if let Some((main_pane, main)) = &mut self.main {
            main.component_mut().render_focused(
                frame,
                main_content,
                &self.theme,
                self.focus == *main_pane,
            );
        }
        fork.component_mut().render_focused(
            frame,
            fork_content,
            &self.theme,
            self.focus == *fork_pane,
        );
        frame.render_widget(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(self.theme.border())),
            Rect::new(divider_x, area.y, 1, area.height),
        );
        self.render_split_hint(frame, area, divider_x);
    }

    pub(crate) fn root(&self, pane: PaneId) -> Option<&RootNode> {
        self.pane(pane).map(Node::component)
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        [
            self.main
                .as_ref()
                .and_then(|(_, root)| root.component().animation_deadline()),
            self.fork
                .as_ref()
                .and_then(|(_, root)| root.component().animation_deadline()),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn update_terminal(&mut self, event: Event) -> ComponentUpdate<AppEffect> {
        if matches!(event, Event::FocusGained) {
            self.refresh_terminal_images();
        }
        if matches!(event, Event::Resize(_, _)) {
            let mut update = self.update_all(RootEvent::Terminal(event));
            update.render = RenderRequest::Immediate;
            return update;
        }
        if self.fork.is_some() && is_control_c(&event) {
            let pane = self.focus;
            let update = self.update_root(pane, RootEvent::Terminal(event));
            if !matches!(update.effects.as_slice(), [AppEffect::Shutdown]) {
                return update;
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
            if self.fork_area.contains(position) {
                self.focus = self.fork.as_ref().map_or(PaneId::Main, |(pane, _)| *pane);
            } else if self.main_area.contains(position)
                && let Some(main_pane) = self.main_pane()
            {
                self.focus = main_pane;
            }
        }
        self.update_root(self.focus, RootEvent::Terminal(event))
    }

    pub(crate) fn refresh_terminal_images(&mut self) {
        for root in [&mut self.main, &mut self.fork]
            .into_iter()
            .filter_map(Option::as_mut)
            .map(|(_, root)| root)
        {
            root.component_mut().refresh_terminal_images();
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

    fn update_all(&mut self, event: RootEvent) -> ComponentUpdate<AppEffect> {
        let main = self.main.take().map(|(pane, mut root)| {
            let update = root.update(event_for_other_pane(&event));
            self.main = Some((pane, root));
            self.map_root_update(pane, update)
        });
        let fork = self.fork.take().map(|(pane, mut root)| {
            let update = root.update(event);
            self.fork = Some((pane, root));
            self.map_root_update(pane, update)
        });
        merge_updates(main, fork)
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
        for effect in update.effects {
            match effect {
                RootEffect::Fork => {
                    if self.fork.is_none() && self.main.is_some() {
                        let (pane, parent) = self.begin_fork();
                        effects.push(AppEffect::OpenFork { pane, parent });
                    }
                }
                RootEffect::Shutdown => {
                    effects.push(AppEffect::Shutdown);
                }
                RootEffect::SetTheme(mode) => {
                    self.set_theme_mode(mode);
                    effects.push(AppEffect::SetTheme(mode));
                }
                effect => effects.push(AppEffect::Pane { pane, effect }),
            }
        }
        ComponentUpdate {
            effects,
            render: update.render,
        }
    }

    fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.theme.set_mode(mode);
        if let Some((_, main)) = &mut self.main {
            main.component_mut().set_theme_mode(mode);
        }
        if let Some((_, fork)) = &mut self.fork {
            fork.component_mut().set_theme_mode(mode);
        }
    }

    pub(crate) fn set_max_subagents(&mut self, limit: usize) {
        if let Some((_, main)) = &mut self.main {
            main.component_mut().set_max_subagents(limit);
        }
        if let Some((_, fork)) = &mut self.fork {
            fork.component_mut().set_max_subagents(limit);
        }
    }

    pub(crate) fn set_preferred_reasoning_mode(&mut self, mode: ReasoningMode) {
        if let Some((_, main)) = &mut self.main {
            main.component_mut().set_preferred_reasoning_mode(mode);
        }
        if let Some((_, fork)) = &mut self.fork {
            fork.component_mut().set_preferred_reasoning_mode(mode);
        }
    }

    pub(crate) fn set_memory_enabled(&mut self, enabled: bool) {
        if let Some((_, main)) = &mut self.main {
            main.component_mut().set_memory_enabled(enabled);
        }
        if let Some((_, fork)) = &mut self.fork {
            fork.component_mut().set_memory_enabled(enabled);
        }
    }

    fn begin_fork(&mut self) -> (PaneId, PaneId) {
        if let Some((_, main)) = &mut self.main {
            main.component_mut().set_fork_available(false);
        }
        let (parent, main) = self
            .main
            .as_ref()
            .expect("forking requires the primary pane");
        let thinking = main.component().composer().effort();
        let fork = main.component().fork(&self.workspace, thinking);
        let pane = PaneId::Fork(self.next_fork);
        self.next_fork = self.next_fork.saturating_add(1);
        self.fork = Some((pane, Node::new(fork)));
        self.focus = pane;
        (pane, *parent)
    }

    fn remove_pane(&mut self, pane: PaneId) {
        if self.main.as_ref().is_some_and(|(id, _)| *id == pane) {
            self.main = self.fork.take();
        } else if self.fork.as_ref().is_some_and(|(id, _)| *id == pane) {
            self.fork = None;
        } else {
            return;
        }
        if let Some((main_pane, main)) = &mut self.main {
            self.focus = *main_pane;
            if self.fork.is_none() {
                main.component_mut().set_fork_available(true);
            }
        }
    }

    pub(crate) fn main_pane(&self) -> Option<PaneId> {
        self.main.as_ref().map(|(pane, _)| *pane)
    }

    fn pane(&self, pane: PaneId) -> Option<&Node<RootNode>> {
        self.main
            .as_ref()
            .filter(|(main_pane, _)| *main_pane == pane)
            .or_else(|| {
                self.fork
                    .as_ref()
                    .filter(|(fork_pane, _)| *fork_pane == pane)
            })
            .map(|(_, root)| root)
    }

    fn pane_mut(&mut self, pane: PaneId) -> Option<&mut Node<RootNode>> {
        if self
            .main
            .as_ref()
            .is_some_and(|(main_pane, _)| *main_pane == pane)
        {
            return self.main.as_mut().map(|(_, root)| root);
        }
        self.fork
            .as_mut()
            .filter(|(fork_pane, _)| *fork_pane == pane)
            .map(|(_, root)| root)
    }
}

fn event_for_other_pane(event: &RootEvent) -> RootEvent {
    match event {
        RootEvent::Terminal(Event::Resize(width, height)) => {
            RootEvent::Terminal(Event::Resize(*width, *height))
        }
        RootEvent::AnimationFrame(now) => RootEvent::AnimationFrame(*now),
        _ => unreachable!("only broadcast events are cloned"),
    }
}

fn merge_updates(
    first: Option<ComponentUpdate<AppEffect>>,
    second: Option<ComponentUpdate<AppEffect>>,
) -> ComponentUpdate<AppEffect> {
    let mut merged = ComponentUpdate::none();
    for mut update in first.into_iter().chain(second) {
        merged.effects.append(&mut update.effects);
        merged.render = merged.render.max(update.render);
    }
    merged
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
        app::config::{ReasoningEffort, ReasoningMode},
        tui::{
            pane::PaneId,
            theme::{ColorScheme, Theme, ThemeMode},
            transcript::{LocalEvent, TranscriptRecord, TurnId},
        },
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nanocodex::Model;
    use ratatui::{Terminal, backend::TestBackend};
    use semver::Version;
    use std::{path::PathBuf, sync::Arc};
    use tact_memory::{MemoryAccess, MemoryKey, MemoryRecord, MemorySource};

    fn local_memory_access() -> MemoryAccess {
        MemoryAccess {
            source: MemorySource::Local,
            namespace: None,
            role: None,
        }
    }

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
            fast_mode: false,
            model: Model::Luna,
            draft_reset: DraftReset::Preserve,
            skills: Arc::from([]),
        });

        let root = app.root(PaneId::Main).unwrap();
        assert_eq!(root.composer().draft(), "send with luna");
        assert_eq!(root.composer().model(), Model::Luna);
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
            LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "inherited history".to_owned(),
            },
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
        app.update_root(PaneId::Main, RootEvent::ContextTokens(136_000));

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
    fn handoff_starts_a_fresh_session_with_an_editable_draft() {
        let mut app = app();

        let update = app.update(AppEvent::HandoffReady {
            pane: PaneId::Main,
            prompt: "Continue from the validated parser design.".to_owned(),
            effort: ReasoningEffort::High,
            reasoning_mode: ReasoningMode::Standard,
            fast_mode: false,
            skills: Arc::from([]),
        });

        assert!(update.effects.is_empty());
        let root = app.root(PaneId::Main).expect("the main pane should remain");
        assert_eq!(
            root.composer().draft(),
            "Continue from the validated parser design."
        );
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
            access: local_memory_access(),
            records: vec![memory_record(1, "main pane memory")],
        });
        app.update(AppEvent::MemoriesLoaded {
            pane: PaneId::Fork(1),
            access: local_memory_access(),
            records: vec![memory_record(2, "fork pane memory")],
        });
        let output = rendered(&mut app, 120, 24);

        assert_eq!(output.matches("main pane memory").count(), 1);
        assert_eq!(output.matches("fork pane memory").count(), 1);
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
            preferred_reasoning_mode: ReasoningMode::Standard,
            memory_enabled: false,
            message: "disabled memory".to_owned(),
        });
        for pane in [PaneId::Main, PaneId::Fork(1)] {
            assert!(open_memory(&mut app, pane).effects.is_empty());
        }
    }
}
