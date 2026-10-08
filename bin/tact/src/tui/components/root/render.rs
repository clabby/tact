//! Drawing the root layout: transcript, queue, composer, overlays, and transient chrome.

use super::{ConfirmationAction, Overlay, RootNode, SubagentOverlay};
use crate::{
    app::theme::Theme,
    tui::components::{floating::Floating, node::Component, selection::Surface},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

impl RootNode {
    pub(super) fn render_root(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        focused: bool,
    ) {
        let height = self.composer.desired_height(area.width).min(area.height);
        let composer_area = Rect {
            y: area.bottom().saturating_sub(height),
            height,
            ..area
        };
        self.composer_area = composer_area;
        let queue_height = self
            .queue
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
        self.composer.render_focused_with_selection(
            frame,
            composer_area,
            theme,
            focused
                && self.blocking_task.is_none()
                && !self.transcript.expandables_focused()
                && (!self.queue.focused() || self.queue_edit.is_some()),
            composer_selection,
        );
        if self.selection.surface() == Some(Surface::Transcript)
            && let Some(range) = self.selection.range()
        {
            self.transcript.render_selection(frame.buffer_mut(), range);
        }
        self.transcript.render_chrome(frame, transcript_area, theme);
        if let Some(overlay) = &mut self.overlay {
            match overlay {
                Overlay::Actions(actions) => actions.render(frame, area, theme),
                Overlay::ContextDiagnostics(panel) => panel.render(frame, area, theme),
                Overlay::Effort(selector) => selector.render(frame, area, theme),
                Overlay::Speed(selector) => selector.render(frame, area, theme),
                Overlay::Model(selector) => selector.render(frame, area, theme),
                Overlay::Theme(selector) => selector.render(frame, area, theme),
                Overlay::FileFinder(mention) => mention.finder.render(frame, area, theme),
                Overlay::Skills(mention) => mention.picker.render(frame, area, theme),
                Overlay::Keybindings(help) => help.render(frame, area, theme),
                Overlay::QrCode(view) => view.render(frame, area, theme),
                Overlay::Memory(browser) => browser.render(frame, area, theme),
                Overlay::RecentPrompts(picker) => picker.render(frame, area, theme),
                Overlay::Sessions(picker) => picker.render(frame, area, theme),
                Overlay::WebInstall(confirmation) => confirmation.render(frame, area, theme),
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
            Style::reset().add_modifier(Modifier::BOLD),
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
        Span::styled(key, Style::reset().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {label}"), Style::default().fg(theme.muted())),
    ])
}
