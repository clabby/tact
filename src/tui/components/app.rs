//! Application-level ownership for the primary and optional forked panes.

use super::{
    node::{ComponentUpdate, Node, RenderRequest},
    queue::QueueId,
    root::{RootEffect, RootEvent, RootNode},
};
use crate::{
    config::ReasoningEffort,
    subagents::AgentUpdate,
    tui::{
        context::completed_transcript_tokens,
        pane::PaneId,
        session::SessionSummary,
        theme::{ColorScheme, Theme, ThemeMode},
        transcript::TranscriptRecord,
    },
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders},
};
use semver::Version;
use std::{path::PathBuf, sync::Arc, time::Instant};
use unicode_width::UnicodeWidthStr;

const SPLIT_HINT: &str = " mouse: focus · Ctrl+C: clear / close ";
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
    QueueEditorFinished {
        pane: PaneId,
        index: usize,
        text: String,
    },
    WorkerTurnFinished(PaneId),
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
    ForkReady(PaneId),
    ForkFailed {
        pane: PaneId,
        error: String,
    },
    NewSessionReady {
        pane: PaneId,
        effort: ReasoningEffort,
        fast_mode: bool,
    },
    NewSessionFailed {
        pane: PaneId,
        error: String,
    },
    SessionsLoaded {
        pane: PaneId,
        sessions: Vec<SessionSummary>,
    },
    SessionLoadFailed {
        pane: PaneId,
        error: String,
    },
    SessionRestored {
        pane: PaneId,
        records: Vec<Arc<TranscriptRecord>>,
        effort: ReasoningEffort,
        fast_mode: bool,
    },
    NotifyError {
        pane: PaneId,
        error: String,
    },
    NotifySuccess {
        pane: PaneId,
        message: String,
    },
    UpdateAvailable(Version),
    ConfigReloaded {
        pane: PaneId,
        theme: Theme,
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
    OpenFork(PaneId),
    ClosePane(PaneId),
    SetTheme(ThemeMode),
    Shutdown,
}

