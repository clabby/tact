//! Compact digests of finished tool calls, so a transcript row can show how a command ended or
//! how large a patch was without fetching the full tool detail.

use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;

/// The most output lines a [`ToolOutcome`] carries.
const TAIL_LINES: usize = 8;
/// The most characters of one tail line, including the trailing ellipsis of a shortened line.
const TAIL_LINE_CHARS: usize = 200;

/// How a finished command-like tool ended: its exit code, its last output lines, and a test or
/// build summary when the output contains a recognisable one.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct ToolOutcome {
    pub(super) exit_code: Option<i64>,
    pub(super) tail: Vec<String>,
    pub(super) summary: Option<String>,
}

impl ToolOutcome {
    /// Digests a tool result that carries command output (an `output` string or an `exit_code`
    /// field, as shell tools report). Results without either have no outcome.
    pub(super) fn from_result(result: &Value) -> Option<Self> {
        let fields = result.as_object()?;
        let output = fields.get("output").and_then(Value::as_str);
        if output.is_none() && !fields.contains_key("exit_code") {
            return None;
        }
        let output = output.unwrap_or_default();
        Some(Self {
            exit_code: fields.get("exit_code").and_then(Value::as_i64),
            tail: tail(output),
            summary: summary(output),
        })
    }
}

/// The size of an `apply_patch` envelope: files touched and lines added and removed.
#[derive(Debug, Default, Eq, PartialEq, Serialize)]
pub(super) struct PatchStats {
    pub(super) files: usize,
    pub(super) additions: usize,
    pub(super) deletions: usize,
}

impl PatchStats {
    /// Counts an envelope's file headers and its `+`/`-` lines. A deleted file's removed lines
    /// are not part of the envelope, so they are not counted.
    pub(super) fn from_patch(patch: &str) -> Option<Self> {
        let mut stats = Self::default();
        let mut in_file = false;
        for line in patch.lines() {
            if line.starts_with("*** ") {
                let header = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                    .into_iter()
                    .any(|prefix| line.starts_with(prefix));
                if header {
                    stats.files += 1;
                    in_file = true;
                } else if !line.starts_with("*** Move to: ") && line != "*** End of File" {
                    in_file = false;
                }
                continue;
            }
            if !in_file {
                continue;
            }
            match line.as_bytes().first() {
                Some(b'+') => stats.additions += 1,
                Some(b'-') => stats.deletions += 1,
                _ => {}
            }
        }
        (stats.files > 0).then_some(stats)
    }
}

/// The last [`TAIL_LINES`] lines of output that hold visible text, without terminal escapes.
fn tail(output: &str) -> Vec<String> {
    let mut lines: Vec<String> = output
        .lines()
        .rev()
        .map(visible_text)
        .filter(|line| !line.trim().is_empty())
        .take(TAIL_LINES)
        .map(|line| shorten(line.trim_end()))
        .collect();
    lines.reverse();
    lines
}

fn shorten(line: &str) -> String {
    if line.chars().nth(TAIL_LINE_CHARS).is_none() {
        return line.to_owned();
    }
    let mut head: String = line.chars().take(TAIL_LINE_CHARS - 1).collect();
    head.push('…');
    head
}

/// What a terminal would show for one output line: ANSI escape sequences and control characters
/// removed, and only the text after the last carriage return (progress bars redraw that way).
fn visible_text(line: &str) -> Cow<'_, str> {
    let line = line
        .rsplit('\r')
        .find(|part| !part.is_empty())
        .unwrap_or("");
    if !line.chars().any(|c| c.is_control() && c != '\t') {
        return Cow::Borrowed(line);
    }
    let mut text = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if !c.is_control() || c == '\t' {
                text.push(c);
            }
            continue;
        }
        match chars.next() {
            // Control sequence: parameters and intermediates up to a final byte in @..=~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // Operating system command: terminated by BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    Cow::Owned(text)
}

/// Pass, fail, and skip counts of a test run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestCounts {
    passed: u64,
    failed: u64,
    skipped: u64,
}

