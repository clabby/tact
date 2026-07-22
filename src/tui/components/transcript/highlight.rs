//! Shared, cached syntax highlighting for transcript code.

use crate::tui::theme::Theme;
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use std::{str::FromStr, sync::OnceLock};
use syntect::{
    easy::HighlightLines,
    highlighting::{
        Color as SyntectColor, FontStyle, ScopeSelectors, StyleModifier, Theme as SyntaxTheme,
        ThemeItem, ThemeSettings,
    },
    parsing::{SyntaxReference, SyntaxSet},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) struct Assets {
    pub(super) syntaxes: SyntaxSet,
}

pub(super) fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
    })
}

pub(super) fn theme(theme: &Theme) -> SyntaxTheme {
    SyntaxTheme {
        name: Some("tact".to_owned()),
        settings: ThemeSettings {
            foreground: Some(syntect_color(theme.code_text())),
            accent: Some(syntect_color(theme.accent())),
            ..ThemeSettings::default()
        },
        scopes: vec![
            rule("comment", theme.muted(), Some(FontStyle::ITALIC)),
            rule("string", theme.thinking_medium(), None),
            rule(
                "constant.numeric, constant.language, constant.character",
                theme.thinking_xhigh(),
                None,
            ),
            rule("keyword, storage", theme.accent(), Some(FontStyle::BOLD)),
            rule(
                "entity.name.function, support.function",
                theme.thinking_high(),
                None,
            ),
            rule(
                "entity.name.type, entity.name.class, support.type, storage.type",
                theme.thinking_max(),
                None,
            ),
            rule(
                "variable.parameter",
                theme.thinking_medium(),
                Some(FontStyle::ITALIC),
            ),
            rule(
                "invalid",
                theme.thinking_xhigh(),
                Some(FontStyle::UNDERLINE),
            ),
        ],
        ..SyntaxTheme::default()
    }
}

pub(super) fn syntax_for_token<'a>(syntaxes: &'a SyntaxSet, token: &str) -> &'a SyntaxReference {
    let token = token.split_ascii_whitespace().next().unwrap_or_default();
    syntaxes
        .find_syntax_by_token(token)
        .or_else(|| syntaxes.find_syntax_by_extension(token))
        .or_else(|| syntaxes.find_syntax_by_name(token))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
}

pub(super) fn line(
    highlighter: &mut HighlightLines<'_>,
    text: &str,
    syntaxes: &SyntaxSet,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let line = format!("{text}\n");
    let Ok(regions) = highlighter.highlight_line(&line, syntaxes) else {
        return vec![Span::styled(text.to_owned(), code_style(theme))];
    };
    let mut spans = Vec::new();
    for (style, region) in regions {
        let region = region.trim_end_matches('\n');
        if region.is_empty() {
            continue;
        }
        let mut modifier = Modifier::empty();
        if style.font_style.contains(FontStyle::BOLD) {
            modifier.insert(Modifier::BOLD);
        }
        if style.font_style.contains(FontStyle::ITALIC) {
            modifier.insert(Modifier::ITALIC);
        }
        if style.font_style.contains(FontStyle::UNDERLINE) {
            modifier.insert(Modifier::UNDERLINED);
        }
        spans.push(Span::styled(
            region.to_owned(),
            Style::default()
                .fg(Color::Rgb(
                    style.foreground.r,
                    style.foreground.g,
                    style.foreground.b,
                ))
                .add_modifier(modifier),
        ));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), code_style(theme)));
    }
    spans
}

pub(super) fn code_style(theme: &Theme) -> Style {
    Style::default().fg(theme.code_text())
}

pub(super) fn wrap(spans: Vec<Span<'static>>, width: u16) -> Vec<Vec<Span<'static>>> {
    let mut lines = vec![Vec::<Span<'static>>::new()];
    let mut used = 0_u16;
    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width =
                u16::try_from(UnicodeWidthStr::width(grapheme)).unwrap_or(u16::MAX);
            if grapheme_width > width {
                if used > 0 {
                    lines.push(Vec::new());
                }
                push_grapheme(
                    lines.last_mut().expect("a line always exists"),
                    "�",
                    span.style,
                );
                used = 1;
                continue;
            }
            if used.saturating_add(grapheme_width) > width && used > 0 {
                lines.push(Vec::new());
                used = 0;
            }
            push_grapheme(
                lines.last_mut().expect("a line always exists"),
                grapheme,
                span.style,
            );
            used = used.saturating_add(grapheme_width);
        }
    }
    lines
}

fn push_grapheme(spans: &mut Vec<Span<'static>>, grapheme: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(grapheme);
        return;
    }
    spans.push(Span::styled(grapheme.to_owned(), style));
}

fn rule(scope: &str, color: Color, font_style: Option<FontStyle>) -> ThemeItem {
    ThemeItem {
        scope: ScopeSelectors::from_str(scope).expect("built-in syntax scopes should be valid"),
        style: StyleModifier {
            foreground: Some(syntect_color(color)),
            background: None,
            font_style,
        },
    }
}

fn syntect_color(color: Color) -> SyntectColor {
    let (r, g, b) = terminal_rgb(color);
    SyntectColor { r, g, b, a: 0xff }
}

fn terminal_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Reset => (0xd7, 0xd7, 0xd7),
        Color::Black => (0x00, 0x00, 0x00),
        Color::Red => (0xcd, 0x31, 0x31),
        Color::Green => (0x0d, 0xbc, 0x79),
        Color::Yellow => (0xe5, 0xe5, 0x10),
        Color::Blue => (0x24, 0x72, 0xc8),
        Color::Magenta => (0xbc, 0x3f, 0xbc),
        Color::Cyan => (0x11, 0xa8, 0xcd),
        Color::Gray => (0xe5, 0xe5, 0xe5),
        Color::DarkGray => (0x66, 0x66, 0x66),
        Color::LightRed => (0xf1, 0x4c, 0x4c),
        Color::LightGreen => (0x23, 0xd1, 0x8b),
        Color::LightYellow => (0xf5, 0xf5, 0x43),
        Color::LightBlue => (0x3b, 0x8e, 0xd0),
        Color::LightMagenta => (0xd6, 0x70, 0xd6),
        Color::LightCyan => (0x29, 0xb8, 0xdb),
        Color::White => (0xff, 0xff, 0xff),
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(index) => indexed_rgb(index),
    }
}

fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    const ANSI: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0x80, 0x00, 0x00),
        (0x00, 0x80, 0x00),
        (0x80, 0x80, 0x00),
        (0x00, 0x00, 0x80),
        (0x80, 0x00, 0x80),
        (0x00, 0x80, 0x80),
        (0xc0, 0xc0, 0xc0),
        (0x80, 0x80, 0x80),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x00, 0x00, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    if index < 16 {
        return ANSI[usize::from(index)];
    }
    if index >= 232 {
        let gray = 8_u8.saturating_add(index.saturating_sub(232).saturating_mul(10));
        return (gray, gray, gray);
    }
    let cube = index - 16;
    let level = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
    (level(cube / 36), level((cube % 36) / 6), level(cube % 6))
}
