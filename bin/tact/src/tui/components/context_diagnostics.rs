//! Read-only context telemetry overlay.
//!
//! [ContextDiagnosticsPanel] renders one [ContextDiagnostics] snapshot as a scrollable report, top
//! to bottom: the active context against the model window and the auto-compact limit; the
//! estimated composition of the latest call's input by category; per-tool totals and the largest
//! single items; context growth over recent calls; prompt-cache reuse; and generation and
//! compaction facts. Data the snapshot lacks renders as "unavailable" rather than as zero, and
//! every color comes from the active [Theme].
//!
//! The panel never gathers telemetry itself: `r` emits [ContextDiagnosticsEffect::Refresh] and the
//! host answers with a new snapshot through [ContextDiagnosticsPanel::replace]. Esc emits
//! [ContextDiagnosticsEffect::Dismiss]. The arrow, page, Home, and End keys scroll a report taller
//! than the panel.

use super::{
    fit::ellipsize,
    floating::Floating,
    node::{Component, ComponentUpdate, RenderRequest},
};
use crate::{
    app::theme::Theme,
    core::context::{
        CallPoint, CategoryUsage, CompactionDiagnostics, CompactionTrigger, ContextCategory,
        ContextDiagnostics, ContinuationMode, LargestItem, ToolUsage,
    },
};
use chrono::{DateTime, Utc};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use nanocodex::{ClaudeModel, HarnessModel, Model as CodexModel};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use std::{borrow::Cow, cmp::Reverse, iter};
use unicode_width::UnicodeWidthStr;

const TITLE: &str = "Context diagnostics";
const PANEL_WIDTH: u16 = 100;
const PANEL_HEIGHT: u16 = 38;
/// Rows the floating chrome adds around the report: two borders and the footer.
const CHROME_ROWS: u16 = 3;
/// Columns between the panel border and the report on each side.
const MARGIN: u16 = 1;
const FOOTER: [(&str, &str); 2] = [("r", "refresh"), ("esc", "close")];
const SCROLL_FOOTER: [(&str, &str); 3] = [("↑↓", "scroll"), ("r", "refresh"), ("esc", "close")];
/// The report width from which paired sections sit side by side.
const TWO_COLUMN_WIDTH: usize = 90;
const COLUMN_GAP: usize = 3;
const SPARKLINE_ROWS: usize = 3;
/// Sparkline axis labels plus the axis line.
const SPARKLINE_AXIS: usize = 8;
const FACT_LABEL_WIDTH: usize = 16;
/// The name the projection gives the entry that merges tools beyond its table limit.
const OTHER_TOOLS: &str = "(other tools)";
const UNAVAILABLE: &str = "unavailable";
/// Left-aligned partial blocks indexed by the eighths of a cell they cover.
const LEFT_EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
/// Bottom-aligned partial blocks indexed by the eighths of a cell they cover.
const RISING_EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// The number of [ContextCategory] variants; their discriminants follow display order.
const CATEGORY_COUNT: usize = 8;

pub(super) enum ContextDiagnosticsEvent {
    Terminal(Event),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ContextDiagnosticsEffect {
    Dismiss,
    Refresh,
}

pub(super) struct ContextDiagnosticsPanel {
    diagnostics: ContextDiagnostics,
    /// The first visible report row.
    scroll: u16,
    /// The largest useful [Self::scroll] and the visible row count, both from the latest render.
    max_scroll: u16,
    page: u16,
}

impl ContextDiagnosticsPanel {
    pub(super) const fn new(diagnostics: ContextDiagnostics) -> Self {
        Self {
            diagnostics,
            scroll: 0,
            max_scroll: 0,
            page: 1,
        }
    }

