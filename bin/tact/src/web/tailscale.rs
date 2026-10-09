//! Publishes the web server to the user's tailnet while this process runs.
//!
//! `tailscale serve` without `--bg` keeps its configuration only while the command is running, so
//! the child process is the whole lifecycle: stopping or dropping it withdraws the publication and
//! leaves nothing behind in the Tailscale daemon. The web server itself never depends on Tailscale:
//! it listens locally either way, and the publication is established, and re-verified, whenever a
//! sign-in link for another device is wanted. A headless server, whose sign-in link is reachable
//! only through the publication, publishes at start and re-verifies on a timer instead.

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, ChildStderr, ChildStdout, Command},
    sync::Mutex,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// How long `tailscale serve` may run before it is considered to have started. It exits quickly
/// when it cannot publish, for example when Serve is not enabled for the tailnet.
const STARTUP_GRACE: Duration = Duration::from_secs(2);
/// How long one Tailscale status command may take. A healthy CLI answers in well under a second;
/// a hung daemon must neither stall startup nor push back the next re-verification, so a command
/// is killed after a third of the re-verification interval and the attempt counts as failed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a headless server re-verifies its publication.
const REVERIFY_INTERVAL: Duration = Duration::from_secs(30);
/// The longest wait between attempts while publishing keeps failing.
const RETRY_LIMIT: Duration = Duration::from_secs(60);
/// The most of a failed command's output that is shown to the user.
const OUTPUT_LIMIT: usize = 400;

/// Where the Tailscale command line lives: on the path, then in the macOS application bundle.
fn candidates() -> Vec<PathBuf> {
    let mut programs = vec![PathBuf::from("tailscale")];
    if cfg!(target_os = "macos") {
        programs.push(PathBuf::from(
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        ));
    }
    programs
}

#[derive(Debug, Error)]
pub(crate) enum TailscaleError {
    #[error("the tailscale command was not found; install Tailscale or unset web.tailscale")]
    NotInstalled,
    #[error("could not run tailscale: {0}")]
    Run(#[source] io::Error),
    #[error("tailscale status failed: {0}")]
    Status(String),
    #[error("tailscale is not connected (state: {0}); connect it, then try again")]
    NotRunning(String),
    #[error(
        "this tailnet has no HTTPS certificate domain; enable HTTPS in the Tailscale admin console"
    )]
    HttpsUnavailable,
    #[error("tailscale serve stopped: {0}")]
    Serve(String),
    #[error(
        "this machine's tailnet address is already being served by another interface (HTTPS port 443), \
         and only one can be shared at a time; stop it, or run \"tailscale serve reset\" if nothing is using it"
    )]
    AlreadyServed,
}

/// A running `tailscale serve` command.
struct Serve {
    child: Child,
    // The command's output pipes stay open so that it never writes into a closed pipe.
    _stdout: ChildStdout,
    _stderr: ChildStderr,
}

impl Serve {
    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

struct Inner {
    programs: Vec<PathBuf>,
    port: u16,
    grace: Duration,
    serve: Mutex<Option<Serve>>,
}

/// The server's publication to the tailnet. Clones share one publication, which is withdrawn when
/// [`Tailnet::stop`] is called or the last clone is dropped.
#[derive(Clone)]
pub(crate) struct Tailnet(Arc<Inner>);

impl fmt::Debug for Tailnet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Tailnet").finish_non_exhaustive()
    }
}

impl PartialEq for Tailnet {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Tailnet {}

impl Tailnet {
    /// A publication of the loopback `port`; nothing is published until [`Tailnet::origin`].
    pub(super) fn new(port: u16) -> Self {
        Self::with(candidates(), port, STARTUP_GRACE)
    }

    pub(super) fn with(programs: Vec<PathBuf>, port: u16, grace: Duration) -> Self {
        Self(Arc::new(Inner {
            programs,
            port,
            grace,
            serve: Mutex::new(None),
        }))
    }

