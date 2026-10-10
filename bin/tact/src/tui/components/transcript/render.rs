//! Conversion of transcript entries into cached terminal layouts.
//!
//! An [EntryRenderer] renders one entry at a fixed width. Its output carries the
//! styled rows together with the per-row link and selection spans that hit
//! testing relies on, so every row it emits must have matching link and selection
//! rows. Image-backed rows are requested through the shared image cache and are
//! re-rendered once deferred image preparation completes.

use super::{image, markdown, message, tool};
use crate::{
    app::theme::Theme,
    core::transcript::{EntryKind, ToolEntry, TranscriptEntry, UserImage},
    tui::format::{format_duration, format_turn_duration, normalize_line_endings},
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{borrow::Cow, ops::Range, path::Path, sync::Arc};

/// Columns reserved for the tree connector in front of a nested workflow tool.
const NESTED_TOOL_INDENT: u16 = 4;
/// Columns occupied by the gutter in front of user prompt rows.
const USER_GUTTER_WIDTH: u16 = 2;

/// Renders transcript entries at one width, borrowing the session's workspace for
/// relative image paths and its image cache for prepared image protocols.
pub(super) struct EntryRenderer<'a> {
    pub(super) width: u16,
    pub(super) theme: &'a Theme,
    pub(super) workspace: &'a Path,
    pub(super) images: &'a mut image::Cache,
}

