//! Shared formatting for terminal-facing values.

use std::{env, path::Path};

pub(crate) fn format_duration(nanoseconds: u64) -> String {
    if nanoseconds >= 1_000_000_000 {
        let tenths = duration_display_tick(nanoseconds).saturating_sub(1_000);
        return format!("{}.{:01}s", tenths / 10, tenths % 10);
    }
    format!("{}ms", duration_display_tick(nanoseconds))
}

pub(crate) fn format_turn_duration(nanoseconds: u64) -> String {
    let total_seconds = nanoseconds / 1_000_000_000;
    let days = total_seconds / 86_400;
    let hours = total_seconds % 86_400 / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    parts.push(format!("{seconds}s"));
    parts.join(" ")
}

pub(crate) fn duration_display_tick(nanoseconds: u64) -> u64 {
    if nanoseconds < 1_000_000_000 {
        return nanoseconds / 1_000_000;
    }
    1_000_u64.saturating_add(nanoseconds.saturating_add(50_000_000) / 100_000_000)
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

#[cfg(test)]
mod tests {
    use super::{duration_display_tick, format_duration, format_turn_duration};

    #[test]
    fn durations_round_to_the_same_tick_used_for_live_redraws() {
        for (nanoseconds, expected) in [
            (999_999_999, "999ms"),
            (1_049_999_999, "1.0s"),
            (1_050_000_000, "1.1s"),
            (11_249_999_999, "11.2s"),
            (11_250_000_000, "11.3s"),
        ] {
            assert_eq!(format_duration(nanoseconds), expected);
        }
        assert_eq!(duration_display_tick(1_050_000_000), 1_011);
    }

    #[test]
    fn turn_durations_only_use_whole_seconds_and_larger_units() {
        for (nanoseconds, expected) in [
            (999_999_999, "0s"),
            (5_000_000_000, "5s"),
            (65_000_000_000, "1m 5s"),
            (3_665_000_000_000, "1h 1m 5s"),
            (176_465_000_000_000, "2d 1h 1m 5s"),
        ] {
            assert_eq!(format_turn_duration(nanoseconds), expected);
        }
    }
}
