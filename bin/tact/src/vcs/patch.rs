//! Parsing of the patches that [`DiffSnapshot`](crate::vcs::DiffSnapshot) captures.
//!
//! The parser understands what `git diff` emits: `diff --git` headers, the extended headers for
//! renames, copies, additions, deletions and binary content, and hunks. Hunk bodies are delimited
//! by the line counts in their headers, so a removed line that happens to read `--- x` is never
//! mistaken for a file header. Bodies are borrowed from the patch text; parsing allocates only per
//! file and per hunk.

/// One file's section of a patch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilePatch<'a> {
    /// The path before the change, or `None` when the change adds the file.
    pub old_path: Option<String>,
    /// The path after the change, or `None` when the change deletes the file.
    pub new_path: Option<String>,
    /// Whether git described the content as binary instead of emitting hunks.
    pub binary: bool,
    pub hunks: Vec<Hunk<'a>>,
}

/// A contiguous run of lines on one side of a hunk, numbered from 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineSpan {
    pub start: u32,
    pub count: u32,
}

/// One hunk: the spans it covers on each side and its marked lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hunk<'a> {
    pub old: LineSpan,
    pub new: LineSpan,
    body: &'a str,
}

impl<'a> FilePatch<'a> {
    /// Splits a patch into its files, in patch order.
    pub fn parse_all(patch: &'a str) -> Vec<Self> {
        let mut files: Vec<Self> = Vec::new();
        let mut rest = patch;
        while let Some((line, after)) = split_line(rest) {
            rest = after;
            if let Some(header) = line.strip_prefix("diff --git ") {
                let (old_path, new_path) = header_paths(header).unzip();
                files.push(Self {
                    old_path,
                    new_path,
                    ..Self::default()
                });
                continue;
            }
            let Some(file) = files.last_mut() else {
                continue;
            };
            if let Some((old, new)) = parse_hunk_header(line) {
                let body_start = patch.len() - rest.len();
                rest = skip_hunk_body(rest, old.count, new.count);
                let body = &patch[body_start..patch.len() - rest.len()];
                file.hunks.push(Hunk { old, new, body });
                continue;
            }
            file.apply_extended_header(line);
        }
        files
    }

    /// The path that names the file in a review: the new path, or the old one for a deletion.
    #[cfg(test)]
    pub fn path(&self) -> Option<&str> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }

    fn apply_extended_header(&mut self, line: &str) {
        if let Some(value) = line.strip_prefix("--- ") {
            self.old_path = patch_path(value);
        } else if let Some(value) = line.strip_prefix("+++ ") {
            self.new_path = patch_path(value);
        } else if let Some(value) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("copy from "))
        {
            self.old_path = decode_git_path(value);
        } else if let Some(value) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
            self.new_path = decode_git_path(value);
        } else if line.starts_with("new file mode ") {
            self.old_path = None;
        } else if line.starts_with("deleted file mode ") {
            self.new_path = None;
        } else if line == "GIT binary patch" || line.starts_with("Binary files ") {
            self.binary = true;
        }
    }
}

impl LineSpan {
    /// Whether every line from `first` through `last` falls inside this span.
    pub fn contains(&self, first: u32, last: u32) -> bool {
        let Some(end) = self.start.checked_add(self.count.saturating_sub(1)) else {
            return false;
        };
        self.count > 0 && first != 0 && first <= last && first >= self.start && last <= end
    }
}

#[cfg(test)]
impl<'a> Hunk<'a> {
    /// The body lines, each still carrying its leading space, `+`, `-` or `\` marker.
    pub fn lines(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        self.body.lines()
    }

    /// The text of the lines this hunk adds.
    pub fn added(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        self.lines().filter_map(|line| line.strip_prefix('+'))
    }

    /// The text of the lines this hunk removes.
    pub fn removed(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        self.lines().filter_map(|line| line.strip_prefix('-'))
    }
}

/// The first line of `text` without its terminator, and the text after it.
fn split_line(text: &str) -> Option<(&str, &str)> {
    if text.is_empty() {
        return None;
    }
    let (line, rest) = text.split_once('\n').unwrap_or((text, ""));
    Some((line.strip_suffix('\r').unwrap_or(line), rest))
}

/// Consumes the body of a hunk that covers `old` and `new` lines, including any trailing
/// "no newline at end of file" marker, and returns the text after it.
fn skip_hunk_body(mut rest: &str, mut old: u32, mut new: u32) -> &str {
    while let Some((line, after)) = split_line(rest) {
        match line.as_bytes().first() {
            Some(b'\\') => {}
            _ if old == 0 && new == 0 => break,
            Some(b'+') => new = new.saturating_sub(1),
            Some(b'-') => old = old.saturating_sub(1),
            Some(b' ') | None => {
                old = old.saturating_sub(1);
                new = new.saturating_sub(1);
            }
            Some(_) => break,
        }
        rest = after;
    }
    rest
}

/// The two paths of a `diff --git a/<old> b/<new>` header. Unquoted paths are only recoverable
/// when both sides name the same file; renames and copies supply their paths in extended headers.
fn header_paths(header: &str) -> Option<(String, String)> {
    let (old, new) = if header.starts_with('"') {
        let (old, rest) = split_quoted(header)?;
        (old, decode_git_path(rest.strip_prefix(' ')?)?)
    } else {
        let length = header.len().checked_sub(5)? / 2;
        let old = header.get(..length + 2)?;
        let new = header.get(length + 3..)?;
        if header.get(length + 2..length + 3)? != " " || old.get(2..) != new.get(2..) {
            return None;
        }
        (old.to_owned(), new.to_owned())
    };
    let strip = |path: String, prefix: &str| path.strip_prefix(prefix).map(str::to_owned);
    Some((strip(old, "a/")?, strip(new, "b/")?))
}