    /// Shows a newer snapshot while keeping the reader's scroll position.
    pub(super) fn replace(&mut self, diagnostics: ContextDiagnostics) {
        self.diagnostics = diagnostics;
    }
}

impl Component for ContextDiagnosticsPanel {
    type Event = ContextDiagnosticsEvent;
    type Effect = ContextDiagnosticsEffect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect> {
        let ContextDiagnosticsEvent::Terminal(Event::Key(key)) = event else {
            return ComponentUpdate::none();
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentUpdate::none();
        }
        let mut effects = Vec::new();
        let scroll = match key.code {
            KeyCode::Esc => {
                effects.push(ContextDiagnosticsEffect::Dismiss);
                self.scroll
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => {
                effects.push(ContextDiagnosticsEffect::Refresh);
                self.scroll
            }
            KeyCode::Up => self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll.saturating_sub(self.page),
            KeyCode::PageDown => self.scroll.saturating_add(self.page),
            KeyCode::Home => 0,
            KeyCode::End => u16::MAX,
            _ => return ComponentUpdate::none(),
        };
        self.scroll = scroll.min(self.max_scroll);
        ComponentUpdate {
            effects,
            render: RenderRequest::Immediate,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let palette = Palette::new(theme);
        let report_width = PANEL_WIDTH.min(area.width).saturating_sub(2 + 2 * MARGIN);
        let report = Report {
            diagnostics: &self.diagnostics,
            palette: &palette,
        }
        .lines(usize::from(report_width));
        let report_height = u16::try_from(report.len()).unwrap_or(u16::MAX);
        let panel_height = report_height.saturating_add(CHROME_ROWS);
        let footer: &[(&str, &str)] = if panel_height > PANEL_HEIGHT.min(area.height) {
            &SCROLL_FOOTER
        } else {
            &FOOTER
        };
        let body = Floating::new(TITLE, PANEL_WIDTH, panel_height.min(PANEL_HEIGHT), footer)
            .render(frame, area, theme)
            .body;
        if body.is_empty() {
            return;
        }
        self.page = body.height;
        self.max_scroll = report_height.saturating_sub(body.height);
        self.scroll = self.scroll.min(self.max_scroll);
        let text = Rect {
            x: body.x + MARGIN,
            width: body.width.saturating_sub(2 * MARGIN),
            ..body
        };
        frame.render_widget(Paragraph::new(report).scroll((self.scroll, 0)), text);
        if self.max_scroll == 0 {
            return;
        }
        // The scrollbar replaces the right border beside the report rows.
        let track = Rect {
            x: body.right(),
            width: 1,
            ..body
        };
        let mut state = ScrollbarState::new(usize::from(self.max_scroll) + 1)
            .position(usize::from(self.scroll))
            .viewport_content_length(usize::from(body.height));
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(Style::new().fg(palette.border))
                .thumb_symbol("┃")
                .thumb_style(Style::new().fg(palette.accent)),
            track,
            &mut state,
        );
    }
}

/// Report colors by role, drawn from the active theme.
struct Palette {
    text: Color,
    muted: Color,
    border: Color,
    accent: Color,
    /// Context fill below, approaching, and beyond the auto-compact limit.
    calm: Color,
    warn: Color,
    alarm: Color,
    cached: Color,
    uncached: Color,
    /// One color per category, indexed by [ContextCategory] discriminant.
    categories: [Color; CATEGORY_COUNT],
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        // Neighboring segments of the composition bar must differ, so each category prefers a
        // fitting theme role and falls back to an unused theme color when the active palette
        // reuses one, as the light palette does for its grays. The terminal default color is
        // never used because it vanishes as a segment boundary's background.
        let mut categories = [
            theme.thinking_low(),
            theme.accent(),
            theme.model(HarnessModel::Claude(ClaudeModel::Sonnet55)),
            theme.thinking_max(),
            theme.thinking_high(),
            theme.thinking_medium(),
            theme.thinking_xhigh(),
            theme.muted(),
        ];
        let spares = [
            theme.model(HarnessModel::Claude(ClaudeModel::Fable51)),
            theme.code_text(),
            theme.model(HarnessModel::Codex(CodexModel::Sol)),
            theme.model(HarnessModel::Claude(ClaudeModel::Opus55)),
            theme.model(HarnessModel::Codex(CodexModel::Astra)),
            theme.model(HarnessModel::Claude(ClaudeModel::Haiku55)),
        ];
        for index in 0..categories.len() {
            if categories[index] != Color::Reset
                && !categories[..index].contains(&categories[index])
            {
                continue;
            }
            if let Some(&spare) = spares
                .iter()
                .find(|spare| **spare != Color::Reset && !categories.contains(spare))
            {
                categories[index] = spare;
            }
        }
        Self {
            text: theme.text(),
            muted: theme.muted(),
            border: theme.border(),
            accent: theme.accent(),
            calm: theme.accent(),
            warn: theme.thinking_high(),
            alarm: theme.thinking_xhigh(),
            cached: theme.thinking_medium(),
            uncached: theme.thinking_high(),
            categories,
        }
    }

    const fn category(&self, kind: ContextCategory) -> Color {
        self.categories[kind as usize]
    }

    fn muted(&self) -> Style {
        Style::new().fg(self.muted)
    }

