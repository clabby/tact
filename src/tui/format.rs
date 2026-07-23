//! Shared formatting for terminal-facing values.

use std::{env, path::Path};

pub(crate) fn format_duration(nanoseconds: u64) -> String {
    if nanoseconds >= 1_000_000_000 {
        let tenths = duration_display_tick(nanoseconds).saturating_sub(1_000);
        return format!("{}.{:01}s", tenths / 10, tenths % 10);
    }
    format!("{}ms", duration_display_tick(nanoseconds))
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
    use super::{duration_display_tick, format_duration};

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
}