    /// The HTTPS origin that reaches this server on the tailnet, publishing it first if needed.
    ///
    /// Tailscale is consulted on every call, so a client that was off, or whose `serve` command has
    /// since stopped, is picked up as soon as it is back; while it is off the publication is
    /// withdrawn and the error says what is wrong.
    pub(crate) async fn origin(&self) -> Result<String, TailscaleError> {
        let mut serve = self.0.serve.lock().await;
        let (program, host) = match status(&self.0.programs).await {
            Ok(found) => found,
            Err(error) => {
                *serve = None;
                return Err(error);
            }
        };
        if !serve.as_mut().is_some_and(Serve::running) {
            ensure_unserved(program).await?;
            *serve = Some(start_serve(program, self.0.port, self.0.grace).await?);
        }
        Ok(format!("https://{host}"))
    }

    /// Withdraws the publication.
    pub(super) async fn stop(&self) {
        *self.0.serve.lock().await = None;
    }

    /// Publishes now, then re-verifies the publication until `shutdown`, republishing whenever
    /// it was lost. Only a missing Tailscale is an error; every other failure is retried, and each
    /// change of state is reported once on stderr. Returns `None` when `shutdown` comes before
    /// the first attempt finishes; that attempt's commands are killed.
    pub(super) async fn keep_published(
        &self,
        shutdown: CancellationToken,
    ) -> Result<Option<JoinHandle<()>>, TailscaleError> {
        self.keep_published_every(REVERIFY_INTERVAL, RETRY_LIMIT, shutdown)
            .await
    }

    /// [`Tailnet::keep_published`] with an `interval` between checks that consecutive failures
    /// double up to `limit`.
    async fn keep_published_every(
        &self,
        interval: Duration,
        limit: Duration,
        shutdown: CancellationToken,
    ) -> Result<Option<JoinHandle<()>>, TailscaleError> {
        let first = tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            first = self.origin() => first,
        };
        if let Err(TailscaleError::NotInstalled) = first {
            return Err(TailscaleError::NotInstalled);
        }
        let tailnet = self.clone();
        Ok(Some(tokio::spawn(async move {
            let mut reported = report_publication(&first, None);
            let mut delay = interval;
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                let outcome = tokio::select! {
                    () = shutdown.cancelled() => return,
                    outcome = tailnet.origin() => outcome,
                };
                reported = report_publication(&outcome, Some(&reported));
                delay = if outcome.is_ok() {
                    interval
                } else {
                    delay.saturating_mul(2).min(limit)
                };
            }
        })))
    }
}

/// Prints the state `outcome` describes to stderr unless it equals `previous`, and returns it.
fn report_publication(outcome: &Result<String, TailscaleError>, previous: Option<&str>) -> String {
    let state = match outcome {
        Ok(origin) => format!("published to the tailnet at {origin}"),
        Err(error) => format!("not published to the tailnet: {error}"),
    };
    if previous != Some(state.as_str()) {
        eprintln!("tact: {state}");
    }
    state
}

/// Refuses to share while anything else, such as another Tact, is served on HTTPS port 443: the
/// tailnet address belongs to one interface at a time. A status that cannot be read is not a
/// conflict; `tailscale serve` itself then decides.
async fn ensure_unserved(program: &Path) -> Result<(), TailscaleError> {
    let Ok(output) = output(program, &["serve", "status", "--json"]).await else {
        return Ok(());
    };
    if output.status.success() && https_port_in_use(&output.stdout) {
        return Err(TailscaleError::AlreadyServed);
    }
    Ok(())
}