    fn value(&self) -> Style {
        Style::new().fg(self.text)
    }
}

/// A fact label and its value; a missing value reads "unavailable".
type Fact = (&'static str, Option<String>);

/// One snapshot laid out at a fixed width.
struct Report<'a> {
    diagnostics: &'a ContextDiagnostics,
    palette: &'a Palette,
}

impl Report<'_> {
    fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = self.meter(width);
        for section in [
            self.composition(width),
            self.paired(width, Self::tools, Self::largest),
            self.growth(width),
            self.paired(width, Self::cache, Self::session),
        ] {
            lines.push(Line::default());
            lines.extend(section);
        }
        lines
    }

    /// Two sections side by side when the report is wide enough, otherwise stacked.
    fn paired(
        &self,
        width: usize,
        left: fn(&Self, usize) -> Vec<Line<'static>>,
        right: fn(&Self, usize) -> Vec<Line<'static>>,
    ) -> Vec<Line<'static>> {
        if width < TWO_COLUMN_WIDTH {
            let mut lines = left(self, width);
            lines.push(Line::default());
            lines.extend(right(self, width));
            return lines;
        }
        let left_width = (width - COLUMN_GAP) / 2;
        let left = left(self, left_width);
        let right = right(self, width - COLUMN_GAP - left_width);
        let rows = left.len().max(right.len());
        left.into_iter()
            .chain(iter::repeat_with(Line::default))
            .zip(right.into_iter().chain(iter::repeat_with(Line::default)))
            .take(rows)
            .map(|(mut line, right)| {
                let padding = left_width + COLUMN_GAP - line.width().min(left_width);
                line.spans.push(Span::raw(" ".repeat(padding)));
                line.spans.extend(right.spans);
                line
            })
            .collect()
    }

    fn meter(&self, width: usize) -> Vec<Line<'static>> {
        let palette = self.palette;
        let window = self.diagnostics.model_window_tokens;
        let limit = self.diagnostics.auto_compact_token_limit;
        let mut lines = vec![self.heading("Active context", None, width)];
        let Some(active) = self.diagnostics.active_tokens else {
            lines.push(Line::from(vec![
                Span::styled(UNAVAILABLE, palette.muted()),
                Span::styled(
                    format!(" / {} token window", format_count(window)),
                    palette.muted(),
                ),
            ]));
            lines.extend(self.wrapped_facts(
                &[
                    ("auto-compact at ", limit.map(format_count)),
                    (
                        "",
                        Some("no model call has reported its input size".to_owned()),
                    ),
                ],
                width,
            ));
            return lines;
        };
        let fill = match limit {
            Some(limit) if active >= limit => palette.alarm,
            Some(limit) if active >= limit / 10 * 9 => palette.warn,
            _ => palette.calm,
        };
        lines.push(Line::from(vec![
            Span::styled(format_count(active), palette.value().bold()),
            Span::styled(
                format!(" / {} tokens   ", format_count(window)),
                palette.muted(),
            ),
            Span::styled(format_percent(active, window), Style::new().fg(fill).bold()),
            Span::styled(" of the window", palette.muted()),
        ]));
        let limit_column =
            limit.map(|limit| scale(limit, window, width).min(width.saturating_sub(1)));
        if let Some((limit, column)) = limit.zip(limit_column) {
            lines.push(self.limit_marker(limit, column));
        }
        let track = ("░", Style::new().fg(palette.border));
        let mut cells = bar(
            &[(scale_eighths(active, window, width), fill)],
            width,
            track,
        );
        if let Some(cell) = limit_column.and_then(|column| cells.get_mut(column))
            && *cell == track
        {
            *cell = ("│", Style::new().fg(palette.alarm).bold());
        }
        lines.push(Line::from(merge(cells)));
        let mut facts = vec![(
            "headroom ",
            Some(format_count(window.saturating_sub(active))),
        )];
        facts.push(match limit {
            Some(limit) if active < limit => {
                ("until auto-compact ", Some(format_count(limit - active)))
            }
            Some(limit) => (
                "past the auto-compact limit by ",
                Some(format_count(active - limit)),
            ),
            None => ("auto-compact limit ", None),
        });
        lines.extend(self.wrapped_facts(&facts, width));
        lines
    }

    /// The row above the meter that points at the auto-compact limit with a labeled ▼.
    fn limit_marker(&self, limit: u64, column: usize) -> Line<'static> {
        let label = format!("auto-compact {}", format_count(limit));
        let label_style = self.palette.muted();
        let marker = Span::styled("▼", Style::new().fg(self.palette.alarm).bold());
        if label.width() < column {
            Line::from(vec![
                Span::raw(" ".repeat(column - label.width() - 1)),
                Span::styled(label, label_style),
                Span::raw(" "),
                marker,
            ])
        } else {
            Line::from(vec![
                Span::raw(" ".repeat(column)),
                marker,
                Span::raw(" "),
                Span::styled(label, label_style),
            ])
        }
    }

    fn composition(&self, width: usize) -> Vec<Line<'static>> {
        let palette = self.palette;
        let Some(breakdown) = &self.diagnostics.breakdown else {
            let mut lines = vec![self.heading("Composition", None, width)];
            lines.extend(note(
                "Breakdown unavailable for this model: it reports no per-call usage, or no call \
                 has completed yet.",
                width,
                palette.muted(),
            ));
            return lines;
        };
        let total = breakdown.input_tokens;
        let mut lines = vec![self.heading(
            "Composition",
            Some(format!(
                "{} input tokens at the latest call",
                format_count(total)
            )),
            width,
        )];
        let tokens = breakdown
            .categories
            .iter()
            .map(|category| category.tokens)
            .collect::<Vec<_>>();
        let segments = stacked_eighths(&tokens, width)
            .into_iter()
            .zip(&breakdown.categories)
            .map(|(eighths, category)| (eighths, palette.category(category.kind)))
            .collect::<Vec<_>>();
        lines.push(Line::from(merge(bar(
            &segments,
            width,
            (" ", Style::new()),
        ))));
        lines.push(Line::default());
        lines.extend(
            breakdown
                .categories
                .iter()
                .map(|category| self.legend_row(category, total, width)),
        );
        lines.extend(note(
            "Shares are estimates: each measured growth step is split among the items that \
             caused it by size. The total is the server's exact count.",
            width,
            palette.muted().italic(),
        ));
        lines
    }

    fn legend_row(&self, category: &CategoryUsage, total: u64, width: usize) -> Line<'static> {
        const NAME: usize = 12;
        const FIXED: usize = 3 + NAME + 10 + 8 + 12 + 2;
        let palette = self.palette;
        let color = palette.category(category.kind);
        let (style, share_color) = if category.tokens == 0 {
            (palette.muted(), palette.muted)
        } else {
            (palette.value(), color)
        };
        let mut spans = vec![
            Span::styled("██ ", Style::new().fg(color)),
            Span::styled(pad(category_label(category.kind), NAME), style),
            Span::styled(format!("{:>10}", format_count(category.tokens)), style),
            Span::styled(
                format!("{:>8}", format_percent(category.tokens, total)),
                Style::new().fg(share_color),
            ),
            Span::styled(
                format!("{:>12}", count_noun(category.items, "item")),
                palette.muted(),
            ),
        ];
        let bar_width = width.saturating_sub(FIXED);
        if bar_width > 0 && category.tokens > 0 {
            spans.push(Span::raw("  "));
            spans.extend(merge(bar(
                &[(scale_eighths(category.tokens, total, bar_width), color)],
                bar_width,
                (" ", Style::new()),
            )));
        }
        Line::from(spans)
    }

    fn tools(&self, width: usize) -> Vec<Line<'static>> {
        const NUMBERS: usize = 6 + 10 + 11;
        let palette = self.palette;
        let mut lines = vec![self.heading("Tools", None, width)];
        let Some(breakdown) = &self.diagnostics.breakdown else {
            lines.push(Line::styled(UNAVAILABLE, palette.muted()));
            return lines;
        };
        if breakdown.tools.is_empty() {
            lines.push(Line::styled(
                "No tool calls in the active context.",
                palette.muted(),
            ));
            return lines;
        }
        // Names keep up to NAME columns; a bar takes the rest when it can be read.
        const NAME: usize = 18;
        const MIN_BAR: usize = 8;
        let rest = width.saturating_sub(NUMBERS);
        let bar_width = rest.saturating_sub(NAME + 2);
        let (name_width, bar_width) = if bar_width >= MIN_BAR {
            (NAME, bar_width)
        } else {
            (rest, 0)
        };
        let calls_color = palette.category(ContextCategory::ToolCalls);
        let output_color = palette.category(ContextCategory::ToolOutput);
        lines.push(Line::from(vec![
            Span::styled(pad("tool", name_width), palette.muted()),
            Span::styled(format!("{:>6}", "calls"), palette.muted()),
            Span::styled(format!("{:>10}", "call tok"), Style::new().fg(calls_color)),
            Span::styled(
                format!("{:>11}", "output tok"),
                Style::new().fg(output_color),
            ),
        ]));
        let largest = breakdown
            .tools
            .iter()
            .map(|tool| tool.call_tokens.saturating_add(tool.output_tokens))
            .max()
            .unwrap_or_default();
        for tool in &breakdown.tools {
            let ToolUsage {
                name,
                calls,
                call_tokens,
                output_tokens,
            } = tool;
            let name_style = if name == OTHER_TOOLS {
                palette.muted().italic()
            } else {
                palette.value()
            };
            let mut spans = vec![
                Span::styled(pad(&display_name(name, name_width), name_width), name_style),
                Span::styled(format!("{:>6}", format_count(*calls)), palette.muted()),
                Span::styled(
                    format!("{:>10}", format_count(*call_tokens)),
                    palette.value(),
                ),
                Span::styled(
                    format!("{:>11}", format_count(*output_tokens)),
                    palette.value(),
                ),
            ];
            if bar_width > 0 {
                spans.push(Span::raw("  "));
                spans.extend(merge(bar(
                    &[
                        (scale_eighths(*call_tokens, largest, bar_width), calls_color),
                        (
                            scale_eighths(*output_tokens, largest, bar_width),
                            output_color,
                        ),
                    ],
                    bar_width,
                    (" ", Style::new()),
                )));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    fn largest(&self, width: usize) -> Vec<Line<'static>> {
        const KIND: usize = 12;
        const FIXED: usize = 2 + KIND + 9 + 10;
        const SHARE: usize = 8;
        let palette = self.palette;
        let mut lines = vec![self.heading("Largest items", None, width)];
        let Some(breakdown) = &self.diagnostics.breakdown else {
            lines.push(Line::styled(UNAVAILABLE, palette.muted()));
            return lines;
        };
        if breakdown.largest.is_empty() {
            lines.push(Line::styled("No items yet.", palette.muted()));
            return lines;
        }
        let show_share = width >= FIXED + SHARE + 20;
        let tool_width = width.saturating_sub(FIXED + if show_share { SHARE } else { 0 });
        for item in &breakdown.largest {
            let LargestItem {
                kind,
                tool,
                turn,
                tokens,
            } = item;
            let color = palette.category(*kind);
            let tool = match tool {
                Some(tool) => Span::styled(
                    pad(&display_name(tool, tool_width), tool_width),
                    palette.value(),
                ),
                None => Span::styled(pad("—", tool_width), palette.muted()),
            };
            let mut spans = vec![
                Span::styled("█ ", Style::new().fg(color)),
                Span::styled(pad(category_label(*kind), KIND), palette.value()),
                tool,
                Span::styled(format!("{:>9}", format!("turn {turn}")), palette.muted()),
                Span::styled(format!("{:>10}", format_count(*tokens)), palette.value()),
            ];
            if show_share {
                spans.push(Span::styled(
                    format!("{:>8}", format_percent(*tokens, breakdown.input_tokens)),
                    Style::new().fg(color),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    fn growth(&self, width: usize) -> Vec<Line<'static>> {
        let palette = self.palette;
        let history = &self.diagnostics.history;
        let (Some(first), Some(last)) = (history.first(), history.last()) else {
            return vec![
                self.heading("Growth", None, width),
                Line::styled(UNAVAILABLE, palette.muted()),
            ];
        };
        let mut lines = vec![self.heading(
            "Growth",
            Some(format!(
                "input size over the last {}",
                count_noun(history.len() as u64, "call")
            )),
            width,
        )];
        let columns = sparkline_columns(history, width.saturating_sub(SPARKLINE_AXIS).max(1));
        let peak = columns
            .iter()
            .map(|column| column.0)
            .max()
            .unwrap_or_default();
        for row in (0..SPARKLINE_ROWS).rev() {
            let label = match row {
                0 => "0".to_owned(),
                row if row == SPARKLINE_ROWS - 1 => abbreviate(peak),
                _ => String::new(),
            };
            let axis = if label.is_empty() { "│" } else { "┤" };
            let mut spans = vec![
                Span::styled(
                    format!("{label:>width$} ", width = SPARKLINE_AXIS - 2),
                    palette.muted(),
                ),
                Span::styled(axis, Style::new().fg(palette.border)),
            ];
            let cells = columns
                .iter()
                .map(|&(input, _)| {
                    let level = scale_eighths(input, peak, SPARKLINE_ROWS);
                    let fill = level.saturating_sub(row * 8).min(8);
                    (RISING_EIGHTHS[fill], Style::new().fg(palette.accent))
                })
                .collect();
            spans.extend(merge(cells));
            lines.push(Line::from(spans));
        }
        let compacted = columns.iter().any(|column| column.1);
        if compacted {
            let markers = columns
                .iter()
                .map(|&(_, compacted)| {
                    if compacted {
                        ("▲", Style::new().fg(palette.alarm).bold())
                    } else {
                        (" ", Style::new())
                    }
                })
                .collect();
            let mut spans = vec![Span::raw(" ".repeat(SPARKLINE_AXIS))];
            spans.extend(merge(markers));
            lines.push(Line::from(spans));
        }
        let first_label = format!("call {}", first.call);
        let last_label = format!("call {}", last.call);
        let gap = columns
            .len()
            .saturating_sub(first_label.width() + last_label.width())
            .max(1);
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(SPARKLINE_AXIS)),
            Span::styled(first_label, palette.muted()),
            Span::raw(" ".repeat(gap)),
            Span::styled(last_label, palette.muted()),
        ]));
        let peak_point = history
            .iter()
            .max_by_key(|point| point.input)
            .unwrap_or(last);
        let mut facts = vec![
            ("first ", Some(format_count(first.input))),
            (
                "peak ",
                Some(format!(
                    "{} (call {})",
                    format_count(peak_point.input),
                    peak_point.call
                )),
            ),
            ("last ", Some(format_count(last.input))),
        ];
        if compacted {
            facts.push(("▲ ", Some("after compaction".to_owned())));
        }
        lines.extend(self.wrapped_facts(&facts, width));
        lines
    }

    /// Facts joined by separators into rows, breaking between facts that would pass `width`.
    fn wrapped_facts(&self, facts: &[Fact], width: usize) -> Vec<Line<'static>> {
        const SEPARATOR: &str = "  ·  ";
        let palette = self.palette;
        let mut rows = vec![Vec::new()];
        let mut used = 0;
        for (label, value) in facts {
            let (value, style) = match value {
                Some(value) => (value.clone(), palette.value()),
                None => (UNAVAILABLE.to_owned(), palette.muted()),
            };
            let fact_width = label.width() + value.width();
            if used > 0 && used + SEPARATOR.width() + fact_width > width {
                rows.push(Vec::new());
                used = 0;
            }
            let row = rows.last_mut().expect("rows start non-empty");
            if used > 0 {
                row.push(Span::styled(SEPARATOR, Style::new().fg(palette.border)));
                used += SEPARATOR.width();
            }
            row.push(Span::styled(*label, palette.muted()));
            row.push(Span::styled(value, style));
            used += fact_width;
        }
        rows.into_iter().map(Line::from).collect()
    }

    fn cache(&self, width: usize) -> Vec<Line<'static>> {
        let palette = self.palette;
        let mut lines = vec![self.heading("Prompt cache", None, width)];
        let Some(usage) = self.diagnostics.usage else {
            lines.push(Line::styled(UNAVAILABLE, palette.muted()));
            return lines;
        };
        let bar_width = width.saturating_sub(9);
        let cached = scale_eighths(usage.cached_input, usage.input, bar_width);
        let uncached = if usage.input == 0 {
            0
        } else {
            bar_width * 8 - cached
        };
        let mut spans = merge(bar(
            &[(cached, palette.cached), (uncached, palette.uncached)],
            bar_width,
            ("░", Style::new().fg(palette.border)),
        ));
        spans.push(Span::styled(
            format!("{:>9}", format_percent(usage.cached_input, usage.input)),
            Style::new().fg(palette.cached).bold(),
        ));
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![
            Span::styled("█ ", Style::new().fg(palette.cached)),
            Span::styled("cached ", palette.muted()),
            Span::styled(format_count(usage.cached_input), palette.value()),
            Span::raw("   "),
            Span::styled("█ ", Style::new().fg(palette.uncached)),
            Span::styled("uncached ", palette.muted()),
            Span::styled(format_count(usage.uncached_input), palette.value()),
        ]));
        let history = &self.diagnostics.history;
        let (cached, input) = history
            .iter()
            .fold((0_u64, 0_u64), |(cached, input), point| {
                (
                    cached.saturating_add(point.cached),
                    input.saturating_add(point.input),
                )
            });
        let average = (input > 0).then(|| {
            format!(
                "{} over {}",
                format_percent(cached, input),
                count_noun(history.len() as u64, "call")
            )
        });
        lines.extend(self.wrapped_facts(&[("average ", average)], width));
        lines.extend(note(
            "Cached input still counts toward the window.",
            width,
            palette.muted().italic(),
        ));
        lines
    }

    fn session(&self, width: usize) -> Vec<Line<'static>> {
        let diagnostics = self.diagnostics;
        let mut lines = vec![self.heading("Session", None, width)];
        let compaction = diagnostics.last_compaction;
        let last_compaction = match compaction {
            Some(compaction) => Some(format_compaction_time(compaction)),
            None if diagnostics.compactions_started == 0 => Some("none".to_owned()),
            None => None,
        };
        let mut facts = vec![
            (
                "latest input",
                diagnostics.usage.map(|usage| format_count(usage.input)),
            ),
            (
                "latest output",
                diagnostics.usage.map(|usage| {
                    format!(
                        "{} · total {}",
                        format_count(usage.output),
                        format_count(usage.total)
                    )
                }),
            ),
            (
                "continuation",
                diagnostics.continuation.map(|mode| {
                    match mode {
                        ContinuationMode::FullContext => "full context",
                        ContinuationMode::PreviousResponse => "previous response",
                    }
                    .to_owned()
                }),
            ),
            (
                "prompt cache",
                diagnostics
                    .prompt_cache
                    .map(|present| if present { "present" } else { "absent" }.to_owned()),
            ),
            (
                "compactions",
                Some(format!(
                    "{} started · {} completed",
                    diagnostics.compactions_started, diagnostics.compactions_completed
                )),
            ),
            ("last compaction", last_compaction),
        ];
        if let Some(compaction) = compaction {
            facts.push((
                "before → after",
                Some(format!(
                    "{} → {}",
                    optional_count(compaction.before_tokens),
                    optional_count(compaction.after_tokens)
                )),
            ));
        }
        for fact in facts {
            lines.extend(self.labeled(fact, width));
        }
        lines
    }

    /// A labeled value whose text wraps under itself rather than under the label.
    fn labeled(&self, (label, value): Fact, width: usize) -> Vec<Line<'static>> {
        let palette = self.palette;
        let (value, style) = match value {
            Some(value) => (value, palette.value()),
            None => (UNAVAILABLE.to_owned(), palette.muted()),
        };
        let label_width = FACT_LABEL_WIDTH.min(width / 2);
        wrap(&value, width.saturating_sub(label_width).max(1))
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let label = if index == 0 { label } else { "" };
                Line::from(vec![
                    Span::styled(pad(label, label_width), palette.muted()),
                    Span::styled(row, style),
                ])
            })
            .collect()
    }

    /// A section title followed by a rule that fills the row, ending in optional detail.
    fn heading(&self, title: &'static str, detail: Option<String>, width: usize) -> Line<'static> {
        let palette = self.palette;
        let detail_width = detail.as_ref().map_or(0, |detail| detail.width() + 2);
        let rule = width.saturating_sub(title.width() + 1 + detail_width);
        let mut spans = vec![
            Span::styled(title, Style::new().fg(palette.accent).bold()),
            Span::raw(" "),
            Span::styled("─".repeat(rule), Style::new().fg(palette.border)),
        ];
        if let Some(detail) = detail {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(detail, palette.muted()));
        }
        Line::from(spans)
    }
}

