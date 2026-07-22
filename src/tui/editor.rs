//! Asynchronous external-editor handoff.

use crate::error::ExternalEditorError;
use std::{
    env, fs,
    path::Path,
    process::{ExitStatus, Stdio},
};
use tempfile::Builder;
use tokio::process::Command;

#[derive(Debug)]
pub(crate) enum EditorOutcome {
    Updated(String),
    Unchanged,
}

pub(crate) async fn edit(
    seed: &str,
    workspace: &Path,
) -> Result<EditorOutcome, ExternalEditorError> {
    let editor = resolve_editor_command()?;
    edit_with(seed, workspace, &editor).await
}

pub(crate) async fn edit_config(path: &Path, workspace: &Path) -> Result<(), ExternalEditorError> {
    let editor = resolve_editor_command()?;
    edit_config_with(path, workspace, &editor).await
}

pub(crate) async fn open_file(path: &Path, workspace: &Path) -> Result<(), ExternalEditorError> {
    let editor = resolve_editor_command()?;
    let _ = launch(&editor, path, workspace).await?;
    Ok(())
}

fn resolve_editor_command() -> Result<Vec<String>, ExternalEditorError> {
    let raw = env::var("EDITOR").map_err(ExternalEditorError::Unavailable)?;
    parse_editor_command(&raw)
}

#[cfg(not(windows))]
fn parse_editor_command(raw: &str) -> Result<Vec<String>, ExternalEditorError> {
    let command = shlex::split(raw).ok_or_else(|| ExternalEditorError::Parse {
        command: raw.to_owned(),
    })?;
    if command.is_empty() {
        return Err(ExternalEditorError::Parse {
            command: raw.to_owned(),
        });
    }
    Ok(command)
}

#[cfg(windows)]
fn parse_editor_command(raw: &str) -> Result<Vec<String>, ExternalEditorError> {
    if raw.trim().is_empty() {
        return Err(ExternalEditorError::Parse {
            command: raw.to_owned(),
        });
    }
    Ok(vec![raw.to_owned()])
}

async fn edit_with(
    seed: &str,
    workspace: &Path,
    editor: &[String],
) -> Result<EditorOutcome, ExternalEditorError> {
    let draft = Builder::new()
        .prefix("tact-draft-")
        .suffix(".md")
        .tempfile()
        .map_err(ExternalEditorError::CreateDraft)?;
    fs::write(draft.path(), seed).map_err(ExternalEditorError::WriteDraft)?;

    let status = launch(editor, draft.path(), workspace).await?;
    if !status.success() {
        return Ok(EditorOutcome::Unchanged);
    }

    let draft = fs::read_to_string(draft.path()).map_err(ExternalEditorError::ReadDraft)?;
    Ok(EditorOutcome::Updated(remove_one_trailing_newline(draft)))
}

async fn edit_config_with(
    path: &Path,
    workspace: &Path,
    editor: &[String],
) -> Result<(), ExternalEditorError> {
    let _ = launch(editor, path, workspace).await?;
    Ok(())
}

async fn launch(
    editor: &[String],
    path: &Path,
    workspace: &Path,
) -> Result<ExitStatus, ExternalEditorError> {
    let (program, arguments) = editor
        .split_first()
        .ok_or_else(|| ExternalEditorError::Parse {
            command: String::new(),
        })?;
    Command::new(program)
        .args(arguments)
        .arg(path)
        .current_dir(workspace)
        .env("TACT_EXTERNAL_EDITOR", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|source| ExternalEditorError::Launch {
            program: program.clone(),
            source,
        })
}

fn remove_one_trailing_newline(mut draft: String) -> String {
    if draft.ends_with("\r\n") {
        draft.truncate(draft.len() - 2);
    } else if draft.ends_with('\n') {
        draft.pop();
    }
    draft
}

#[cfg(test)]
mod tests {
    use super::{
        EditorOutcome, edit_config_with, edit_with, parse_editor_command,
        remove_one_trailing_newline,
    };
    use crate::error::ExternalEditorError;
    use std::path::Path;

    #[test]
    #[cfg(not(windows))]
    fn editor_arguments_preserve_shell_quoting() {
        assert_eq!(
            parse_editor_command("nvim -c 'set spell'").unwrap(),
            ["nvim", "-c", "set spell"]
        );
        assert!(parse_editor_command("nvim '").is_err());
        assert!(parse_editor_command("  ").is_err());
    }

    #[test]
    fn exactly_one_trailing_newline_is_removed() {
        assert_eq!(
            remove_one_trailing_newline("draft\n\n".to_owned()),
            "draft\n"
        );
        assert_eq!(remove_one_trailing_newline("draft\r\n".to_owned()), "draft");
        assert_eq!(remove_one_trailing_newline("draft".to_owned()), "draft");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn successful_editor_replaces_the_draft() {
        let command = [
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "test \"$TACT_EXTERNAL_EDITOR\" = 1 && printf 'edited\\n' > \"$1\"".to_owned(),
            "tact-editor".to_owned(),
        ];

        let outcome = edit_with("seed", Path::new("."), &command).await.unwrap();
        assert!(matches!(outcome, EditorOutcome::Updated(draft) if draft == "edited"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn failed_editor_preserves_the_draft() {
        let command = ["/bin/sh".to_owned(), "-c".to_owned(), "exit 2".to_owned()];

        let outcome = edit_with("seed", Path::new("."), &command).await.unwrap();
        assert!(matches!(outcome, EditorOutcome::Unchanged));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn config_editor_receives_the_selected_config_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selected-config.toml");
        let command = [
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf 'edited = true\n' > \"$1\"".to_owned(),
            "tact-editor".to_owned(),
        ];

        edit_config_with(&path, Path::new("."), &command)
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "edited = true\n");
    }

    #[tokio::test]
    async fn unlaunchable_editor_has_a_typed_diagnostic() {
        let command = ["/definitely/missing/tact-editor".to_owned()];

        let error = edit_with("seed", Path::new("."), &command)
            .await
            .unwrap_err();

        assert!(matches!(error, ExternalEditorError::Launch { .. }));
    }
}
