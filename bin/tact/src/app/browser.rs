//! Platform browser launcher shared by authentication and the TUI.

use std::{io, process::Stdio};
use tokio::{io::AsyncReadExt, process::Command};

const STDERR_LIMIT: usize = 4096;

pub(crate) async fn open(url: &str) -> io::Result<()> {
    run(command(url)?).await
}

async fn run(mut command: Command) -> io::Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), "could not start browser launcher"))?;
    let mut stderr = child
        .stderr
        .take()
        .expect("browser launcher stderr is piped");

    // Drain the pipe after the bounded capture so the child can finish writing.
    let capture = async {
        let mut captured = Vec::with_capacity(STDERR_LIMIT);
        (&mut stderr)
            .take(STDERR_LIMIT as u64)
            .read_to_end(&mut captured)
            .await?;
        tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
        Ok::<_, io::Error>(captured)
    };
    let (status, captured) = tokio::join!(child.wait(), capture);
    let status = status
        .map_err(|error| io::Error::new(error.kind(), "could not wait for browser launcher"))?;
    let captured = captured
        .map_err(|error| io::Error::new(error.kind(), "could not read browser launcher stderr"))?;
    if status.success() {
        return Ok(());
    }

    // Launchers can echo login tokens to stderr. Only a fixed diagnostic is safe to display.
    let diagnostic = if captured
        .windows(b"failed with error -1712".len())
        .any(|part| part == b"failed with error -1712")
    {
        " (macOS launch timed out, error -1712)"
    } else {
        ""
    };
    Err(io::Error::other(format!(
        "browser launcher exited with {status}{diagnostic}"
    )))
}

fn command(url: &str) -> io::Result<Command> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "automatic browser launch is unsupported on this platform",
    ));

    command.arg(url);
    Ok(command)
}

#[cfg(all(test, unix))]
mod tests {
    use super::run;
    use tokio::process::Command;

    #[tokio::test]
    async fn waits_for_successful_launcher() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("finished");
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 0.05; echo finished > \"$1\"", "sh"]);
        command.arg(&marker);

        run(command).await.unwrap();
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn exit_error_never_echoes_url_or_stderr() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf '_LSOpenURLsWithCompletionHandler() failed with error -1712 for the URL %s\\n' \"$1\" >&2; exit 1",
            "sh",
            "https://example.test/login?token=private-auth-token",
        ]);

        let error = run(command).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("exit status: 1"), "{message}");
        assert!(message.contains("-1712"), "{message}");
        assert!(!message.contains("private-auth-token"));
        assert!(!message.contains("example.test"));
    }

    #[tokio::test]
    async fn drains_stderr_after_capture_limit() {
        let mut command = Command::new("sh");
        command.args(["-c", "yes long-error-message | head -n 1000 >&2; exit 2"]);

        let error = run(command).await.unwrap_err();
        assert!(error.to_string().contains("exit status: 2"));
    }
}
