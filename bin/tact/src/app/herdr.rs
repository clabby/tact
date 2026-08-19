//! Best-effort lifecycle reporting for Tact sessions hosted by Herdr.

use std::{
    env,
    ffi::{OsStr, OsString},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

const AGENT: &str = "tact";
const SOURCE: &str = "herdr:tact";

pub(crate) struct Reporter(Option<ActiveReporter>);

impl Reporter {
    pub(crate) fn from_env(session_id: &str) -> Self {
        let Some((binary, pane_id)) = environment(
            env::var("HERDR_ENV").ok(),
            env::var_os("HERDR_BIN_PATH"),
            env::var("HERDR_PANE_ID").ok(),
        ) else {
            return Self(None);
        };
        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_mul(1_000)
            .min(u128::from(u64::MAX)) as u64;
        let mut reporter = ActiveReporter {
            binary,
            pane_id,
            session_id: session_id.to_owned(),
            sequence,
        };
        reporter.report(State::Idle);
        Self(Some(reporter))
    }

    pub(crate) fn working(&mut self, session_id: Option<&str>) {
        let Some(reporter) = &mut self.0 else {
            return;
        };
        reporter.update_session(session_id);
        reporter.report(State::Working);
    }

    pub(crate) fn idle(&mut self, session_id: Option<&str>) {
        let Some(reporter) = &mut self.0 else {
            return;
        };
        reporter.update_session(session_id);
        reporter.report(State::Idle);
    }
}

struct ActiveReporter {
    binary: OsString,
    pane_id: String,
    session_id: String,
    sequence: u64,
}

impl ActiveReporter {
    fn update_session(&mut self, session_id: Option<&str>) {
        if let Some(session_id) = session_id {
            session_id.clone_into(&mut self.session_id);
        }
    }

    fn report(&mut self, state: State) {
        let sequence = self.next_sequence();
        spawn(command(
            &self.binary,
            &self.pane_id,
            Report::State {
                state,
                session_id: &self.session_id,
            },
            sequence,
        ));
    }

    fn next_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        sequence
    }
}

impl Drop for ActiveReporter {
    fn drop(&mut self) {
        let sequence = self.next_sequence();
        spawn(command(
            &self.binary,
            &self.pane_id,
            Report::Release,
            sequence,
        ));
    }
}

#[derive(Clone, Copy)]
enum State {
    Idle,
    Working,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
        }
    }
}

enum Report<'a> {
    State { state: State, session_id: &'a str },
    Release,
}

fn environment(
    herdr_env: Option<String>,
    binary: Option<OsString>,
    pane_id: Option<String>,
) -> Option<(OsString, String)> {
    if herdr_env.as_deref() != Some("1") {
        return None;
    }
    Some((
        binary
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| "herdr".into()),
        pane_id.filter(|pane_id| !pane_id.is_empty())?,
    ))
}

fn command(binary: &OsStr, pane_id: &str, report: Report<'_>, sequence: u64) -> Command {
    let mut command = Command::new(binary);
    command.args(["pane"]);
    match report {
        Report::State { state, session_id } => {
            command.args([
                "report-agent",
                pane_id,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--state",
                state.as_str(),
                "--agent-session-id",
                session_id,
            ]);
        }
        Report::Release => {
            command.args([
                "release-agent",
                pane_id,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
            ]);
        }
    }
    command
        .args(["--seq", &sequence.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn spawn(mut command: Command) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let Ok(mut child) = command.spawn() else {
        return;
    };
    runtime.spawn(async move {
        drop(child.wait().await);
    });
}

#[cfg(test)]
mod tests {
    use super::{ActiveReporter, Report, State, command, environment};
    use std::ffi::{OsStr, OsString};

    #[test]
    fn activates_only_inside_a_herdr_pane() {
        assert_eq!(
            environment(
                Some("1".to_owned()),
                Some("/opt/herdr".into()),
                Some("w1:p1".to_owned()),
            ),
            Some(("/opt/herdr".into(), "w1:p1".to_owned()))
        );
        assert_eq!(
            environment(None, Some("/opt/herdr".into()), Some("w1:p1".to_owned())),
            None
        );
        assert_eq!(
            environment(Some("1".to_owned()), None, Some("w1:p1".to_owned())),
            Some(("herdr".into(), "w1:p1".to_owned()))
        );
        assert_eq!(
            environment(Some("1".to_owned()), Some("/opt/herdr".into()), None),
            None
        );
        assert_eq!(
            environment(
                Some("1".to_owned()),
                Some(OsString::new()),
                Some("w1:p1".to_owned()),
            ),
            Some(("herdr".into(), "w1:p1".to_owned()))
        );
        assert_eq!(
            environment(
                Some("1".to_owned()),
                Some("/opt/herdr".into()),
                Some(String::new()),
            ),
            None
        );
    }

    #[test]
    fn reports_state_with_session_identity() {
        let command = command(
            OsStr::new("/opt/herdr"),
            "w1:p1",
            Report::State {
                state: State::Working,
                session_id: "session-1",
            },
            42,
        );

        assert_eq!(command.as_std().get_program(), "/opt/herdr");
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            [
                "pane",
                "report-agent",
                "w1:p1",
                "--source",
                "herdr:tact",
                "--agent",
                "tact",
                "--state",
                "working",
                "--agent-session-id",
                "session-1",
                "--seq",
                "42",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn releases_lifecycle_ownership() {
        let command = command(OsStr::new("/opt/herdr"), "w1:p1", Report::Release, 43);

        assert_eq!(command.as_std().get_program(), "/opt/herdr");
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            [
                "pane",
                "release-agent",
                "w1:p1",
                "--source",
                "herdr:tact",
                "--agent",
                "tact",
                "--seq",
                "43",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn refreshes_the_session_identity() {
        let mut reporter = ActiveReporter {
            binary: "/opt/herdr".into(),
            pane_id: "w1:p1".to_owned(),
            session_id: "old".to_owned(),
            sequence: 1,
        };

        reporter.update_session(Some("new"));

        assert_eq!(reporter.session_id, "new");
    }
}