/// One terminal cell of a bar: its symbol and style.
type BarCell = (&'static str, Style);

/// A bar of `width` cells holding consecutive colored segments measured in eighths of a cell.
///
/// A cell where one segment ends and the next begins draws a partial block in the first color
/// over a background of the second, so boundaries keep sub-cell precision. Cells no segment
/// reaches show `track`.
fn bar(segments: &[(usize, Color)], width: usize, track: BarCell) -> Vec<BarCell> {
    let mut end = 0;
    let ranges = segments
        .iter()
        .filter(|(eighths, _)| *eighths > 0)
        .map(|&(eighths, color)| {
            end += eighths;
            (end - eighths, end, color)
        })
        .collect::<Vec<_>>();
    (0..width)
        .map(|cell| {
            let (low, high) = (cell * 8, cell * 8 + 8);
            let mut covering = ranges
                .iter()
                .filter(|(start, end, _)| *start < high && *end > low);
            let Some(&(_, end, first)) = covering.next() else {
                return track;
            };
            let covered = end.min(high) - low;
            if covered == 8 {
                return ("█", Style::new().fg(first));
            }
            match covering.next_back() {
                Some(&(_, _, second)) => (LEFT_EIGHTHS[covered], Style::new().fg(first).bg(second)),
                None => (LEFT_EIGHTHS[covered], Style::new().fg(first)),
            }
        })
        .collect()
}

/// Joins runs of equally styled cells into spans.
fn merge(cells: Vec<BarCell>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (symbol, style) in cells {
        match spans.last_mut() {
            Some(span) if span.style == style => span.content.to_mut().push_str(symbol),
            _ => spans.push(Span::styled(symbol.to_owned(), style)),
        }
    }
    spans
}

/// `value / total` of `cells`, rounded down.
fn scale(value: u64, total: u64, cells: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let scaled = u128::from(value.min(total)) * cells as u128 / u128::from(total);
    usize::try_from(scaled).unwrap_or(cells)
}

/// The eighths of `width` cells that `value / total` covers, at least one for any nonzero value.
fn scale_eighths(value: u64, total: u64, width: usize) -> usize {
    scale(value, total, width * 8).max(usize::from(value > 0 && total > 0 && width > 0))
}

/// Splits `width` cells into eighths in proportion to `values`, summing to the whole width.
///
/// Each nonzero value receives at least one whole cell when room allows, so every segment stays
/// visible and no cell has to show more than two segments. The rest is shared in proportion to
/// the remaining values, with leftover eighths going to the largest remainders.
fn stacked_eighths(values: &[u64], width: usize) -> Vec<usize> {
    let units = width * 8;
    let nonzero = values.iter().filter(|value| **value > 0).count();
    if nonzero == 0 {
        return vec![0; values.len()];
    }
    let floor = (units / nonzero).min(8);
    let mut floored = vec![false; values.len()];
    let open = |floored: &[bool]| {
        (0..values.len())
            .filter(|&index| values[index] > 0 && !floored[index])
            .collect::<Vec<_>>()
    };
    let free = |floored: &[bool]| {
        let fixed = floored.iter().filter(|floored| **floored).count();
        let total = open(floored)
            .into_iter()
            .map(|index| u128::from(values[index]))
            .sum::<u128>();
        ((units - floor * fixed) as u128, total)
    };
    loop {
        let (free_units, free_total) = free(&floored);
        let open = open(&floored);
        let small = open
            .iter()
            .copied()
            .filter(|&index| u128::from(values[index]) * free_units < floor as u128 * free_total)
            .collect::<Vec<_>>();
        if small.is_empty() || small.len() == open.len() {
            break;
        }
        for index in small {
            floored[index] = true;
        }
    }
    let (free_units, free_total) = free(&floored);
    let share = |index: usize| u128::from(values[index]) * free_units;
    let mut eighths = (0..values.len())
        .map(|index| match (values[index], floored[index]) {
            (0, _) => 0,
            (_, true) => floor,
            _ => usize::try_from(share(index) / free_total).unwrap_or(units),
        })
        .collect::<Vec<_>>();
    let mut open = open(&floored);
    open.sort_by_key(|&index| Reverse(share(index) % free_total));
    let leftover = units.saturating_sub(eighths.iter().sum());
    for index in open.into_iter().cycle().take(leftover) {
        eighths[index] += 1;
    }
    eighths
}

/// The peak input and whether any compaction preceded it for each of at most `width` columns,
/// grouping consecutive calls when the history is longer than the plot.
fn sparkline_columns(history: &[CallPoint], width: usize) -> Vec<(u64, bool)> {
    let columns = history.len().min(width);
    (0..columns)
        .map(|column| {
            let calls =
                &history[column * history.len() / columns..(column + 1) * history.len() / columns];
            (
                calls
                    .iter()
                    .map(|call| call.input)
                    .max()
                    .unwrap_or_default(),
                calls.iter().any(|call| call.after_compaction),
            )
        })
        .collect()
}

const fn category_label(kind: ContextCategory) -> &'static str {
    match kind {
        ContextCategory::Prefix => "Prefix",
        ContextCategory::User => "User",
        ContextCategory::Assistant => "Assistant",
        ContextCategory::Reasoning => "Reasoning",
        ContextCategory::ToolCalls => "Tool calls",
        ContextCategory::ToolOutput => "Tool output",
        ContextCategory::Compacted => "Compacted",
        ContextCategory::Other => "Other",
    }
}

