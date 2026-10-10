use super::{
    Component, ContextDiagnosticsEffect, ContextDiagnosticsEvent, ContextDiagnosticsPanel, Palette,
    format_compaction_time,
};
use crate::{
    app::theme::{Theme, ThemeMode},
    core::context::{
        CallPoint, CategoryUsage, CompactionDiagnostics, CompactionTrigger, ContextBreakdown,
        ContextCategory, ContextDiagnostics, ContinuationMode, LargestItem, TokenUsage, ToolUsage,
    },
};
use crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};
use std::collections::HashSet;

const WINDOW: u64 = 272_000;
const LIMIT: u64 = 244_800;

fn themes() -> [(&'static str, Theme); 2] {
    let mut light = Theme::default();
    light.set_mode(ThemeMode::Light);
    let mut dark = Theme::default();
    dark.set_mode(ThemeMode::Dark);
    [("light", light), ("dark", dark)]
}

fn tool(name: &str, calls: u64, call_tokens: u64, output_tokens: u64) -> ToolUsage {
    ToolUsage {
        name: name.to_owned(),
        calls,
        call_tokens,
        output_tokens,
    }
}

fn item(kind: ContextCategory, tool: Option<&str>, turn: u64, tokens: u64) -> LargestItem {
    LargestItem {
        kind,
        tool: tool.map(str::to_owned),
        turn,
        tokens,
    }
}

/// A long session with one compaction, every category populated, and a tiny "other" share.
fn full() -> ContextDiagnostics {
    let categories = [
        (ContextCategory::Prefix, 9_800, 1),
        (ContextCategory::User, 4_200, 14),
        (ContextCategory::Assistant, 12_640, 31),
        (ContextCategory::Reasoning, 21_300, 29),
        (ContextCategory::ToolCalls, 6_140, 88),
        (ContextCategory::ToolOutput, 115_210, 88),
        (ContextCategory::Compacted, 13_000, 1),
        (ContextCategory::Other, 50, 1),
    ]
    .map(|(kind, tokens, items)| CategoryUsage {
        kind,
        tokens,
        items,
    });
    let mut history = (0..60_u64)
        .map(|index| {
            let input = if index < 30 {
                40_000 + index * 7_000
            } else {
                41_200 + (index - 30) * 4_700
            };
            CallPoint {
                call: 72 + index,
                input,
                cached: input / 10 * 8,
                output: 1_500,
                after_compaction: index == 30,
            }
        })
        .collect::<Vec<_>>();
    history.last_mut().unwrap().input = 182_340;
    let mut diagnostics = ContextDiagnostics::default();
    diagnostics.active_tokens = Some(182_340);
    diagnostics.usage = Some(TokenUsage {
        input: 182_340,
        cached_input: 150_000,
        uncached_input: 32_340,
        output: 2_000,
        total: 184_340,
    });
    diagnostics.continuation = Some(ContinuationMode::PreviousResponse);
    diagnostics.prompt_cache = Some(true);
    diagnostics.compactions_started = 2;
    diagnostics.compactions_completed = 2;
    diagnostics.last_compaction = Some(CompactionDiagnostics {
        trigger: CompactionTrigger::Automatic,
        started_at_unix_ms: 1_791_000_000_000,
        completed_at_unix_ms: Some(1_791_000_039_095),
        before_tokens: Some(250_123),
        after_tokens: Some(41_200),
    });
    diagnostics.breakdown = Some(ContextBreakdown {
        input_tokens: 182_340,
        categories: categories.to_vec(),
        tools: vec![
            tool("shell", 41, 3_100, 71_000),
            tool("read_file", 22, 1_200, 30_110),
            tool("web.run", 5, 200, 8_000),
            tool("apply_patch", 9, 1_500, 4_000),
            tool("(other tools)", 11, 140, 2_100),
        ],
        largest: vec![
            item(ContextCategory::ToolOutput, Some("shell"), 7, 24_301),
            item(ContextCategory::ToolOutput, Some("read_file"), 9, 18_002),
            item(ContextCategory::Compacted, None, 5, 13_000),
            item(ContextCategory::ToolOutput, Some("shell"), 11, 9_870),
            item(ContextCategory::Prefix, None, 1, 9_800),
            item(ContextCategory::Reasoning, None, 11, 4_100),
        ],
    });
    diagnostics.history = history;
    diagnostics
}

/// What a model without per-call usage reports: a native budget and nothing else.
fn minimal() -> ContextDiagnostics {
    let mut diagnostics = ContextDiagnostics::default();
    diagnostics.auto_compact_token_limit = None;
    diagnostics.active_tokens = Some(125_000);
    diagnostics
}

fn draw(panel: &mut ContextDiagnosticsPanel, width: u16, height: u16, theme: &Theme) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| panel.render(frame, frame.area(), theme))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|row| {
            (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect()
        })
        .collect()
}