impl TestCounts {
    /// Tallies every `<number> <word>` pair such as "17 passed" or "1 fail", ignoring other pairs
    /// like "18 total" or "0 measured".
    fn tally(text: &str) -> Self {
        let mut counts = Self::default();
        let mut words = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .peekable();
        while let Some(word) = words.next() {
            let Ok(number) = word.parse::<u64>() else {
                continue;
            };
            let Some(label) = words.peek() else {
                break;
            };
            match *label {
                "passed" | "pass" => counts.passed += number,
                "failed" | "fail" | "error" | "errors" => counts.failed += number,
                "skipped" | "skip" | "ignored" | "todo" | "xfailed" => counts.skipped += number,
                _ => continue,
            }
            words.next();
        }
        counts
    }

    fn add(&mut self, other: Self) {
        self.passed += other.passed;
        self.failed += other.failed;
        self.skipped += other.skipped;
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }

    fn describe(self, unit: &str) -> String {
        let mut parts = vec![format!("{} {unit}passed", self.passed)];
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.skipped > 0 {
            parts.push(format!("{} skipped", self.skipped));
        }
        parts.join(", ")
    }
}

/// The summaries one pass over the output collects, one slot per recognised tool.
#[derive(Default)]
struct Summaries {
    /// Sum of every `test result:` line; a workspace run prints one per test binary.
    cargo: Option<TestCounts>,
    /// The last nextest `Summary [...]` line.
    nextest: Option<TestCounts>,
    /// The last pytest result line, such as `== 1 failed, 17 passed in 0.12s ==`.
    pytest: Option<TestCounts>,
    /// The last Jest `Tests:` line.
    jest: Option<TestCounts>,
    /// Bun's bare `17 pass` / `1 fail` lines.
    bun: Option<TestCounts>,
    /// Go's top-level `--- PASS:` / `--- FAIL:` / `--- SKIP:` lines (verbose runs).
    go_tests: Option<TestCounts>,
    /// Go's per-package `ok` / `FAIL` lines.
    go_packages: Option<TestCounts>,
    /// Compiler errors and warnings from cargo and tsc.
    build_errors: u64,
    build_warnings: u64,
}

impl Summaries {
    fn observe(&mut self, line: &str) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("test result:") {
            self.cargo
                .get_or_insert_default()
                .add(TestCounts::tally(rest));
        } else if let Some(rest) = trimmed.strip_prefix("Summary [") {
            let rest = rest.split_once(']').map_or(rest, |(_, rest)| rest);
            self.nextest = Some(TestCounts::tally(rest));
        } else if let Some(rest) = trimmed.strip_prefix("Tests:") {
            self.jest = Some(TestCounts::tally(rest));
        } else if let Some(counts) = bun_count(trimmed) {
            self.bun.get_or_insert_default().add(counts);
        } else if let Some(counts) = go_test(line) {
            self.go_tests.get_or_insert_default().add(counts);
        } else if let Some(counts) = go_package(line) {
            self.go_packages.get_or_insert_default().add(counts);
        } else if let Some(counts) = pytest_result(trimmed) {
            self.pytest = Some(counts);
        } else {
            self.observe_build(trimmed);
        }
    }

    fn observe_build(&mut self, line: &str) {
        let count_before = |text: &str, word: &str| {
            let (before, _) = text.split_once(word)?;
            before.split_whitespace().last()?.parse::<u64>().ok()
        };
        if line.starts_with("error: could not compile") {
            self.build_errors += count_before(line, " previous error").unwrap_or(1);
            self.build_warnings += count_before(line, " warning").unwrap_or(0);
        } else if line.starts_with("warning: ") && line.contains(" generated ") {
            self.build_warnings += count_before(line, " warning").unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Found ") {
            self.build_errors += count_before(rest, " error").unwrap_or(0);
        }
    }

    fn summary(self) -> Option<String> {
        let tests = [
            self.cargo,
            self.nextest,
            self.pytest,
            self.jest,
            self.bun,
            self.go_tests,
        ]
        .into_iter()
        .flatten()
        .find(|counts| !counts.is_empty());
        if let Some(counts) = tests {
            return Some(counts.describe(""));
        }
        if let Some(packages) = self.go_packages {
            return Some(packages.describe("packages "));
        }
        let plural =
            |count: u64, word: &str| format!("{count} {word}{}", if count == 1 { "" } else { "s" });
        match (self.build_errors, self.build_warnings) {
            (0, 0) => None,
            (errors, 0) => Some(plural(errors, "error")),
            (0, warnings) => Some(plural(warnings, "warning")),
            (errors, warnings) => Some(format!(
                "{}, {}",
                plural(errors, "error"),
                plural(warnings, "warning")
            )),
        }
    }
}