/// A server-provided name made safe for the terminal and cut to `width` columns.
fn display_name(name: &str, width: usize) -> String {
    let printable = name
        .chars()
        .map(|character| {
            if character.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                character
            }
        })
        .collect::<String>();
    ellipsize(Cow::Owned(printable), width).into_owned()
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}

fn note(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    wrap(text, width)
        .into_iter()
        .map(|row| Line::styled(row, style))
        .collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        if !row.is_empty() && row.width() + 1 + word.width() > width {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

fn optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| UNAVAILABLE.to_owned(), format_count)
}

fn count_noun(count: u64, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{} {noun}{plural}", format_count(count))
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

/// A one-decimal share that never rounds a nonzero part down to zero.
fn format_percent(part: u64, whole: u64) -> String {
    if whole == 0 || part == 0 {
        return "—".to_owned();
    }
    let percent = part as f64 * 100.0 / whole as f64;
    if percent < 0.1 {
        return "<0.1%".to_owned();
    }
    format!("{percent:.1}%")
}

/// A short axis label such as "245k" or "1.2M".
fn abbreviate(value: u64) -> String {
    match value {
        0..1_000 => value.to_string(),
        1_000..1_000_000 => format!("{}k", value / 1_000),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

fn format_compaction_time(compaction: CompactionDiagnostics) -> String {
    let trigger = match compaction.trigger {
        CompactionTrigger::Automatic => "automatic",
        CompactionTrigger::Manual => "manual",
    };
    let timestamp = i64::try_from(compaction.started_at_unix_ms)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map_or_else(
            || "unknown time".to_owned(),
            |time| time.format("%Y-%m-%d %H:%M:%SZ").to_string(),
        );
    let duration = compaction.completed_at_unix_ms.map_or_else(
        || "ongoing".to_owned(),
        |completed| format_duration_millis(completed.saturating_sub(compaction.started_at_unix_ms)),
    );
    format!("{trigger} · {timestamp} · {duration}")
}

fn format_duration_millis(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds}ms");
    }
    format!("{}.{}s", milliseconds / 1_000, milliseconds % 1_000 / 100)
}

#[cfg(test)]
mod tests;