fn text(buffer: &Buffer) -> String {
    rows(buffer).join("\n")
}

fn find_row(buffer: &Buffer, needle: &str) -> Option<u16> {
    rows(buffer)
        .iter()
        .position(|row| row.contains(needle))
        .and_then(|row| u16::try_from(row).ok())
}

fn press(panel: &mut ContextDiagnosticsPanel, code: KeyCode) -> Vec<ContextDiagnosticsEffect> {
    panel
        .update(ContextDiagnosticsEvent::Terminal(Event::Key(
            KeyEvent::from(code),
        )))
        .effects
}

/// Every report line, read by scrolling a panel from top to bottom.
fn whole_report(diagnostics: ContextDiagnostics, width: u16, height: u16) -> String {
    let theme = Theme::default();
    let mut panel = ContextDiagnosticsPanel::new(diagnostics);
    let mut seen = Vec::new();
    loop {
        let buffer = draw(&mut panel, width, height, &theme);
        seen.push(text(&buffer));
        let before = panel.scroll;
        press(&mut panel, KeyCode::PageDown);
        if panel.scroll == before {
            return seen.join("\n");
        }
    }
}

#[test]
fn full_report_enumerates_every_section_at_full_size() {
    for (_, theme) in themes() {
        let mut panel = ContextDiagnosticsPanel::new(full());
        let rendered = text(&draw(&mut panel, 100, 40, &theme));
        for expected in [
            "Context diagnostics",
            "182,340 / 272,000 tokens",
            "67.0% of the window",
            "auto-compact 244,800 ▼",
            "headroom 89,660",
            "until auto-compact 62,460",
            "182,340 input tokens at the latest call",
            "Tool output    115,210   63.2%    88 items",
            "Other               50   <0.1%      1 item",
            "Shares are estimates",
            "shell",
            "(other tools)",
            "Largest items",
            "↑↓ scroll · r refresh · esc close",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
    }

    let report = whole_report(full(), 100, 40);
    for expected in [
        "input size over the last 60 calls",
        "call 72",
        "call 131",
        "▲",
        "peak 243,000 (call 101)",
        "▲ after compaction",
        "82.3%",
        "cached 150,000",
        "uncached 32,340",
        "average ",
        "Cached input still counts toward the window.",
        "latest input    182,340",
        "latest output   2,000 · total 184,340",
        "previous response",
        "present",
        "2 started · 2 completed",
        "automatic · 2026-10-03",
        "39.0s",
        "250,123 → 41,200",
    ] {
        assert!(report.contains(expected), "missing {expected:?}\n{report}");
    }
}

#[test]
fn minimal_report_explains_missing_breakdown_at_small_size() {
    for (_, theme) in themes() {
        let mut panel = ContextDiagnosticsPanel::new(minimal());
        let buffer = draw(&mut panel, 80, 24, &theme);
        let rendered = text(&buffer);
        for expected in [
            "125,000 / 272,000 tokens",
            "auto-compact limit unavailable",
            "Breakdown unavailable for this model",
            "r refresh · esc close",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
        assert!(!rendered.contains("▼"), "no limit, no marker\n{rendered}");
        assert!(!rendered.contains("0.0%"), "{rendered}");
    }

    let report = whole_report(minimal(), 80, 24);
    for section in [
        "Tools",
        "Largest items",
        "Growth",
        "Prompt cache",
        "Session",
    ] {
        let row = report
            .lines()
            .position(|line| line.contains(&format!(" {section} ─")))
            .unwrap_or_else(|| panic!("missing {section}\n{report}"));
        let next = report.lines().nth(row + 1).unwrap();
        assert!(
            next.contains("unavailable") || section == "Session",
            "{section} should say unavailable\n{report}"
        );
    }
    for expected in ["latest output   unavailable", "last compaction none"] {
        assert!(report.contains(expected), "missing {expected:?}\n{report}");
    }
}

#[test]
fn unknown_active_size_still_shows_the_window_and_limit() {
    let mut panel = ContextDiagnosticsPanel::new(ContextDiagnostics::default());
    let rendered = text(&draw(&mut panel, 100, 40, &Theme::default()));
    assert!(
        rendered.contains("unavailable / 272,000 token window"),
        "{rendered}"
    );
    assert!(rendered.contains("auto-compact at 244,800"), "{rendered}");
}

#[test]
fn keys_scroll_within_the_report_and_keep_refresh_and_close() {
    let theme = Theme::default();
    let mut panel = ContextDiagnosticsPanel::new(full());
    let top = draw(&mut panel, 80, 24, &theme);
    assert!(text(&top).contains("Active context"));
    // The scrollbar thumb replaces the right border and tracks the position.
    assert_eq!(top[(79, 1)].symbol(), "┃");
    assert_eq!(top[(79, 21)].symbol(), "│");

    assert!(press(&mut panel, KeyCode::Down).is_empty());
    let scrolled = draw(&mut panel, 80, 24, &theme);
    assert!(!text(&scrolled).contains("Active context"));
    assert_eq!(panel.scroll, 1);

    press(&mut panel, KeyCode::End);
    let bottom = draw(&mut panel, 80, 24, &theme);
    assert!(
        text(&bottom).contains("before → after"),
        "{}",
        text(&bottom)
    );
    assert_eq!(bottom[(79, 1)].symbol(), "│");
    assert_eq!(bottom[(79, 21)].symbol(), "┃");
    assert_eq!(panel.scroll, panel.max_scroll);
    press(&mut panel, KeyCode::Down);
    assert_eq!(panel.scroll, panel.max_scroll, "scrolling stops at the end");

    press(&mut panel, KeyCode::PageUp);
    assert_eq!(panel.scroll, panel.max_scroll - panel.page);
    press(&mut panel, KeyCode::Home);
    assert_eq!(panel.scroll, 0);

    assert_eq!(
        press(&mut panel, KeyCode::Char('r')),
        [ContextDiagnosticsEffect::Refresh]
    );
    assert_eq!(
        press(&mut panel, KeyCode::Esc),
        [ContextDiagnosticsEffect::Dismiss]
    );
}

#[test]
fn short_report_fits_without_scrolling() {
    let mut diagnostics = minimal();
    diagnostics.breakdown = None;
    let mut panel = ContextDiagnosticsPanel::new(diagnostics);
    let rendered = text(&draw(&mut panel, 120, 60, &Theme::default()));
    assert_eq!(panel.max_scroll, 0);
    assert!(!rendered.contains("scroll"), "{rendered}");
}

#[test]
fn categories_keep_distinct_colors_and_all_appear_in_the_bar() {
    for (name, theme) in themes() {
        let palette = Palette::new(&theme);
        let distinct = palette.categories.iter().collect::<HashSet<_>>();
        assert_eq!(distinct.len(), palette.categories.len(), "{name} theme");
        assert!(!palette.categories.contains(&Color::Reset), "{name} theme");

        for width in [100, 80] {
            let mut panel = ContextDiagnosticsPanel::new(full());
            let buffer = draw(&mut panel, width, 40, &theme);
            let heading = find_row(&buffer, "input tokens at the latest call").unwrap();
            let bar = heading + 1;
            let colors = (0..width)
                .flat_map(|column| {
                    let cell = &buffer[(column, bar)];
                    [cell.fg, cell.bg]
                })
                .collect::<HashSet<_>>();
            for color in palette.categories {
                assert!(colors.contains(&color), "{name} {width}: {color:?} missing");
            }
        }
    }
}

#[test]
fn auto_compact_marker_sits_at_the_limit() {
    let theme = Theme::default();
    let palette = Palette::new(&theme);
    let mut panel = ContextDiagnosticsPanel::new(full());
    let buffer = draw(&mut panel, 100, 40, &theme);
    let marker_row = find_row(&buffer, "▼").unwrap();
    let marker = (0..100)
        .find(|&column| buffer[(column, marker_row)].symbol() == "▼")
        .unwrap();
    // The 100-column panel fills the terminal: one border and one margin column precede the
    // 96-column report.
    let expected = 2 + u16::try_from(LIMIT * 96 / WINDOW).unwrap();
    assert_eq!(marker, expected);
    let tick = &buffer[(marker, marker_row + 1)];
    assert_eq!(tick.symbol(), "│");
    assert_eq!(tick.fg, palette.alarm);
    assert_eq!(buffer[(2, marker_row + 1)].fg, palette.calm);
}

#[test]
fn context_past_the_limit_turns_the_meter_to_alarm() {
    let theme = Theme::default();
    let palette = Palette::new(&theme);
    let mut diagnostics = full();
    diagnostics.active_tokens = Some(250_000);
    let mut panel = ContextDiagnosticsPanel::new(diagnostics);
    let buffer = draw(&mut panel, 100, 40, &theme);
    let rendered = text(&buffer);
    assert!(
        rendered.contains("past the auto-compact limit by 5,200"),
        "{rendered}"
    );
    let bar = find_row(&buffer, "▼").unwrap() + 1;
    assert_eq!(buffer[(2, bar)].fg, palette.alarm);
}

#[test]
fn tool_names_cannot_inject_terminal_control_sequences() {
    let mut diagnostics = full();
    diagnostics.breakdown.as_mut().unwrap().tools[0].name = "sh\u{1b}]52;c;x\u{7}ell".to_owned();
    let mut panel = ContextDiagnosticsPanel::new(diagnostics);
    let buffer = draw(&mut panel, 100, 40, &Theme::default());
    let rendered = text(&buffer);
    assert!(
        rendered.contains("sh\u{FFFD}]52;c;x\u{FFFD}ell"),
        "{rendered}"
    );
    assert!(!rendered.contains('\u{1b}'));
}

#[test]
fn compaction_time_is_readable_and_includes_duration() {
    let rendered = format_compaction_time(CompactionDiagnostics {
        trigger: CompactionTrigger::Automatic,
        started_at_unix_ms: 0,
        completed_at_unix_ms: Some(39_095),
        before_tokens: None,
        after_tokens: None,
    });

    assert_eq!(rendered, "automatic · 1970-01-01 00:00:00Z · 39.0s");
}

/// Prints the report as text for eyeballing:
/// `cargo nextest run -p tact --run-ignored only print_reports --no-capture`.
#[test]
#[ignore = "prints rendered panels for manual inspection"]
fn print_reports() {
    for (diagnostics, width, height) in [(full(), 100, 40), (full(), 80, 24), (minimal(), 80, 24)] {
        println!(
            "{width}x{height}\n{}\n",
            whole_report(diagnostics, width, height)
        );
    }
}
