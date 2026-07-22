//! Shared formatting for terminal-facing values.

use std::{env, path::Path};

pub(crate) fn format_duration(nanoseconds: u64) -> String {
    if nanoseconds >= 1_000_000_000 {
        return format!("{:.1}s", nanoseconds as f64 / 1_000_000_000.0);
    }
    format!("{}ms", nanoseconds / 1_000_000)
}

pub(crate) fn humanize_tool(name: &str) -> String {
    name.trim_start_matches("mcp__")
        .replace("__", " · ")
        .replace('_', " ")
}

pub(crate) fn shorten_home(path: &Path) -> String {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    let Some(home) = home else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_owned();
    }
    let Ok(relative) = path.strip_prefix(&home) else {
        return path.display().to_string();
    };
    format!("~/{}", relative.display())
}