impl EntryRenderer<'_> {
    pub(super) fn entry(
        &mut self,
        entry: &TranscriptEntry,
        live_duration_ns: Option<u64>,
        expanded: bool,
    ) -> markdown::Layout {
        let (width, theme) = (self.width, self.theme);
        let mut layout = match &entry.kind {
            EntryKind::User { text, images } => self.user_with_images(text, images),
            EntryKind::Assistant { text, .. } => {
                markdown::render_cached(text, width, theme, self.workspace, self.images)
            }
            EntryKind::Reasoning { text } => {
                let mut layout = markdown::render_cached(
                    text,
                    width.saturating_sub(2),
                    theme,
                    self.workspace,
                    self.images,
                );
                let style = Style::default()
                    .fg(theme.muted())
                    .add_modifier(Modifier::ITALIC);
                for line in &mut layout.lines {
                    for span in &mut line.spans {
                        span.style = span.style.patch(style);
                    }
                }
                layout
            }
            EntryKind::Tool(tool) => {
                let indent = nested_tool_indent(entry, width);
                let mut layout = tool::render_layout(
                    tool,
                    live_duration_ns,
                    width.saturating_sub(indent),
                    theme,
                    expanded,
                );
                indent_nested_tool(
                    indent,
                    &mut layout.lines,
                    theme,
                    expanded,
                    entry.trailing_spacer,
                );
                for span in layout.selections.iter_mut().flatten() {
                    span.columns.start = span.columns.start.saturating_add(indent);
                    span.columns.end = span.columns.end.saturating_add(indent);
                }
                layout
            }
            EntryKind::DirectedMessage(thread) => {
                markdown::Layout::plain(message::render(thread, width, theme, expanded))
            }
            EntryKind::SessionMessage {
                from_session_id,
                text,
            } => markdown::Layout::plain(message::render_session_message(
                from_session_id,
                text,
                width,
                theme,
            )),
            EntryKind::ForkedFrom { session_id } => {
                markdown::Layout::plain(vec![Line::from(Span::styled(
                    format!("◇ Forked from @@{session_id}"),
                    Style::default().fg(theme.muted()),
                ))])
            }
            EntryKind::EffortChanged { to } => self.setting_changed(
                "◇ Effort changed to ",
                Span::styled(
                    to.as_str(),
                    Style::default()
                        .fg(theme.effort(*to))
                        .add_modifier(Modifier::BOLD),
                ),
            ),
            EntryKind::SpeedChanged { speed } => self.setting_changed(
                "◇ Speed changed to ",
                Span::styled(
                    speed.as_str(),
                    Style::default()
                        .fg(theme.speed(*speed))
                        .add_modifier(Modifier::BOLD),
                ),
            ),
            EntryKind::ReflectionStarted => {
                self.marker("◇ Reflection started".to_owned(), theme.muted())
            }
            EntryKind::Interrupted { count } => {
                let label = if *count == 1 {
                    "◇ Interrupted response".to_owned()
                } else {
                    format!("◇ Interrupted {count} responses")
                };
                self.marker(label, theme.border())
            }
            EntryKind::ContextCompacted { duration_ns } => self.marker(
                format!("◇ Context compacted · {}", format_duration(*duration_ns)),
                theme.muted(),
            ),
            EntryKind::TurnCompleted { duration_ns } => self.marker(
                format!("◇ Turn completed · {}", format_turn_duration(*duration_ns)),
                theme.muted(),
            ),
            EntryKind::ContextCompactionFailed { message } => self.marker(
                format!("◇ Context compaction failed · continuing · {message}"),
                theme.thinking_high(),
            ),
            EntryKind::Error { message } => markdown::Layout::plain(markdown::wrap_plain(
                &format!("× {message}"),
                width,
                Style::default().fg(theme.thinking_xhigh()),
            )),
        };
        if entry.trailing_spacer {
            layout.lines.push(Line::default());
            layout.links.push(Vec::new());
            layout.selections.push(Vec::new());
        }
        layout
    }

    /// Renders only the summary rows of a running tool, which change every time
    /// its live timer ticks while the rest of the tool layout stays cached.
    pub(super) fn live_tool_summary(
        &self,
        entry: &TranscriptEntry,
        tool: &ToolEntry,
        duration_ns: u64,
        expanded: bool,
    ) -> Vec<Line<'static>> {
        let indent = nested_tool_indent(entry, self.width);
        let mut lines = tool::render_live_summary(
            tool,
            duration_ns,
            self.width.saturating_sub(indent),
            self.theme,
            expanded,
        );
        indent_nested_tool(
            indent,
            &mut lines,
            self.theme,
            expanded,
            entry.trailing_spacer,
        );
        lines
    }

    fn marker(&self, label: String, color: Color) -> markdown::Layout {
        markdown::Layout::plain(vec![Line::from(Span::styled(
            label,
            Style::default().fg(color),
        ))])
    }

    fn setting_changed(&self, label: &'static str, value: Span<'static>) -> markdown::Layout {
        let muted = Style::default().fg(self.theme.muted());
        markdown::Layout::plain(vec![Line::from(vec![
            Span::styled(label, muted),
            value,
            Span::styled(" · takes effect on the next turn", muted),
        ])])
    }

    /// Renders a user prompt whose image markers are replaced by image rows.
    ///
    /// Selection spans keep byte offsets into the original prompt text, so text
    /// on either side of an image remains selectable as part of the same source.
    fn user_with_images(&mut self, text: &str, attached: &[UserImage]) -> markdown::Layout {
        if attached.is_empty() {
            return self.user(text);
        }
        let theme = self.theme;
        let mut layout = markdown::Layout::plain(Vec::new());
        let mut offset = 0;
        for attached in attached {
            let range = &attached.range;
            if range.start < offset || text.get(range.clone()).is_none() {
                continue;
            }
            self.append_user_text(&mut layout, text, offset..range.start);
            let marker = &text[range.clone()];
            let destination = image::materialize_user_image(&attached.data_url);
            if layout.image_state == markdown::ImageState::None {
                layout.image_state = markdown::ImageState::Ready;
            }
            let result = destination.as_ref().map(|destination| {
                (
                    destination,
                    self.images.load(destination, self.workspace, self.width),
                )
            });
            let link_style = Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::UNDERLINED);
            let (label, style) = match result {
                Some((destination, image::LoadResult::Loaded(protocol))) => {
                    let line = layout.lines.len();
                    let size = protocol.size();
                    for _ in 0..size.height {
                        layout
                            .lines
                            .push(Line::from(Span::raw(" ".repeat(usize::from(size.width)))));
                        layout.selections.push(Vec::new());
                    }
                    layout.images.push(markdown::ImagePlacement {
                        line,
                        destination: Arc::from(destination.as_str()),
                        protocol,
                        retransmit: false,
                    });
                    offset = range.end;
                    continue;
                }
                Some((_, image::LoadResult::Deferred)) => {
                    layout.image_state = markdown::ImageState::Pending;
                    (marker, link_style)
                }
                Some((_, image::LoadResult::Unsupported)) => (marker, link_style),
                Some((_, image::LoadResult::Failed)) => (
                    "image could not be rendered",
                    Style::default().fg(Color::Red),
                ),
                None => (marker, Style::default().fg(theme.thinking_medium())),
            };
            let gutter = Span::styled("┃ ", Style::default().fg(theme.thinking_medium()));
            for line in markdown::wrap_plain_preserving_whitespace(
                label,
                self.width.saturating_sub(USER_GUTTER_WIDTH).max(1),
                style,
            ) {
                layout.lines.push(Line::from(
                    std::iter::once(gutter.clone())
                        .chain(line.spans)
                        .collect::<Vec<_>>(),
                ));
                layout.selections.push(Vec::new());
            }
            offset = range.end;
        }
        self.append_user_text(&mut layout, text, offset..text.len());
        layout.links = vec![Vec::new(); layout.lines.len()];
        layout
    }

    /// Appends the prompt text in `range` that surrounds image markers.
    ///
    /// The line break separating the text from an adjacent marker is dropped
    /// because the marker already occupies its own rows.
    fn append_user_text(&self, layout: &mut markdown::Layout, text: &str, mut range: Range<usize>) {
        if range.start > 0 {
            let segment = &text[range.clone()];
            range.start += if segment.starts_with("\r\n") {
                2
            } else {
                usize::from(segment.starts_with(['\r', '\n']))
            };
        }
        if range.end < text.len() {
            let segment = &text[range.clone()];
            range.end -= if segment.ends_with("\r\n") {
                2
            } else {
                usize::from(segment.ends_with(['\r', '\n']))
            };
        }
        if range.is_empty() {
            return;
        }
        let mut offset = range.start;
        loop {
            let remaining = &text[offset..range.end];
            let end = remaining.find(['\r', '\n']).unwrap_or(remaining.len());
            let rendered = self.user(&remaining[..end]);
            layout.lines.extend(rendered.lines);
            for mut selections in rendered.selections {
                for selection in &mut selections {
                    selection.source.start += offset;
                    selection.source.end += offset;
                }
                layout.selections.push(selections);
            }
            if end == remaining.len() {
                break;
            }
            offset += end
                + if remaining[end..].starts_with("\r\n") {
                    2
                } else {
                    1
                };
        }
    }

    /// Renders prompt text behind the user gutter, preserving its whitespace.
    pub(super) fn user(&self, text: &str) -> markdown::Layout {
        let text = normalize_line_endings(text);
        let style = Style::default().fg(self.theme.thinking_medium());
        let content_width = self.width.saturating_sub(USER_GUTTER_WIDTH).max(1);
        let mut lines = Vec::new();
        let mut selections = Vec::new();
        let mut source_offset = 0;
        for logical in text.split('\n') {
            let wrapped = markdown::wrap_plain_preserving_whitespace(logical, content_width, style);
            let wrapped_selections = markdown::plain_selection_spans(logical, &wrapped);
            for (line, mut line_selections) in wrapped.into_iter().zip(wrapped_selections) {
                for selection in &mut line_selections {
                    selection.columns.start =
                        selection.columns.start.saturating_add(USER_GUTTER_WIDTH);
                    selection.columns.end = selection.columns.end.saturating_add(USER_GUTTER_WIDTH);
                    selection.source.start = selection.source.start.saturating_add(source_offset);
                    selection.source.end = selection.source.end.saturating_add(source_offset);
                }
                lines.push(Line::from(
                    std::iter::once(Span::styled("┃ ", style))
                        .chain(line.spans)
                        .collect::<Vec<_>>(),
                ));
                selections.push(line_selections);
            }
            source_offset = source_offset
                .saturating_add(logical.len())
                .saturating_add(1);
        }
        markdown::Layout {
            selection_source: match text {
                Cow::Borrowed(_) => None,
                Cow::Owned(text) => Some(text),
            },
            selections,
            ..markdown::Layout::plain(lines)
        }
    }
}

