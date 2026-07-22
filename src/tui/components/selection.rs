//! Shared mouse selection over rendered component surfaces.

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Style},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Surface {
    Transcript,
    Composer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Point {
    surface: Surface,
    position: Position,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Range {
    anchor: Point,
    head: Point,
}

#[derive(Default)]
pub(super) struct Selection {
    pending: Option<Point>,
    range: Option<Range>,
    snapshot: Option<Snapshot>,
}

struct Snapshot {
    area: Rect,
    rows: Vec<Vec<String>>,
}

impl Selection {
    pub(super) fn begin(&mut self, surface: Surface, position: Position) {
        let point = Point { surface, position };
        self.pending = Some(point);
        self.range = None;
        self.snapshot = None;
    }

    pub(super) fn drag(&mut self, position: Position, area: Rect) -> bool {
        let Some(anchor) = self
            .pending
            .or_else(|| self.range.map(|range| range.anchor))
        else {
            return false;
        };
        let head = Point {
            surface: anchor.surface,
            position: clamp(position, area),
        };
        self.range = Some(Range { anchor, head });
        true
    }

    pub(super) fn finish(&mut self, position: Position, area: Rect) -> bool {
        let Some(anchor) = self
            .pending
            .or_else(|| self.range.map(|range| range.anchor))
        else {
            return false;
        };
        let head = Point {
            surface: anchor.surface,
            position: clamp(position, area),
        };
        if self.range.is_none() && head == anchor {
            self.cancel_pending();
            return false;
        }
        self.range = Some(Range { anchor, head });
        self.pending = None;
        true
    }

    fn cancel_pending(&mut self) {
        self.pending = None;
        if self.range.is_none() {
            self.snapshot = None;
        }
    }

    pub(super) fn clear(&mut self) -> bool {
        let changed = self.pending.take().is_some() || self.range.take().is_some();
        self.snapshot = None;
        changed
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn is_active(&self) -> bool {
        self.range.is_some()
    }

    pub(super) fn surface(&self) -> Option<Surface> {
        self.pending
            .map(|point| point.surface)
            .or_else(|| self.range.map(|range| range.anchor.surface))
    }

    pub(super) fn capture_and_render(&mut self, buffer: &mut Buffer, area: Rect) {
        if self.pending.is_none() && self.range.is_none() {
            return;
        }

        self.snapshot = Some(Snapshot::capture(buffer, area));
        let Some(range) = self.range else {
            return;
        };
        for position in positions(range, area) {
            if let Some(cell) = buffer.cell_mut(position) {
                cell.set_style(Style::reset().fg(Color::Black).bg(Color::Yellow));
            }
        }
    }

    pub(super) fn take_text(&mut self) -> Option<String> {
        let range = self.range.take()?;
        self.pending = None;
        let snapshot = self.snapshot.take()?;
        let text = snapshot.text(range);
        (!text.is_empty()).then_some(text)
    }
}

impl Snapshot {
    fn capture(buffer: &Buffer, area: Rect) -> Self {
        let rows = (area.y..area.bottom())
            .map(|y| capture_row(buffer, area, y))
            .collect();
        Self { area, rows }
    }

    fn text(&self, range: Range) -> String {
        let mut lines = Vec::new();
        for row in selected_rows(range, self.area) {
            let y = usize::from(row.y.saturating_sub(self.area.y));
            let start = usize::from(row.start.saturating_sub(self.area.x));
            let end = usize::from(row.end.saturating_sub(self.area.x));
            let Some(cells) = self.rows.get(y).and_then(|line| line.get(start..=end)) else {
                continue;
            };
            let line = cells.concat();
            lines.push(line.trim_end().to_owned());
        }
        lines.join("\n")
    }
}

fn capture_row(buffer: &Buffer, area: Rect, y: u16) -> Vec<String> {
    let mut continuation_cells = 0;
    (area.x..area.right())
        .map(|x| {
            if continuation_cells > 0 {
                continuation_cells -= 1;
                return String::new();
            }
            let symbol = buffer[(x, y)].symbol();
            continuation_cells = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
            symbol.to_owned()
        })
        .collect()
}

#[derive(Clone, Copy)]
struct SelectedRow {
    y: u16,
    start: u16,
    end: u16,
}

fn positions(range: Range, area: Rect) -> impl Iterator<Item = Position> {
    selected_rows(range, area)
        .flat_map(|row| (row.start..=row.end).map(move |x| Position::new(x, row.y)))
}

fn selected_rows(range: Range, area: Rect) -> impl Iterator<Item = SelectedRow> {
    let (start, end) = ordered(range.anchor.position, range.head.position);
    (start.y..=end.y).map(move |y| SelectedRow {
        y,
        start: if y == start.y { start.x } else { area.x },
        end: if y == end.y {
            end.x
        } else {
            area.right().saturating_sub(1)
        },
    })
}

fn ordered(left: Position, right: Position) -> (Position, Position) {
    if (left.y, left.x) <= (right.y, right.x) {
        (left, right)
    } else {
        (right, left)
    }
}

fn clamp(position: Position, area: Rect) -> Position {
    Position::new(
        position.x.clamp(area.x, area.right().saturating_sub(1)),
        position.y.clamp(area.y, area.bottom().saturating_sub(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::{Selection, Surface};
    use ratatui::{
        buffer::Buffer,
        layout::{Position, Rect},
        style::{Color, Modifier, Style},
    };

    #[test]
    fn selection_extracts_forward_and_reverse_visual_ranges() {
        for (anchor, head) in [
            (Position::new(1, 0), Position::new(2, 1)),
            (Position::new(2, 1), Position::new(1, 0)),
        ] {
            let area = Rect::new(0, 0, 5, 2);
            let mut buffer = Buffer::empty(area);
            buffer.set_string(0, 0, "alpha", Style::default());
            buffer.set_string(0, 1, "beta ", Style::default());
            let mut selection = Selection::default();
            selection.begin(Surface::Transcript, anchor);
            selection.drag(head, area);
            selection.capture_and_render(&mut buffer, area);

            assert_eq!(selection.take_text().as_deref(), Some("lpha\nbet"));
        }
    }

    #[test]
    fn pending_click_does_not_create_copyable_text() {
        let mut selection = Selection::default();
        selection.begin(Surface::Composer, Position::new(1, 1));

        assert!(selection.is_pending());
        assert!(!selection.finish(Position::new(1, 1), Rect::new(0, 0, 3, 3)));
        assert!(selection.take_text().is_none());
    }

    #[test]
    fn displaced_release_finishes_without_a_drag_event() {
        let area = Rect::new(0, 0, 7, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "copy me", Style::default());
        let mut selection = Selection::default();
        selection.begin(Surface::Composer, Position::new(0, 0));
        selection.capture_and_render(&mut buffer, area);

        assert!(selection.finish(Position::new(6, 0), area));
        assert_eq!(selection.take_text().as_deref(), Some("copy me"));
    }

    #[test]
    fn wide_graphemes_do_not_add_continuation_spaces() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "a界b", Style::default());
        let mut selection = Selection::default();
        selection.begin(Surface::Composer, Position::new(0, 0));
        selection.drag(Position::new(3, 0), area);
        selection.capture_and_render(&mut buffer, area);

        assert_eq!(selection.take_text().as_deref(), Some("a界b"));
    }

    #[test]
    fn selection_is_always_black_on_yellow() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(
            0,
            0,
            "text",
            Style::default()
                .fg(Color::Red)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        );
        let mut selection = Selection::default();
        selection.begin(Surface::Transcript, Position::new(1, 0));
        selection.drag(Position::new(2, 0), area);

        selection.capture_and_render(&mut buffer, area);

        for x in 1..=2 {
            assert_eq!(buffer[(x, 0)].fg, Color::Black);
            assert_eq!(buffer[(x, 0)].bg, Color::Yellow);
            assert!(buffer[(x, 0)].modifier.is_empty());
        }
        assert_eq!(buffer[(0, 0)].fg, Color::Red);
        assert_eq!(buffer[(0, 0)].bg, Color::Blue);
    }
}