/// Whether the serve configuration, including commands still attached to a terminal, listens on 443.
fn https_port_in_use(json: &[u8]) -> bool {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Listeners {
        #[serde(rename = "TCP")]
        tcp: BTreeMap<String, serde_json::Value>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct ServeConfig {
        #[serde(flatten)]
        background: Listeners,
        #[serde(rename = "Foreground")]
        foreground: BTreeMap<String, Listeners>,
    }
    let Ok(config) = serde_json::from_slice::<ServeConfig>(json) else {
        return false;
    };
    std::iter::once(&config.background)
        .chain(config.foreground.values())
        .any(|listeners| listeners.tcp.contains_key("443"))
}

/// Starts publishing `port` on the loopback interface to the tailnet over HTTPS.
async fn start_serve(program: &Path, port: u16, grace: Duration) -> Result<Serve, TailscaleError> {
    let mut child = Command::new(program)
        .args(["serve", "--yes", "--https=443"])
        .arg(format!("http://127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(TailscaleError::Run)?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    if tokio::time::timeout(grace, child.wait()).await.is_ok() {
        let mut output = String::new();
        for pipe in [
            &mut stderr as &mut (dyn AsyncRead + Unpin + Send),
            &mut stdout,
        ] {
            let mut text = String::new();
            drop(
                pipe.take(OUTPUT_LIMIT as u64)
                    .read_to_string(&mut text)
                    .await,
            );
            output.push_str(text.trim());
            output.push(' ');
        }
        return Err(TailscaleError::Serve(output.trim().to_owned()));
    }
    Ok(Serve {
        child,
        _stdout: stdout,
        _stderr: stderr,
    })
}

/// This machine's HTTPS name, from `tailscale status --json` run by the first command that exists.
async fn status(programs: &[PathBuf]) -> Result<(&Path, String), TailscaleError> {
    for program in programs {
        match output(program, &["status", "--json"]).await {
            Ok(output) if output.status.success() => {
                return parse_status(&output.stdout).map(|host| (program.as_path(), host));
            }
            Ok(output) => {
                let message = String::from_utf8_lossy(&output.stderr);
                return Err(TailscaleError::Status(
                    message.trim().chars().take(OUTPUT_LIMIT).collect(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(TailscaleError::Run(error)),
        }
    }
    Err(TailscaleError::NotInstalled)
}

/// Runs `program` to completion, killing it when it has not finished within [`COMMAND_TIMEOUT`].
async fn output(program: &Path, arguments: &[&str]) -> io::Result<Output> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(COMMAND_TIMEOUT, output)
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer within {} s", COMMAND_TIMEOUT.as_secs()),
            ))
        })
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    backend_state: String,
    #[serde(default)]
    cert_domains: Option<Vec<String>>,
}

/// The name HTTPS is served under, from the output of `tailscale status --json`.
fn parse_status(json: &[u8]) -> Result<String, TailscaleError> {
    let status: Status =
        serde_json::from_slice(json).map_err(|error| TailscaleError::Status(error.to_string()))?;
    if status.backend_state != "Running" {
        return Err(TailscaleError::NotRunning(status.backend_state));
    }
    status
        .cert_domains
        .and_then(|domains| domains.into_iter().next())
        .map(|domain| domain.trim_end_matches('.').to_owned())
        .filter(|domain| !domain.is_empty())
        .ok_or(TailscaleError::HttpsUnavailable)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Duration, Tailnet, TailscaleError, https_port_in_use, parse_status};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };
    use tokio_util::sync::CancellationToken;

    const GRACE: Duration = Duration::from_millis(300);
    const RUNNING: &str = r#"{"BackendState":"Running","CertDomains":["box.tail1234.ts.net."]}"#;
    const STOPPED: &str = r#"{"BackendState":"Stopped","CertDomains":["box.tail1234.ts.net."]}"#;

    /// A stand-in `tailscale` in `directory`: `status` prints the contents of `status.json`, and
    /// `serve` runs `serve` (a shell snippet) with `serves.log` collecting one pid per start.
    fn fake(directory: &Path, status: &str, serve: &str) -> Tailnet {
        let program = directory.join("tailscale");
        fs::write(directory.join("status.json"), status).unwrap();
        fs::write(
            &program,
            format!(
                "#!/bin/sh\ncd {dir}\ncase \"$1\" in\nstatus) cat status.json ;;\nserve) if [ \"$2\" = status ]; then cat served.json 2>/dev/null || echo '{{}}'; else echo $$ >> serves.log; {serve}; fi ;;\nesac\n",
                dir = directory.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Tailnet::with(vec![program], 7878, GRACE)
    }

    fn serve_pids(directory: &Path) -> Vec<String> {
        fs::read_to_string(directory.join("serves.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .output()
            .unwrap()
            .status
            .success()
    }

    async fn stops(pid: &str) -> bool {
        for _ in 0..50 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[test]
    fn status_names_the_certificate_domain_without_its_trailing_dot() {
        assert_eq!(
            parse_status(RUNNING.as_bytes()).unwrap(),
            "box.tail1234.ts.net"
        );
    }

    #[test]
    fn status_rejects_a_disconnected_client_and_a_tailnet_without_https() {
        assert!(matches!(
            parse_status(STOPPED.as_bytes()),
            Err(TailscaleError::NotRunning(state)) if state == "Stopped"
        ));
        for json in [
            &br#"{"BackendState":"Running","CertDomains":[]}"#[..],
            br#"{"BackendState":"Running","CertDomains":null}"#,
            br#"{"BackendState":"Running"}"#,
        ] {
            assert!(matches!(
                parse_status(json),
                Err(TailscaleError::HttpsUnavailable)
            ));
        }
    }

    #[tokio::test]
    async fn the_origin_is_the_tailnet_name_and_one_serve_is_shared() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = fake(directory.path(), RUNNING, "exec sleep 30");

        assert_eq!(
            tailnet.origin().await.unwrap(),
            "https://box.tail1234.ts.net"
        );
        assert_eq!(
            tailnet.origin().await.unwrap(),
            "https://box.tail1234.ts.net"
        );

        assert_eq!(serve_pids(directory.path()).len(), 1);
    }

    #[tokio::test]
    async fn a_serve_that_exits_early_reports_its_output() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = Tailnet::with(
            fake(
                directory.path(),
                RUNNING,
                "echo 'Serve is not enabled' >&2; exit 1",
            )
            .0
            .programs
            .clone(),
            7878,
            Duration::from_secs(5),
        );

        let error = tailnet.origin().await.unwrap_err();

        assert!(
            matches!(&error, TailscaleError::Serve(output) if output.contains("Serve is not enabled")),
            "{error}"
        );
    }

    #[test]
    fn port_443_is_in_use_when_any_serve_listens_on_it() {
        for json in [
            r#"{"TCP":{"443":{"HTTPS":true}},"Web":{}}"#,
            r#"{"Foreground":{"abc":{"TCP":{"443":{"HTTPS":true}}}}}"#,
        ] {
            assert!(https_port_in_use(json.as_bytes()), "{json}");
        }
        for json in [
            "{}",
            "",
            "not json",
            r#"{"TCP":{"8443":{"HTTPS":true}}}"#,
            r#"{"Foreground":{"abc":{"TCP":{"8443":{"HTTPS":true}}}}}"#,
        ] {
            assert!(!https_port_in_use(json.as_bytes()), "{json}");
        }
    }

    #[tokio::test]
    async fn an_interface_already_served_on_the_tailnet_address_is_not_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = fake(directory.path(), RUNNING, "exec sleep 30");
        fs::write(
            directory.path().join("served.json"),
            r#"{"TCP":{"443":{"HTTPS":true}}}"#,
        )
        .unwrap();

        assert!(matches!(
            tailnet.origin().await,
            Err(TailscaleError::AlreadyServed)
        ));
        assert!(serve_pids(directory.path()).is_empty());

        fs::remove_file(directory.path().join("served.json")).unwrap();
        tailnet.origin().await.unwrap();
        assert_eq!(serve_pids(directory.path()).len(), 1);
    }

    #[tokio::test]
    async fn a_missing_command_is_reported_as_not_installed() {
        let tailnet = Tailnet::with(vec![PathBuf::from("/nonexistent/tailscale")], 7878, GRACE);

        assert!(matches!(
            tailnet.origin().await,
            Err(TailscaleError::NotInstalled)
        ));
    }

    #[tokio::test]
    async fn going_offline_withdraws_the_publication_and_coming_back_republishes() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = fake(directory.path(), RUNNING, "exec sleep 30");
        tailnet.origin().await.unwrap();
        let first = serve_pids(directory.path()).remove(0);

        fs::write(directory.path().join("status.json"), STOPPED).unwrap();
        assert!(matches!(
            tailnet.origin().await,
            Err(TailscaleError::NotRunning(_))
        ));
        assert!(stops(&first).await, "the offline publication is withdrawn");

        fs::write(directory.path().join("status.json"), RUNNING).unwrap();
        tailnet.origin().await.unwrap();
        assert_eq!(serve_pids(directory.path()).len(), 2);
    }

    #[tokio::test]
    async fn a_serve_that_died_is_started_again() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = fake(directory.path(), RUNNING, "exec sleep 30");
        tailnet.origin().await.unwrap();
        let first = serve_pids(directory.path()).remove(0);
        std::process::Command::new("kill")
            .arg(&first)
            .output()
            .unwrap();
        // The dead command is only noticed, and reaped, by the next check.
        tokio::time::sleep(Duration::from_millis(200)).await;

        tailnet.origin().await.unwrap();

        assert_eq!(serve_pids(directory.path()).len(), 2);
    }

    #[tokio::test]
    async fn stopping_and_dropping_both_end_serve() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = fake(directory.path(), RUNNING, "exec sleep 30");
        tailnet.origin().await.unwrap();
        let first = serve_pids(directory.path()).remove(0);

        tailnet.stop().await;
        assert!(stops(&first).await);

        tailnet.origin().await.unwrap();
        let second = serve_pids(directory.path()).remove(1);
        drop(tailnet);
        assert!(stops(&second).await);
    }

    #[tokio::test]
    async fn a_kept_publication_is_published_at_once_and_again_after_its_serve_dies() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = fake(directory.path(), RUNNING, "exec sleep 30");
        let shutdown = CancellationToken::new();

        let republisher = tailnet
            .keep_published_every(
                Duration::from_millis(100),
                Duration::from_millis(200),
                shutdown.clone(),
            )
            .await
            .unwrap()
            .expect("the first attempt finishes before shutdown");

        let first = serve_pids(directory.path()).remove(0);
        std::process::Command::new("kill")
            .arg(&first)
            .output()
            .unwrap();
        let mut republished = false;
        for _ in 0..50 {
            if serve_pids(directory.path()).len() == 2 {
                republished = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(republished, "the dead serve is started again");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), republisher)
            .await
            .unwrap()
            .unwrap();
        tailnet.stop().await;
        let second = serve_pids(directory.path()).remove(1);
        assert!(stops(&second).await);
    }

    #[tokio::test]
    async fn a_publication_that_fails_at_start_is_retried() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = fake(directory.path(), STOPPED, "exec sleep 30");
        let shutdown = CancellationToken::new();

        let republisher = tailnet
            .keep_published_every(
                Duration::from_millis(100),
                Duration::from_millis(200),
                shutdown.clone(),
            )
            .await
            .unwrap()
            .expect("the first attempt finishes before shutdown");
        assert!(serve_pids(directory.path()).is_empty());
        fs::write(directory.path().join("status.json"), RUNNING).unwrap();

        let mut published = false;
        for _ in 0..50 {
            if serve_pids(directory.path()).len() == 1 {
                published = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(published, "the publication follows Tailscale coming up");
        shutdown.cancel();
        republisher.await.unwrap();
    }

    #[tokio::test]
    async fn keeping_a_publication_requires_tailscale() {
        let tailnet = Tailnet::with(vec![PathBuf::from("/nonexistent/tailscale")], 7878, GRACE);

        let kept = tailnet.keep_published(CancellationToken::new()).await;

        assert!(matches!(kept, Err(TailscaleError::NotInstalled)));
    }

    /// A stand-in `tailscale` in `directory` whose every command records its pid in `hung.pid`
    /// and never answers.
    fn unresponsive(directory: &Path) -> Tailnet {
        let program = directory.join("tailscale");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\necho $$ > {}/hung.pid\nexec sleep 30\n",
                directory.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Tailnet::with(vec![program], 7878, GRACE)
    }

    #[tokio::test]
    async fn shutdown_ends_a_first_publication_that_tailscale_never_answers() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = unresponsive(directory.path());
        let shutdown = CancellationToken::new();
        let cancel = shutdown.clone();
        let pid_file = directory.path().join("hung.pid");
        let watched = pid_file.clone();
        tokio::spawn(async move {
            while !watched.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            cancel.cancel();
        });

        let kept = tokio::time::timeout(Duration::from_secs(5), tailnet.keep_published(shutdown))
            .await
            .expect("shutdown ends the first publication");

        assert!(matches!(kept, Ok(None)));
        let pid = fs::read_to_string(pid_file).unwrap();
        assert!(stops(pid.trim()).await, "the tailscale command is killed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_tailscale_command_that_never_answers_times_out() {
        let directory = tempfile::tempdir().unwrap();
        let tailnet = unresponsive(directory.path());

        let error = tailnet.origin().await.unwrap_err();

        assert!(
            matches!(&error, TailscaleError::Run(error) if error.kind() == std::io::ErrorKind::TimedOut),
            "{error}"
        );
    }
}