/// Splits a leading git-quoted path from the text after it.
fn split_quoted(text: &str) -> Option<(String, &str)> {
    let bytes = text.as_bytes();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some((decode_git_path(&text[..=index])?, &text[index + 1..])),
            _ => index += 1,
        }
    }
    None
}

fn patch_path(value: &str) -> Option<String> {
    let value = value.split('\t').next().unwrap_or(value);
    if value == "/dev/null" {
        return None;
    }
    let value = decode_git_path(value)?;
    Some(
        value
            .strip_prefix("a/")
            .or_else(|| value.strip_prefix("b/"))
            .unwrap_or(&value)
            .to_owned(),
    )
}

/// Decodes a path that git C-quoted because it contains special or non-ASCII bytes.
fn decode_git_path(value: &str) -> Option<String> {
    let Some(quoted) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return Some(value.to_owned());
    };
    let bytes = quoted.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let escaped = *bytes.get(index)?;
        if escaped.is_ascii_digit() && escaped < b'8' {
            let mut value = 0_u8;
            let mut digits = 0;
            while digits < 3 {
                let Some(digit) = bytes.get(index).copied() else {
                    break;
                };
                if !(b'0'..=b'7').contains(&digit) {
                    break;
                }
                value = value.checked_mul(8)?.checked_add(digit - b'0')?;
                index += 1;
                digits += 1;
            }
            decoded.push(value);
            continue;
        }
        decoded.push(match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'\\' => b'\\',
            b'"' => b'"',
            _ => return None,
        });
        index += 1;
    }
    String::from_utf8(decoded).ok()
}

fn parse_hunk_header(line: &str) -> Option<(LineSpan, LineSpan)> {
    let header = line.strip_prefix("@@ -")?;
    let (old, remainder) = header.split_once(" +")?;
    let (new, _) = remainder.split_once(" @@")?;
    Some((parse_span(old)?, parse_span(new)?))
}

fn parse_span(value: &str) -> Option<LineSpan> {
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    Some(LineSpan {
        start: start.parse().ok()?,
        count: count.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::{FilePatch, LineSpan};

    #[test]
    fn quoted_paths_are_decoded() {
        let patch = concat!(
            "diff --git \"a/caf\\303\\251.rs\" \"b/caf\\303\\251.rs\"\n",
            "--- \"a/caf\\303\\251.rs\"\n",
            "+++ \"b/caf\\303\\251.rs\"\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
        );

        let files = FilePatch::parse_all(patch);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("café.rs"));
        assert_eq!(files[0].new_path.as_deref(), Some("café.rs"));
        assert_eq!(files[0].hunks[0].removed().collect::<Vec<_>>(), ["old"]);
        assert_eq!(files[0].hunks[0].added().collect::<Vec<_>>(), ["new"]);
    }

    #[test]
    fn hunk_bodies_are_delimited_by_their_line_counts() {
        let patch = concat!(
            "diff --git a/notes.md b/notes.md\n",
            "--- a/notes.md\n",
            "+++ b/notes.md\n",
            "@@ -1,3 +1,2 @@\n",
            " keep\n",
            "--- a/elsewhere\n",
            "-@@ -9 +9 @@\n",
            "+added\n",
            "\\ No newline at end of file\n",
            "diff --git a/other.md b/other.md\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/other.md\n",
            "@@ -0,0 +1 @@\n",
            "+fresh\n",
        );

        let files = FilePatch::parse_all(patch);

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), Some("notes.md"));
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(
            files[0].hunks[0].removed().collect::<Vec<_>>(),
            ["-- a/elsewhere", "@@ -9 +9 @@"]
        );
        assert_eq!(files[1].old_path, None);
        assert_eq!(files[1].new_path.as_deref(), Some("other.md"));
        assert_eq!(files[1].hunks[0].added().collect::<Vec<_>>(), ["fresh"]);
    }

    #[test]
    fn renames_deletions_and_binary_content_are_described_without_hunks() {
        let patch = concat!(
            "diff --git a/old name.txt b/new name.txt\n",
            "similarity index 100%\n",
            "rename from old name.txt\n",
            "rename to new name.txt\n",
            "diff --git a/gone.txt b/gone.txt\n",
            "deleted file mode 100644\n",
            "Binary files a/gone.txt and /dev/null differ\n",
            "diff --git a/image.png b/image.png\n",
            "new file mode 100644\n",
            "GIT binary patch\n",
            "literal 2\n",
            "JcmZQzfB^si00961\n",
        );

        let summary = FilePatch::parse_all(patch)
            .iter()
            .map(|file| {
                (
                    file.old_path.clone(),
                    file.new_path.clone(),
                    file.binary,
                    file.hunks.len(),
                )
            })
            .collect::<Vec<_>>();

        let path = |value: &str| Some(value.to_owned());
        assert_eq!(
            summary,
            [
                (path("old name.txt"), path("new name.txt"), false, 0),
                (path("gone.txt"), None, true, 0),
                (None, path("image.png"), true, 0),
            ]
        );
    }

    #[test]
    fn spans_contain_only_ranges_inside_them() {
        let span = LineSpan { start: 3, count: 4 };

        assert!(span.contains(3, 6));
        assert!(span.contains(4, 4));
        assert!(!span.contains(2, 4));
        assert!(!span.contains(5, 7));
        assert!(!span.contains(5, 4));
        assert!(!LineSpan { start: 0, count: 0 }.contains(0, 0));
        assert!(!LineSpan { start: 1, count: 3 }.contains(0, 1));
    }
}