/// Bun prints its totals as bare lines: `17 pass`, `1 fail`, `2 skip`, `1 todo`.
fn bun_count(line: &str) -> Option<TestCounts> {
    let (number, label) = line.split_once(' ')?;
    number.parse::<u64>().ok()?;
    matches!(label, "pass" | "fail" | "skip" | "todo").then(|| TestCounts::tally(line))
}

fn go_test(line: &str) -> Option<TestCounts> {
    let rest = line.strip_prefix("--- ")?;
    let mut counts = TestCounts::default();
    if rest.starts_with("PASS:") {
        counts.passed = 1;
    } else if rest.starts_with("FAIL:") {
        counts.failed = 1;
    } else if rest.starts_with("SKIP:") {
        counts.skipped = 1;
    } else {
        return None;
    }
    Some(counts)
}

/// `ok  \tpkg\t0.01s` or `FAIL\tpkg\t0.01s`; a bare `FAIL` line closes the run and is not a package.
fn go_package(line: &str) -> Option<TestCounts> {
    let (status, rest) = line.split_once('\t')?;
    if rest.is_empty() {
        return None;
    }
    let mut counts = TestCounts::default();
    match status.trim_end() {
        "ok" => counts.passed = 1,
        "FAIL" => counts.failed = 1,
        _ => return None,
    }
    Some(counts)
}

/// Pytest closes with `== 1 failed, 17 passed, 2 skipped in 0.12s ==` (or the same without the
/// rules in quiet mode).
fn pytest_result(line: &str) -> Option<TestCounts> {
    let core = line.trim_matches(|c: char| c == '=' || c.is_whitespace());
    let (counts, duration) = core.rsplit_once(" in ")?;
    let duration = duration.split_whitespace().next()?;
    if !duration.ends_with('s') || !counts.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(TestCounts::tally(counts)).filter(|counts| !counts.is_empty())
}

fn summary(output: &str) -> Option<String> {
    let mut summaries = Summaries::default();
    for line in output.lines() {
        summaries.observe(&visible_text(line));
    }
    summaries.summary()
}

#[cfg(test)]
mod tests {
    use super::{PatchStats, ToolOutcome, summary, tail, visible_text};
    use serde_json::json;

    #[test]
    fn terminal_escapes_and_redrawn_progress_are_removed() {
        assert_eq!(visible_text("\u{1b}[1;32mok\u{1b}[0m done"), "ok done");
        assert_eq!(
            visible_text("\u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{1b}\\ after"),
            "link after"
        );
        assert_eq!(visible_text("10%\r50%\r100% done"), "100% done");
        assert_eq!(visible_text("tab\tkept\u{7}"), "tab\tkept");
        assert_eq!(visible_text("plain"), "plain");
    }

    #[test]
    fn the_tail_keeps_the_last_eight_visible_lines_shortened_to_two_hundred_chars() {
        let mut output: String = (1..=12).map(|n| format!("line {n}\n\n")).collect();
        output.push_str("\u{1b}[31m\u{1b}[0m\n");
        output.push_str(&"x".repeat(300));
        output.push_str("   \n");
        let tail = tail(&output);
        assert_eq!(tail.len(), 8);
        assert_eq!(tail[0], "line 6");
        assert_eq!(tail[6], "line 12");
        assert_eq!(tail[7].chars().count(), 200);
        assert!(tail[7].ends_with('…'));
        assert!(super::tail("\n  \n").is_empty());
    }

