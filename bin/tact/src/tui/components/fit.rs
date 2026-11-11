//! Fitting text into a number of terminal columns.
//!
//! Widths are measured in terminal columns and text is only ever cut between grapheme clusters,
//! so wide characters and combining sequences are never split.

use std::{borrow::Cow, mem};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The longest prefix of `text` that fits in `width` columns.
pub(super) fn clip_to_width(text: &str, width: usize) -> &str {
    let mut used = 0_usize;
    for (index, grapheme) in text.grapheme_indices(true) {
        used = used.saturating_add(grapheme.width());
        if used > width {
            return &text[..index];
        }
    }
    text
}

/// `text` unchanged when it fits in `width` columns, and otherwise cut to end in an ellipsis that
/// keeps the result within `width`.
pub(super) fn ellipsize(text: Cow<'_, str>, width: usize) -> Cow<'_, str> {
    if text.width() <= width {
        return text;
    }
    let Some(content_width) = width.checked_sub(1) else {
        return Cow::Borrowed("");
    };
    let mut fitted = clip_to_width(&text, content_width).to_owned();
    fitted.push('…');
    Cow::Owned(fitted)
}

/// `text` broken into at most `max_rows` rows of at most `width` columns. Rows break between
/// words, and a word wider than a row is split between graphemes. When the text needs more rows,
/// the last row ends in an ellipsis.
pub(super) fn wrap(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    let mut rows = Vec::new();
    if width == 0 || max_rows == 0 {
        return rows;
    }
    let mut row = String::new();
    for mut word in text.split_whitespace() {
        if rows.len() > max_rows {
            break;
        }
        if !row.is_empty() && row.width() + 1 + word.width() > width {
            rows.push(mem::take(&mut row));
        }
        while word.width() > width {
            // A grapheme wider than the whole row still has to go somewhere.
            let head = match clip_to_width(word, width) {
                "" => word.graphemes(true).next().unwrap_or(word),
                head => head,
            };
            rows.push(head.to_owned());
            word = &word[head.len()..];
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    if rows.len() > max_rows {
        rows.truncate(max_rows);
        if let Some(last) = rows.pop() {
            rows.push(ellipsize(Cow::Owned(format!("{last}…")), width).into_owned());
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{clip_to_width, ellipsize, wrap};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn clipping_keeps_whole_graphemes_within_the_width() {
        assert_eq!(clip_to_width("abcdef", 4), "abcd");
        assert_eq!(clip_to_width("abc", 3), "abc");
        assert_eq!(clip_to_width("界界界", 5), "界界");
        assert_eq!(clip_to_width("e\u{301}x", 1), "e\u{301}");
        assert_eq!(clip_to_width("abc", 0), "");
    }

    #[test]
    fn ellipsizing_marks_cut_text_and_never_exceeds_the_width() {
        assert_eq!(ellipsize("abc".into(), 3), "abc");
        assert_eq!(ellipsize("abcdef".into(), 4), "abc…");
        assert_eq!(ellipsize("abcdef".into(), 0), "");
        for width in 0..8 {
            assert!(ellipsize("界界界界".into(), width).width() <= width);
        }
    }

    #[test]
    fn wrapping_breaks_between_words_and_splits_words_wider_than_a_row() {
        assert_eq!(
            wrap("alpha  beta gamma", 10, usize::MAX),
            ["alpha beta", "gamma"]
        );
        assert_eq!(
            wrap("in /home/user/project", 8, usize::MAX),
            ["in", "/home/us", "er/proje", "ct"]
        );
        assert_eq!(wrap("界界界", 4, usize::MAX), ["界界", "界"]);
        assert_eq!(wrap("界", 1, usize::MAX), ["界"]);
        assert!(wrap("abc", 0, usize::MAX).is_empty());
        assert!(wrap("", 4, usize::MAX).is_empty());
    }

    #[test]
    fn wrapping_marks_text_cut_at_the_row_limit() {
        assert_eq!(wrap("one two three four", 7, 2), ["one two", "three…"]);
        assert_eq!(wrap("abcdefg hij", 7, 1), ["abcdef…"]);
        assert_eq!(wrap("one two", 7, 1), ["one two"]);
        assert!(wrap("abc", 3, 0).is_empty());
    }
}
