use crate::tui::theme::Theme;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::sync::Arc;
use syntect::easy::HighlightLines;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) struct Layout {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) links: Vec<Vec<LinkSpan>>,
}

#[derive(Clone)]
pub(super) struct LinkSpan {
    pub(super) destination: Arc<str>,
    pub(super) start: u16,
    pub(super) end: u16,
}

pub(super) fn render(markdown: &str, width: u16, theme: &Theme) -> Layout {
    if width == 0 {
        return Layout {
            lines: Vec::new(),
            links: Vec::new(),
        };
    }
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM;
    let mut renderer = Renderer::new(width, theme);
    let mut events = Parser::new_ext(markdown, options).peekable();
    while let Some(event) = events.next() {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                renderer.flush();
                renderer.code_block(kind, &mut events);
            }
            Event::Start(Tag::Table(_)) => {
                renderer.flush();
                renderer.table(&mut events);
            }
            event => renderer.event(event),
        }
    }
    renderer.finish()
}

pub(super) fn wrap_plain(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    let logical = sanitize(text);
    let mut lines = Vec::new();
    for line in logical.split('\n') {
        let spans = vec![Span::styled(line.to_owned(), style)];
        lines.extend(wrap_spans(&spans, width, true));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

pub(super) fn sanitize(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => sanitized.push('\n'),
            '\t' => sanitized.push_str("    "),
            character if character.is_control() => sanitized.push('�'),
            character => sanitized.push(character),
        }
    }
    sanitized
}

struct Renderer<'a> {
    width: u16,
    theme: &'a Theme,
    lines: Vec<Line<'static>>,
    current: Vec<TaggedSpan>,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    quote_depth: usize,
    links: Vec<LinkState>,
    rendered_links: Vec<(usize, LinkSpan)>,
    image: Option<ImageState>,
}

struct ListState {
    next: Option<u64>,
}

struct LinkState {
    destination: Arc<str>,
    label: String,
}

struct TaggedSpan {
    span: Span<'static>,
    link: Option<Arc<str>>,
}

struct ImageState {
    destination: String,
    alt: String,
}

