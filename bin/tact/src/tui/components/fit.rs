//! Fitting text into a number of terminal columns.
//!
//! Widths are measured in terminal columns and text is only ever cut between grapheme clusters,
//! so wide characters and combining sequences are never split.

use std::borrow::Cow;
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

#[cfg(test)]
mod tests {
    use super::{clip_to_width, ellipsize};
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
}
