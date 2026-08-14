//! User-configured lifecycle hooks.

#[cfg(unix)]
use std::env;
use std::{
    io,
    path::Path,
    process::{ExitStatus, Stdio},
};
use tokio::process::Command;

pub(crate) async fn execute(command: &str, workspace: &Path) -> io::Result<ExitStatus> {
    let mut process = shell_command(command);
    process
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let shell = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut process = Command::new(shell);
    process.args(["-lc", command]);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.args(["/C", command]);
    process
}

#[cfg(test)]
mod tests {
    use super::execute;
    use tempfile::tempdir;

    #[tokio::test]
    async fn executes_completion_hook_in_the_workspace() {
        let workspace = tempdir().unwrap();
        #[cfg(unix)]
        let command = "printf completed > completion.txt";
        #[cfg(windows)]
        let command = "echo completed> completion.txt";

        let status = execute(command, workspace.path()).await.unwrap();

        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("completion.txt")).unwrap(),
            "completed"
        );
    }
}