impl<'a> Renderer<'a> {
    fn new(width: u16, theme: &'a Theme) -> Self {
        Self {
            width,
            theme,
            lines: Vec::new(),
            current: Vec::new(),
            styles: vec![Style::default().fg(theme.text())],
            lists: Vec::new(),
            quote_depth: 0,
            links: Vec::new(),
            rendered_links: Vec::new(),
            image: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.span(
                &code,
                Style::default()
                    .fg(self.theme.code_text())
                    .bg(self.theme.code_background()),
            ),
            Event::InlineMath(math) => self.span(
                &format!("${}$", sanitize(&math)),
                Style::default().fg(self.theme.thinking_high()),
            ),
            Event::DisplayMath(math) => {
                self.flush();
                self.span(
                    &format!("  $${}$$", sanitize(&math)),
                    Style::default().fg(self.theme.thinking_high()),
                );
                self.flush();
                self.blank();
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.span(&html, Style::default().fg(self.theme.muted()));
            }
            Event::FootnoteReference(reference) => self.span(
                &format!("[{}]", sanitize(&reference)),
                Style::default().fg(self.theme.thinking_medium()),
            ),
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(usize::from(self.width)),
                    Style::default().fg(self.theme.muted()),
                )));
                self.blank();
            }
            Event::TaskListMarker(checked) => self.span(
                if checked { "✓ " } else { "□ " },
                Style::default().fg(if checked {
                    self.theme.thinking_medium()
                } else {
                    self.theme.muted()
                }),
            ),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.ensure_prefix(),
            Tag::Heading { level, .. } => {
                self.flush();
                let style = heading_style(level, self.theme);
                self.push_unlinked(Span::styled("▍ ", style));
                self.push_style(style);
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_add(1);
                self.push_style(Style::default().fg(self.theme.muted()));
            }
            Tag::List(start) => self.lists.push(ListState { next: start }),
            Tag::Item => {
                self.flush();
                self.ensure_quote_prefix();
                let depth = self.lists.len().saturating_sub(1);
                self.push_unlinked(Span::raw("  ".repeat(depth)));
                let marker = self.lists.last_mut().map_or_else(
                    || "• ".to_owned(),
                    |list| match &mut list.next {
                        Some(next) => {
                            let marker = format!("{next}. ");
                            *next = next.saturating_add(1);
                            marker
                        }
                        None => "• ".to_owned(),
                    },
                );
                self.push_unlinked(Span::styled(
                    marker,
                    Style::default().fg(self.theme.accent()),
                ));
            }
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(
                Style::default()
                    .fg(self.theme.muted())
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
            Tag::Superscript => self.text("^"),
            Tag::Subscript => self.text("~"),
            Tag::Link { dest_url, .. } => {
                self.links.push(LinkState {
                    destination: Arc::from(sanitize(&dest_url)),
                    label: String::new(),
                });
                self.push_style(
                    Style::default()
                        .fg(self.theme.accent())
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some(ImageState {
                    destination: sanitize(&dest_url),
                    alt: String::new(),
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.flush();
                self.push_unlinked(Span::styled(
                    format!("[{}] ", sanitize(&label)),
                    Style::default().fg(self.theme.thinking_medium()),
                ));
            }
            Tag::DefinitionListTitle => {
                self.flush();
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::DefinitionListDefinition => {
                self.flush();
                self.push_unlinked(Span::styled(
                    "  : ",
                    Style::default().fg(self.theme.muted()),
                ));
            }
            Tag::HtmlBlock | Tag::MetadataBlock(_) => {
                self.flush();
                self.push_style(Style::default().fg(self.theme.muted()));
            }
            Tag::CodeBlock(_)
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::DefinitionList => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.blank();
            }
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.pop_style();
                self.blank();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Superscript => self.text("^"),
            TagEnd::Subscript => self.text("~"),
            TagEnd::Link => {
                self.pop_style();
                if let Some(link) = self.links.pop()
                    && link.label.trim() != link.destination.as_ref()
                {
                    self.push_linked(
                        Span::styled(
                            format!(" ↗ {}", link.destination),
                            Style::default()
                                .fg(self.theme.accent())
                                .add_modifier(Modifier::DIM),
                        ),
                        link.destination,
                    );
                }
            }
            TagEnd::Image => {
                if let Some(image) = self.image.take() {
                    self.push_unlinked(Span::styled(
                        format!("▧ {} · ", image.alt.trim()),
                        Style::default().fg(self.theme.muted()),
                    ));
                    let destination = Arc::<str>::from(image.destination);
                    self.push_linked(
                        Span::styled(
                            destination.to_string(),
                            Style::default()
                                .fg(self.theme.accent())
                                .add_modifier(Modifier::UNDERLINED),
                        ),
                        destination,
                    );
                }
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_) => {
                self.pop_style();
                self.flush();
                self.blank();
            }
            TagEnd::CodeBlock
            | TagEnd::DefinitionList
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell => {}
        }
    }

    fn text(&mut self, text: &str) {
        let text = sanitize(text);
        if let Some(image) = &mut self.image {
            image.alt.push_str(&text);
            return;
        }
        if let Some(link) = self.links.last_mut() {
            link.label.push_str(&text);
        }
        self.ensure_prefix();
        self.push_current(Span::styled(text, self.style()));
    }

    fn span(&mut self, text: &str, style: Style) {
        let text = sanitize(text);
        if let Some(image) = &mut self.image {
            image.alt.push_str(&text);
            return;
        }
        self.ensure_prefix();
        self.push_current(Span::styled(text, self.style().patch(style)));
    }

    fn code_block(
        &mut self,
        kind: CodeBlockKind<'_>,
        events: &mut std::iter::Peekable<Parser<'_>>,
    ) {
        let language = match kind {
            CodeBlockKind::Indented => None,
            CodeBlockKind::Fenced(language) if language.is_empty() => None,
            CodeBlockKind::Fenced(language) => Some(sanitize(&language)),
        };
        let mut code = String::new();
        for event in events.by_ref() {
            match event {
                Event::End(TagEnd::CodeBlock) => break,
                Event::Text(text) | Event::Code(text) => code.push_str(&sanitize(&text)),
                Event::SoftBreak | Event::HardBreak => code.push('\n'),
                _ => {}
            }
        }
        let is_diff = language.as_deref().is_some_and(is_diff_language);
        if is_diff {
            self.lines
                .extend(super::diff::render(&code, self.width, self.theme));
            self.blank();
            return;
        }
        if self.width < 6 {
            self.narrow_code_block(&code, language.as_deref());
            self.blank();
            return;
        }

        let border = Style::default().fg(self.theme.border());
        let assets = super::highlight::assets();
        let syntax = language.as_deref().map_or_else(
            || assets.syntaxes.find_syntax_plain_text(),
            |language| super::highlight::syntax_for_token(&assets.syntaxes, language),
        );
        let syntax_theme = super::highlight::theme(self.theme);
        let mut highlighter = HighlightLines::new(syntax, &syntax_theme);
        self.lines.push(code_block_header(
            language.as_deref(),
            self.width,
            self.theme,
        ));
        let content_width = self.width.saturating_sub(4).max(1);
        for source_line in code.trim_end_matches('\n').split('\n') {
            let highlighted =
                super::highlight::line(&mut highlighter, source_line, &assets.syntaxes, self.theme);
            for (index, mut spans) in
                super::highlight::wrap(highlighted, content_width.saturating_sub(2).max(1))
                    .into_iter()
                    .enumerate()
            {
                if index > 0 {
                    spans.insert(
                        0,
                        Span::styled("↪ ", Style::default().fg(self.theme.muted())),
                    );
                }
                let used = spans.iter().map(Span::width).sum::<usize>();
                let padding = usize::from(content_width).saturating_sub(used);
                let mut body = Vec::with_capacity(spans.len() + 3);
                body.push(Span::styled("│ ", border));
                body.extend(spans);
                body.push(Span::raw(" ".repeat(padding)));
                body.push(Span::styled(" │", border));
                self.lines.push(Line::from(body));
            }
        }
        self.lines.push(Line::from(Span::styled(
            format!(
                "╰{}╯",
                "─".repeat(usize::from(self.width.saturating_sub(2)))
            ),
            border,
        )));
        self.blank();
    }

    fn narrow_code_block(&mut self, code: &str, language: Option<&str>) {
        let gutter = Style::default().fg(self.theme.border());
        let assets = super::highlight::assets();
        let syntax = language.map_or_else(
            || assets.syntaxes.find_syntax_plain_text(),
            |language| super::highlight::syntax_for_token(&assets.syntaxes, language),
        );
        let syntax_theme = super::highlight::theme(self.theme);
        let mut highlighter = HighlightLines::new(syntax, &syntax_theme);
        let content_width = self.width.saturating_sub(2).max(1);
        for source_line in code.trim_end_matches('\n').split('\n') {
            let highlighted =
                super::highlight::line(&mut highlighter, source_line, &assets.syntaxes, self.theme);
            for spans in super::highlight::wrap(highlighted, content_width) {
                let mut line = vec![Span::styled("┃ ", gutter)];
                line.extend(spans);
                self.lines.push(Line::from(line));
            }
        }
    }

    fn table(&mut self, events: &mut std::iter::Peekable<Parser<'_>>) {
        let mut rows = Vec::<Vec<String>>::new();
        let mut row = Vec::<String>::new();
        let mut cell = String::new();
        let mut in_cell = false;
        let mut header_rows = 0_usize;
        let mut in_header = false;
        for event in events.by_ref() {
            match event {
                Event::Start(Tag::TableHead) => in_header = true,
                Event::End(TagEnd::TableHead) => {
                    if !row.is_empty() {
                        rows.push(std::mem::take(&mut row));
                        header_rows = header_rows.saturating_add(1);
                    }
                    in_header = false;
                }
                Event::Start(Tag::TableCell) => {
                    cell.clear();
                    in_cell = true;
                }
                Event::End(TagEnd::TableCell) => {
                    row.push(cell.trim().to_owned());
                    in_cell = false;
                }
                Event::End(TagEnd::TableRow) => {
                    if in_header {
                        header_rows = header_rows.saturating_add(1);
                    }
                    rows.push(std::mem::take(&mut row));
                }
                Event::End(TagEnd::Table) => break,
                Event::Text(text) | Event::Code(text) if in_cell => {
                    cell.push_str(&sanitize(&text));
                }
                Event::SoftBreak | Event::HardBreak if in_cell => cell.push(' '),
                _ => {}
            }
        }
        self.lines
            .extend(render_table(&rows, header_rows, self.width, self.theme));
        self.blank();
    }

    fn ensure_prefix(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        self.ensure_quote_prefix();
    }

    fn ensure_quote_prefix(&mut self) {
        for _ in 0..self.quote_depth {
            self.push_unlinked(Span::styled("▌ ", Style::default().fg(self.theme.accent())));
        }
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(self.style().patch(style));
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_current(&mut self, span: Span<'static>) {
        let link = self.links.last().map(|link| Arc::clone(&link.destination));
        self.current.push(TaggedSpan { span, link });
    }

    fn push_unlinked(&mut self, span: Span<'static>) {
        self.current.push(TaggedSpan { span, link: None });
    }

    fn push_linked(&mut self, span: Span<'static>, destination: Arc<str>) {
        self.current.push(TaggedSpan {
            span,
            link: Some(destination),
        });
    }

    fn flush(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let first_line = self.lines.len();
        for (offset, (line, links)) in wrap_tagged_spans(&self.current, self.width, true)
            .into_iter()
            .enumerate()
        {
            self.lines.push(line);
            self.rendered_links.extend(
                links
                    .into_iter()
                    .map(|link| (first_line.saturating_add(offset), link)),
            );
        }
        self.current.clear();
    }

    fn blank(&mut self) {
        if self.lines.last().is_some_and(|line| line.width() == 0) {
            return;
        }
        self.lines.push(Line::default());
    }

    fn finish(mut self) -> Layout {
        self.flush();
        while self.lines.last().is_some_and(|line| line.width() == 0) {
            self.lines.pop();
        }
        let mut links = vec![Vec::new(); self.lines.len()];
        for (line, link) in self.rendered_links {
            if let Some(line_links) = links.get_mut(line) {
                line_links.push(link);
            }
        }
        Layout {
            lines: self.lines,
            links,
        }
    }
}

fn is_diff_language(language: &str) -> bool {
    let language = language.split_ascii_whitespace().next().unwrap_or_default();
    language.eq_ignore_ascii_case("diff") || language.eq_ignore_ascii_case("patch")
}

fn heading_style(level: HeadingLevel, theme: &Theme) -> Style {
    let color = match level {
        HeadingLevel::H1 => theme.thinking_max(),
        HeadingLevel::H2 => theme.accent(),
        HeadingLevel::H3 => theme.thinking_medium(),
        HeadingLevel::H4 => theme.thinking_high(),
        HeadingLevel::H5 => theme.thinking_xhigh(),
        HeadingLevel::H6 => theme.muted(),
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub(super) fn wrap_spans(
    spans: &[Span<'static>],
    width: u16,
    prefer_words: bool,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let graphemes = spans
        .iter()
        .flat_map(|span| {
            span.content
                .graphemes(true)
                .map(|text| StyledGrapheme {
                    text: text.to_owned(),
                    style: span.style,
                    link: None,
                    width: u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX),
                    whitespace: text.chars().all(char::is_whitespace),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    wrap_graphemes(&graphemes, width, prefer_words)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

fn wrap_tagged_spans(
    spans: &[TaggedSpan],
    width: u16,
    prefer_words: bool,
) -> Vec<(Line<'static>, Vec<LinkSpan>)> {
    if width == 0 {
        return Vec::new();
    }
    let graphemes = spans
        .iter()
        .flat_map(|tagged| {
            tagged
                .span
                .content
                .graphemes(true)
                .map(|text| StyledGrapheme {
                    text: text.to_owned(),
                    style: tagged.span.style,
                    link: tagged.link.as_ref().map(Arc::clone),
                    width: u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX),
                    whitespace: text.chars().all(char::is_whitespace),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    wrap_graphemes(&graphemes, width, prefer_words)
}

fn wrap_graphemes(
    graphemes: &[StyledGrapheme],
    width: u16,
    prefer_words: bool,
) -> Vec<(Line<'static>, Vec<LinkSpan>)> {
    if graphemes.is_empty() {
        return vec![(Line::default(), Vec::new())];
    }

    let mut lines = Vec::new();
    let mut start = 0_usize;
    while start < graphemes.len() {
        while start < graphemes.len() && graphemes[start].whitespace {
            start += 1;
        }
        if start == graphemes.len() {
            break;
        }
        let mut end = start;
        let mut used = 0_u16;
        let mut word_break = None;
        while end < graphemes.len() {
            let next = used.saturating_add(graphemes[end].width);
            if next > width && end > start {
                break;
            }
            if graphemes[end].whitespace {
                word_break = Some(end);
            }
            used = next;
            end += 1;
            if used >= width {
                break;
            }
        }
        let split = if prefer_words && end < graphemes.len() {
            word_break.filter(|&index| index > start).unwrap_or(end)
        } else {
            end
        };
        lines.push(graphemes_to_line(&graphemes[start..split]));
        start = split.max(start + 1);
    }
    lines
}

struct StyledGrapheme {
    text: String,
    style: Style,
    link: Option<Arc<str>>,
    width: u16,
    whitespace: bool,
}

fn graphemes_to_line(graphemes: &[StyledGrapheme]) -> (Line<'static>, Vec<LinkSpan>) {
    let mut spans = Vec::<Span<'static>>::new();
    let mut links = Vec::<LinkSpan>::new();
    let mut column = 0_u16;
    let rendered_len = graphemes
        .iter()
        .rposition(|grapheme| !grapheme.whitespace)
        .map_or(0, |index| index.saturating_add(1));
    for grapheme in &graphemes[..rendered_len] {
        if let Some(last) = spans.last_mut()
            && last.style == grapheme.style
        {
            last.content.to_mut().push_str(&grapheme.text);
        } else {
            spans.push(Span::styled(grapheme.text.clone(), grapheme.style));
        }
        if let Some(destination) = &grapheme.link
            && grapheme.width > 0
        {
            let end = column.saturating_add(grapheme.width);
            if let Some(last) = links.last_mut()
                && last.destination == *destination
                && last.end == column
            {
                last.end = end;
            } else {
                links.push(LinkSpan {
                    destination: Arc::clone(destination),
                    start: column,
                    end,
                });
            }
        }
        column = column.saturating_add(grapheme.width);
    }
    (Line::from(spans), links)
}

fn hard_wrap(text: &str, width: u16) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0_u16;
    for grapheme in text.graphemes(true) {
        let grapheme_width = u16::try_from(UnicodeWidthStr::width(grapheme)).unwrap_or(u16::MAX);
        if used.saturating_add(grapheme_width) > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    lines.push(line);
    lines
}

fn code_block_header(language: Option<&str>, width: u16, theme: &Theme) -> Line<'static> {
    let border = Style::default().fg(theme.border());
    let Some(language) = language else {
        return Line::from(Span::styled(
            format!("╭{}╮", "─".repeat(usize::from(width.saturating_sub(2)))),
            border,
        ));
    };
    let available = width.saturating_sub(5);
    let language = truncate_graphemes(language, available);
    let language_width =
        u16::try_from(UnicodeWidthStr::width(language.as_str())).unwrap_or(u16::MAX);
    let fill = width.saturating_sub(language_width.saturating_add(5));
    Line::from(vec![
        Span::styled("╭─ ", border),
        Span::styled(
            language,
            Style::default()
                .fg(theme.accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {}╮", "─".repeat(usize::from(fill))), border),
    ])
}

fn truncate_graphemes(text: &str, width: u16) -> String {
    let mut rendered = String::new();
    let mut used = 0_u16;
    for grapheme in text.graphemes(true) {
        let grapheme_width = u16::try_from(UnicodeWidthStr::width(grapheme)).unwrap_or(u16::MAX);
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        rendered.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    rendered
}

fn render_table(
    rows: &[Vec<String>],
    header_rows: usize,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 || width < 4 {
        return Vec::new();
    }
    let border_cells = u16::try_from(columns.saturating_add(1)).unwrap_or(u16::MAX);
    let available = width.saturating_sub(border_cells);
    if available < u16::try_from(columns.saturating_mul(3)).unwrap_or(u16::MAX) {
        return render_stacked_table(rows, header_rows, width, theme);
    }
    let mut widths = vec![3_u16; columns];
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column]
                .max(u16::try_from(UnicodeWidthStr::width(cell.as_str())).unwrap_or(u16::MAX));
        }
    }
    while widths.iter().copied().sum::<u16>() > available {
        let Some((index, _)) = widths.iter().enumerate().max_by_key(|(_, width)| **width) else {
            break;
        };
        if widths[index] <= 3 {
            break;
        }
        widths[index] -= 1;
    }

    let mut lines = Vec::new();
    lines.push(table_rule('╭', '┬', '╮', &widths, theme));
    for (row_index, row) in rows.iter().enumerate() {
        let wrapped = widths
            .iter()
            .enumerate()
            .map(|(column, &cell_width)| {
                hard_wrap(row.get(column).map_or("", String::as_str), cell_width)
            })
            .collect::<Vec<_>>();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            let mut spans = vec![Span::styled("│", Style::default().fg(theme.border()))];
            for (column, &cell_width) in widths.iter().enumerate() {
                let text = wrapped[column].get(line_index).map_or("", String::as_str);
                let padding = usize::from(cell_width).saturating_sub(UnicodeWidthStr::width(text));
                let style = if row_index < header_rows {
                    Style::default()
                        .fg(theme.accent())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text())
                };
                spans.push(Span::styled(
                    format!("{text}{}", " ".repeat(padding)),
                    style,
                ));
                spans.push(Span::styled("│", Style::default().fg(theme.border())));
            }
            lines.push(Line::from(spans));
        }
        if row_index + 1 < rows.len() {
            lines.push(table_rule('├', '┼', '┤', &widths, theme));
        }
    }
    lines.push(table_rule('╰', '┴', '╯', &widths, theme));
    lines
}

fn table_rule(
    left: char,
    middle: char,
    right: char,
    widths: &[u16],
    theme: &Theme,
) -> Line<'static> {
    let mut rule = left.to_string();
    for (index, width) in widths.iter().enumerate() {
        rule.push_str(&"─".repeat(usize::from(*width)));
        rule.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    Line::from(Span::styled(rule, Style::default().fg(theme.border())))
}

fn render_stacked_table(
    rows: &[Vec<String>],
    header_rows: usize,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let Some(headers) = rows.first() else {
        return Vec::new();
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(usize::from(width.saturating_sub(2)))),
        Style::default().fg(theme.border()),
    ))];
    for row in rows.iter().skip(header_rows.max(1)) {
        for (column, value) in row.iter().enumerate() {
            let header = headers.get(column).map_or("Value", String::as_str);
            let content = format!("{header}: {value}");
            for line in hard_wrap(&content, width.saturating_sub(4).max(1)) {
                let padding = usize::from(width.saturating_sub(4))
                    .saturating_sub(UnicodeWidthStr::width(line.as_str()));
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.border())),
                    Span::styled(line, Style::default().fg(theme.text())),
                    Span::raw(" ".repeat(padding)),
                    Span::styled(" │", Style::default().fg(theme.border())),
                ]));
            }
        }
        if row.as_ptr() != rows.last().map_or(row.as_ptr(), Vec::as_ptr) {
            lines.push(Line::from(Span::styled(
                format!("├{}┤", "─".repeat(usize::from(width.saturating_sub(2)))),
                Style::default().fg(theme.border()),
            )));
        }
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(usize::from(width.saturating_sub(2)))),
        Style::default().fg(theme.border()),
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::tui::theme::Theme;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn requested_markdown_styles_are_applied() {
        let theme = Theme::default();
        let lines = render(
            "# Header\n\n`code` and [link](https://example.com)",
            80,
            &theme,
        )
        .lines;

        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Magenta));
        let code = lines[2]
            .spans
            .iter()
            .find(|span| span.content.contains("code"))
            .unwrap();
        assert_eq!(code.style.fg, Some(Color::Rgb(0xD7, 0xD7, 0xD7)));
        assert_eq!(code.style.bg, Some(Color::Rgb(0x26, 0x26, 0x26)));
        let link = lines[2]
            .spans
            .iter()
            .find(|span| span.content == "link")
            .unwrap();
        assert_eq!(link.style.fg, Some(Color::Blue));
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn wrapped_links_retain_clickable_ranges() {
        let layout = render("[abcdefghij](https://example.com)", 5, &Theme::default());

        assert_eq!(layout.lines[0].to_string(), "abcde");
        assert_eq!(layout.lines[1].to_string(), "fghij");
        assert_eq!(layout.links[0].len(), 1);
        assert_eq!(
            layout.links[0][0].destination.as_ref(),
            "https://example.com"
        );
        assert_eq!((layout.links[0][0].start, layout.links[0][0].end), (0, 5));
        assert_eq!(layout.links[1].len(), 1);
        assert_eq!((layout.links[1][0].start, layout.links[1][0].end), (0, 5));
    }

    #[test]
    fn code_blocks_use_high_contrast_rounded_chrome() {
        let lines = render("```rust\npub fn main() {}\n```", 32, &Theme::default()).lines;

        assert_eq!(lines[0].to_string(), "╭─ rust ───────────────────────╮");
        assert_eq!(lines[1].to_string(), "│ pub fn main() {}             │");
        assert_eq!(lines[2].to_string(), "╰──────────────────────────────╯");
        let keyword = lines[1]
            .spans
            .iter()
            .find(|span| span.content == "pub")
            .expect("Rust keywords should be syntax-highlighted separately");
        assert_ne!(keyword.style.fg, Some(Theme::default().code_text()));
        assert!(lines[1].spans.iter().all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn fenced_languages_use_syntects_built_in_syntaxes() {
        let lines = render(
            "```javascript\nconst greeting = \"hello\";\n```",
            40,
            &Theme::default(),
        )
        .lines;
        let keyword = lines[1]
            .spans
            .iter()
            .find(|span| span.content == "const")
            .expect("JavaScript keywords should be syntax-highlighted separately");

        assert_ne!(keyword.style.fg, Some(Theme::default().code_text()));
    }

    #[test]
    fn diff_code_blocks_color_additions_and_deletions() {
        let lines = render(
            "```diff\n--- a/file.rs\n+++ b/file.rs\n-old value\n+new value\n context\n```",
            32,
            &Theme::default(),
        )
        .lines;
        let addition = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "+ ")
            .expect("addition should be rendered");
        let deletion = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "- ")
            .expect("deletion should be rendered");
        assert_eq!(addition.style.fg, Some(Color::Green));
        assert_eq!(deletion.style.fg, Some(Color::Red));
        assert_eq!(addition.style.bg, None);
        assert_eq!(deletion.style.bg, None);
    }

    #[test]
    fn diff_code_blocks_render_hunk_ranges_and_highlight_source() {
        let lines = render(
            "```diff\ndiff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,2 +10,3 @@ impl App\n-pub fn old() {}\n+pub fn new() {}\n+let value = 1;\n```",
            60,
            &Theme::default(),
        )
        .lines;
        let rendered = lines.iter().map(ToString::to_string).collect::<String>();
        let keyword = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "fn")
            .expect("Rust keyword should be syntax-highlighted separately");

        assert!(rendered.contains("src/lib.rs"));
        assert!(rendered.contains("-10,2 → +10,3"));
        assert_ne!(keyword.style.fg, Some(Color::Green));
        assert_ne!(keyword.style.fg, Some(Color::Red));
    }

    #[test]
    fn tables_use_rounded_unicode_chrome() {
        let lines = render("| A | B |\n|---|---|\n| 1 | 2 |", 30, &Theme::default()).lines;
        let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();

        assert!(rendered.first().unwrap().starts_with('╭'));
        assert!(rendered.last().unwrap().starts_with('╰'));
        assert!(rendered.iter().any(|line| line.contains('┼')));
    }

    #[test]
    fn narrow_tables_fall_back_without_overflowing() {
        let lines = render(
            "| Header | Other |\n|---|---|\n| value | data |",
            8,
            &Theme::default(),
        )
        .lines;

        assert!(lines.iter().all(|line| line.width() <= 8));
        assert!(lines.first().unwrap().to_string().starts_with('╭'));
    }

    #[test]
    fn terminal_controls_are_sanitized() {
        let rendered = render("hello \u{1b}[31mred", 80, &Theme::default())
            .lines
            .into_iter()
            .map(|line| line.to_string())
            .collect::<String>();

        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains('�'));
    }
}