pub(crate) struct AppNode {
    theme: Theme,
    workspace: PathBuf,
    main: Option<Node<RootNode>>,
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
            main: Some(Node::new(root)),
            fork: None,
            focus: PaneId::Main,
            main_area: Rect::default(),
            fork_area: Rect::default(),
            next_fork: 1,
        }
    }

    pub(crate) fn update(&mut self, event: AppEvent) -> ComponentUpdate<AppEffect> {
        match event {
            AppEvent::Terminal(event) => self.update_terminal(event),
            AppEvent::PasteImage(data_url) => {
                self.update_root(self.focus, RootEvent::PasteImage(data_url))
            }
            AppEvent::Transcript { pane, record } => {
                let tokens = completed_transcript_tokens(&record);
                let mut update = self.update_root(pane, RootEvent::Transcript(record));
                if let Some(tokens) = tokens {
                    let context = self.update_root(pane, RootEvent::ContextTokens(tokens));
                    update.effects.extend(context.effects);
                    update.render = update.render.max(context.render);
                }
                update
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
            AppEvent::QueueEditorFinished { pane, index, text } => {
                self.update_root(pane, RootEvent::RestoreQueued { index, text })
            }
            AppEvent::WorkerTurnFinished(pane) => {
                self.update_root(pane, RootEvent::WorkerTurnFinished)
            }
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
            AppEvent::ForkReady(pane) => self.update_root(pane, RootEvent::ForkReady),
            AppEvent::ForkFailed { pane, error } => {
                self.remove_pane(pane);
                let target = if self.main.is_some() {
                    PaneId::Main
                } else {
                    self.focus
                };
                self.update_root(
                    target,
                    RootEvent::NotifyError(format!("Could not fork session: {error}")),
                )
            }
            AppEvent::NewSessionReady {
                pane,
                effort,
                fast_mode,
            } => {
                let workspace = self.workspace.clone();
                let Some(root) = self.pane_mut(pane) else {
                    return ComponentUpdate::none();
                };
                root.component_mut().reset_session(&workspace, effort);
                root.component_mut().set_fast_mode(fast_mode);
                ComponentUpdate::render(RenderRequest::Immediate)
            }
            AppEvent::NewSessionFailed { pane, error } => {
                self.update_root(pane, RootEvent::NewSessionFailed(error))
            }
            AppEvent::SessionsLoaded { pane, sessions } => {
                self.update_root(pane, RootEvent::SessionsLoaded(sessions))
            }
            AppEvent::SessionLoadFailed { pane, error } => {
                self.update_root(pane, RootEvent::SessionLoadFailed(error))
            }
            AppEvent::SessionRestored {
                pane,
                records,
                effort,
                fast_mode,
            } => self.update_root(
                pane,
                RootEvent::SessionRestored {
                    records,
                    effort,
                    fast_mode,
                },
            ),
            AppEvent::NotifyError { pane, error } => {
                self.update_root(pane, RootEvent::NotifyError(error))
            }
            AppEvent::NotifySuccess { pane, message } => {
                self.update_root(pane, RootEvent::NotifySuccess(message))
            }
            AppEvent::UpdateAvailable(version) => {
                let pane = if self.main.is_some() {
                    PaneId::Main
                } else {
                    self.focus
                };
                self.update_root(pane, RootEvent::UpdateAvailable(version))
            }
            AppEvent::ConfigReloaded {
                pane,
                theme,
                message,
            } => {
                self.theme.replace_from_config(theme);
                let mode = self.theme.mode();
                if let Some(main) = &mut self.main {
                    main.component_mut().set_theme_mode(mode);
                }
                if let Some((_, fork)) = &mut self.fork {
                    fork.component_mut().set_theme_mode(mode);
                }
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
            if let Some(main) = &mut self.main {
                main.component_mut().render_focused(
                    frame,
                    area,
                    &self.theme,
                    self.focus == PaneId::Main,
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
        if let Some(main) = &mut self.main {
            main.component_mut().render_focused(
                frame,
                main_content,
                &self.theme,
                self.focus == PaneId::Main,
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
                .and_then(|root| root.component().animation_deadline()),
            self.fork
                .as_ref()
                .and_then(|(_, root)| root.component().animation_deadline()),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn update_terminal(&mut self, event: Event) -> ComponentUpdate<AppEffect> {
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
            } else if self.main_area.contains(position) {
                self.focus = PaneId::Main;
            }
        }
        self.update_root(self.focus, RootEvent::Terminal(event))
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
        let main = self.main.take().map(|mut root| {
            let update = root.update(event_for_other_pane(&event));
            self.main = Some(root);
            self.map_root_update(PaneId::Main, update)
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
                        effects.push(AppEffect::OpenFork(self.begin_fork()));
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
        if let Some(main) = &mut self.main {
            main.component_mut().set_theme_mode(mode);
        }
        if let Some((_, fork)) = &mut self.fork {
            fork.component_mut().set_theme_mode(mode);
        }
    }

    fn begin_fork(&mut self) -> PaneId {
        if let Some(main) = &mut self.main {
            main.component_mut().set_fork_available(false);
        }
        let main = self
            .main
            .as_ref()
            .expect("forking requires the primary pane");
        let thinking = main.component().composer().effort();
        let fork = main.component().fork(&self.workspace, thinking);
        let pane = PaneId::Fork(self.next_fork);
        self.next_fork = self.next_fork.saturating_add(1);
        self.fork = Some((pane, Node::new(fork)));
        self.focus = pane;
        pane
    }

    fn remove_pane(&mut self, pane: PaneId) {
        match pane {
            PaneId::Main => self.main = None,
            PaneId::Fork(_) if self.fork.as_ref().is_some_and(|(id, _)| *id == pane) => {
                self.fork = None;
            }
            PaneId::Fork(_) => return,
        }
        if self.main.is_some() {
            self.focus = PaneId::Main;
            if self.fork.is_none()
                && let Some(main) = &mut self.main
            {
                main.component_mut().set_fork_available(true);
            }
        } else if self.fork.is_some() {
            self.focus = self.fork.as_ref().map_or(PaneId::Main, |(pane, _)| *pane);
        }
    }

    fn pane(&self, pane: PaneId) -> Option<&Node<RootNode>> {
        match pane {
            PaneId::Main => self.main.as_ref(),
            PaneId::Fork(_) => self
                .fork
                .as_ref()
                .filter(|(fork_pane, _)| *fork_pane == pane)
                .map(|(_, root)| root),
        }
    }

    fn pane_mut(&mut self, pane: PaneId) -> Option<&mut Node<RootNode>> {
        match pane {
            PaneId::Main => self.main.as_mut(),
            PaneId::Fork(_) => self
                .fork
                .as_mut()
                .filter(|(fork_pane, _)| *fork_pane == pane)
                .map(|(_, root)| root),
        }
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
    use super::{AppEffect, AppEvent, AppNode, RootEvent, RootNode, SPLIT_HINT};
    use crate::{
        config::ReasoningEffort,
        tui::{
            pane::PaneId,
            theme::{ColorScheme, Theme, ThemeMode},
            transcript::{LocalEvent, TranscriptRecord, TurnId},
        },
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use semver::Version;
    use std::{path::PathBuf, sync::Arc};

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

        let update = app.update(control('f'));

        assert!(matches!(
            update.effects.as_slice(),
            [AppEffect::OpenFork(PaneId::Fork(1))]
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

        app.update(control('f'));

        assert_eq!(
            app.root(PaneId::Fork(1))
                .unwrap()
                .composer()
                .context_tokens(),
            136_000
        );
    }

    #[test]
    fn fork_effort_changes_do_not_change_the_primary_composer() {
        let mut app = app();
        app.update(control('f'));
        app.update(AppEvent::ForkReady(PaneId::Fork(1)));
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
                effect: super::RootEffect::SetEffort(ReasoningEffort::Medium),
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
    fn split_view_shows_mouse_focus_and_close_hint() {
        let mut app = app();
        assert!(!rendered(&mut app, 100, 20).contains(SPLIT_HINT.trim()));

        app.update(control('f'));
        let rendered = rendered(&mut app, 100, 20);

        assert!(rendered.contains(SPLIT_HINT.trim()));
    }

    #[test]
    fn split_hint_is_hidden_at_narrow_widths() {
        let mut app = app();
        app.update(control('f'));

        let rendered = rendered(&mut app, 40, 10);

        assert!(!rendered.contains(SPLIT_HINT.trim()));
    }

    #[test]
    fn control_c_closes_only_the_focused_pane_when_multiplexed() {
        let mut app = app();
        app.update(control('f'));

        let update = app.update(control('c'));

        assert!(matches!(
            update.effects.as_slice(),
            [AppEffect::ClosePane(PaneId::Fork(1))]
        ));
        assert!(app.root(PaneId::Main).is_some());
        assert!(app.root(PaneId::Fork(1)).is_none());
    }

    #[test]
    fn control_c_clears_the_focused_fork_composer_before_closing_it() {
        let mut app = app();
        app.update(control('f'));
        app.update(AppEvent::ForkReady(PaneId::Fork(1)));
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
        app.update(control('f'));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.update(AppEvent::Terminal(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));

        let update = app.update(control('c'));

        assert!(matches!(
            update.effects.as_slice(),
            [AppEffect::ClosePane(PaneId::Main)]
        ));
        assert!(app.root(PaneId::Main).is_none());
        assert!(app.root(PaneId::Fork(1)).is_some());
    }

    #[test]
    fn a_second_fork_is_unavailable_until_the_first_closes() {
        let mut app = app();
        app.update(control('f'));
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.update(AppEvent::Terminal(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));

        let update = app.update(control('f'));

        assert!(update.effects.is_empty());
        assert!(app.root(PaneId::Fork(1)).is_some());
        assert!(app.root(PaneId::Fork(2)).is_none());
    }

    #[test]
    fn fork_failure_closes_the_pending_pane_and_surfaces_the_error() {
        let mut app = app();
        app.update(control('f'));

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
}