    #[test]
    fn summaries_are_recognised_across_test_runners_and_builds() {
        let cases = [
            (
                "running 3 tests\ntest result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\
                 running 15 tests\ntest result: FAILED. 14 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out",
                Some("17 passed, 1 failed, 1 skipped"),
            ),
            (
                "        PASS [   0.004s] tact web::a\n────────────\n     Summary [   1.234s] 18 tests run: 17 passed, 1 failed, 2 skipped",
                Some("17 passed, 1 failed, 2 skipped"),
            ),
            (
                "\u{1b}[31m=========== 1 failed, 17 passed, 2 warnings in 0.52s ===========\u{1b}[0m",
                Some("17 passed, 1 failed"),
            ),
            ("17 passed in 0.12s", Some("17 passed")),
            (
                "Test Suites: 1 failed, 3 passed, 4 total\nTests:       1 failed, 1 skipped, 17 passed, 19 total\nTime: 2 s",
                Some("17 passed, 1 failed, 1 skipped"),
            ),
            (
                "bun test v1.1.0\n\n 17 pass\n 1 fail\n 41 expect() calls\nRan 18 tests across 3 files. [12.00ms]",
                Some("17 passed, 1 failed"),
            ),
            (
                "=== RUN   TestA\n--- PASS: TestA (0.00s)\n    --- PASS: TestA/sub (0.00s)\n--- FAIL: TestB (0.00s)\nFAIL\nFAIL\texample.com/m\t0.01s\nFAIL",
                Some("1 passed, 1 failed"),
            ),
            (
                "ok  \texample.com/m/a\t0.012s\n?   \texample.com/m/b\t[no test files]\nFAIL\texample.com/m/c\t0.020s\nFAIL",
                Some("1 packages passed, 1 failed"),
            ),
            (
                "warning: unused variable\nwarning: `tact` (bin \"tact\") generated 2 warnings\nerror: could not compile `tact` (bin \"tact\") due to 3 previous errors; 2 warnings emitted",
                Some("3 errors, 4 warnings"),
            ),
            ("Found 1 error in src/a.ts:3", Some("1 error")),
            ("hello world\nall good", None),
            ("total 3 passed tickets in 2 days", None),
        ];
        for (output, expected) in cases {
            assert_eq!(summary(output).as_deref(), expected, "{output}");
        }
    }

    #[test]
    fn outcomes_need_command_output_or_an_exit_code() {
        let outcome = ToolOutcome::from_result(&json!({
            "output": "compiling\n\u{1b}[32mtest result: ok. 2 passed; 0 failed\u{1b}[0m\n",
            "exit_code": 0,
        }))
        .unwrap();
        assert_eq!(
            outcome,
            ToolOutcome {
                exit_code: Some(0),
                tail: vec![
                    "compiling".to_owned(),
                    "test result: ok. 2 passed; 0 failed".to_owned()
                ],
                summary: Some("2 passed".to_owned()),
            }
        );
        let killed =
            ToolOutcome::from_result(&json!({"exit_code": null, "error": "killed"})).unwrap();
        assert_eq!(killed.exit_code, None);
        assert!(killed.tail.is_empty());
        assert_eq!(ToolOutcome::from_result(&json!({"text": "hi"})), None);
        assert_eq!(ToolOutcome::from_result(&json!("output")), None);
    }

    #[test]
    fn patch_stats_count_files_and_changed_lines() {
        let patch = "*** Begin Patch\n\
            *** Add File: new.rs\n+fn a() {}\n+\n\
            *** Update File: src/lib.rs\n*** Move to: src/core.rs\n@@ fn b\n context\n-old\n--- not a header\n+new\n*** End of File\n\
            *** Delete File: gone.rs\n\
            *** End Patch";
        assert_eq!(
            PatchStats::from_patch(patch),
            Some(PatchStats {
                files: 3,
                additions: 3,
                deletions: 2,
            })
        );
        assert_eq!(PatchStats::from_patch("not a patch"), None);
    }
}