/// Indentation of a workflow child tool beneath its parent, shrinking on narrow
/// terminals so at least one column of content remains.
pub(super) const fn nested_tool_indent(entry: &TranscriptEntry, width: u16) -> u16 {
    if entry.parent.is_none() {
        return 0;
    }
    let available = width.saturating_sub(1);
    if available < NESTED_TOOL_INDENT {
        available
    } else {
        NESTED_TOOL_INDENT
    }
}

/// Prefixes nested tool rows with tree connectors. The last child of a workflow
/// closes the tree on its final visible row.
fn indent_nested_tool(
    indent: u16,
    lines: &mut [Line<'static>],
    theme: &Theme,
    expanded: bool,
    terminal: bool,
) {
    let (terminal_marker, continuing_marker, continuation) = match indent {
        0 => return,
        1 => ("└", "├", "│"),
        2 => ("└─", "├─", "│ "),
        3 => (" └─", " ├─", " │ "),
        _ => ("  └─", "  ├─", "  │ "),
    };
    let line_count = lines.len();
    for (index, line) in lines.iter_mut().enumerate() {
        let marker = if index == 0 {
            if expanded || !terminal || line_count > 1 {
                continuing_marker
            } else {
                terminal_marker
            }
        } else if !expanded && terminal && index + 1 == line_count {
            terminal_marker
        } else {
            continuation
        };
        line.spans
            .insert(0, Span::styled(marker, Style::default().fg(theme.border())));
    }
}
