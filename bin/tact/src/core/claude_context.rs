use chrono::Utc;
use nanocodex::NanocodexError;
use std::{
    env,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

const INSTRUCTION_BYTES: usize = 32 * 1024;
const CANDIDATES: [&str; 2] = ["AGENTS.override.md", "AGENTS.md"];

/// Loads workspace instructions before a fresh Claude conversation is persisted.
/// Global instructions have a separate 32 KiB budget; project files share 32 KiB from root to cwd.
/// Read failures abort context creation so a session cannot silently omit configured instructions.
pub(super) fn context(
    workspace: &Path,
    codex_home: Option<&Path>,
) -> Result<String, NanocodexError> {
    let workspace =
        fs::canonicalize(workspace).map_err(|source| NanocodexError::ResolveWorkspace {
            path: workspace.to_owned(),
            source,
        })?;
    if !workspace.is_dir() {
        return Err(NanocodexError::WorkspaceNotDirectory { path: workspace });
    }
    let cwd = workspace
        .to_str()
        .ok_or_else(|| NanocodexError::WorkspaceNotUtf8 {
            path: workspace.clone(),
        })?;
    let mut instructions = Vec::new();
    let mut notices = Vec::new();
    if let Some(home) = codex_home {
        for filename in CANDIDATES {
            let path = home.join(filename);
            if !is_file(&path)? {
                continue;
            }
            let text = read_instructions(&path, INSTRUCTION_BYTES, &mut notices)?;
            if !text.trim().is_empty() {
                instructions.push(text.trim().to_owned());
                break;
            }
        }
    }

    let root = project_root(&workspace)?;
    let mut directories = workspace
        .ancestors()
        .take_while(|directory| *directory != root)
        .collect::<Vec<_>>();
    directories.push(&root);
    let mut remaining = INSTRUCTION_BYTES;
    let mut project = Vec::new();
    for directory in directories.into_iter().rev() {
        if remaining == 0 {
            break;
        }
        for filename in CANDIDATES {
            let path = directory.join(filename);
            if !is_file(&path)? {
                continue;
            }
            let text = read_instructions(&path, remaining, &mut notices)?;
            if !text.trim().is_empty() {
                remaining -= text.len();
                project.push(text);
            }
            // The first existing project file owns this directory, even when it is empty.
            break;
        }
    }
    if !project.is_empty() {
        instructions.push(project.join("\n\n"));
    }
    let mut output = String::new();
    if !instructions.is_empty() {
        output.push_str(&format!(
            "# AGENTS.md instructions for {cwd}\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>\n\n",
            instructions.join("\n\n--- project-doc ---\n\n")
        ));
    }
    if remaining == 0 {
        notices
            .push("Project instruction budget exhausted; deeper files were not loaded.".to_owned());
    }
    if !notices.is_empty() {
        output.push_str("<instruction_loading_notices>\n");
        output.push_str(&notices.join("\n"));
        output.push_str("\n</instruction_loading_notices>\n\n");
    }
    let shell = env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "powershell"
            } else {
                "/bin/sh"
            }
            .to_owned()
        });
    output.push_str(&format!(
        "<environment_context>\n  <cwd>{}</cwd>\n  <shell>{}</shell>\n  <current_date>{}</current_date>\n  <timezone>UTC</timezone>\n</environment_context>",
        xml_text(cwd), xml_text(&shell), Utc::now().format("%Y-%m-%d")
    ));
    Ok(output)
}

fn project_root(workspace: &Path) -> Result<PathBuf, NanocodexError> {
    for directory in workspace.ancestors() {
        let path = directory.join(".git");
        match fs::metadata(&path) {
            Ok(_) => return Ok(directory.to_owned()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(read_error(&path, source)),
        }
    }
    Ok(workspace.to_owned())
}

fn is_file(path: &Path) -> Result<bool, NanocodexError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(read_error(path, source)),
    }
}

fn read_instructions(
    path: &Path,
    limit: usize,
    notices: &mut Vec<String>,
) -> Result<String, NanocodexError> {
    let file = File::open(path).map_err(|source| read_error(path, source))?;
    let mut bytes = Vec::with_capacity((limit + 1).min(8192));
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| read_error(path, source))?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    // A byte budget may split the last UTF-8 character; retain only complete characters.
    if let Err(error) = std::str::from_utf8(&bytes) {
        if truncated && error.error_len().is_none() {
            bytes.truncate(error.valid_up_to());
        } else {
            return Err(read_error(
                path,
                io::Error::new(io::ErrorKind::InvalidData, error),
            ));
        }
    }
    if truncated {
        notices.push(format!(
            "Instructions at {} were truncated to the remaining {limit}-byte budget.",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map_err(|source| read_error(path, io::Error::new(io::ErrorKind::InvalidData, source)))
}

fn read_error(path: &Path, source: io::Error) -> NanocodexError {
    NanocodexError::InvalidRequest(format!(
        "could not load instructions at {}: {source}",
        path.display()
    ))
}

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn global_override_and_root_to_leaf_project_order() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("AGENTS.md"), "unused-global").unwrap();
        fs::write(home.path().join("AGENTS.override.md"), " global ").unwrap();
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join("AGENTS.md"), "root-doc").unwrap();
        let leaf = repo.path().join("child/leaf");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(repo.path().join("child/AGENTS.md"), "child-doc").unwrap();
        fs::write(leaf.join("AGENTS.md"), "unused-leaf").unwrap();
        fs::write(leaf.join("AGENTS.override.md"), "leaf-doc").unwrap();
        let output = context(&leaf, Some(home.path())).unwrap();
        assert!(
            output.contains("global\n\n--- project-doc ---\n\nroot-doc\n\nchild-doc\n\nleaf-doc")
        );
        assert!(!output.contains("unused"));
        assert!(output.contains(&format!(
            "<cwd>{}</cwd>",
            fs::canonicalize(leaf).unwrap().display()
        )));
        assert!(output.contains("<timezone>UTC</timezone>"));
        assert!(output.contains("<shell>"));
    }

    #[test]
    fn empty_global_override_falls_back_but_empty_project_override_suppresses() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        for directory in [home.path(), workspace.path()] {
            fs::write(directory.join("AGENTS.override.md"), " \n").unwrap();
            fs::write(directory.join("AGENTS.md"), "fallback").unwrap();
        }
        let output = context(workspace.path(), Some(home.path())).unwrap();
        assert_eq!(output.matches("fallback").count(), 1);
        assert!(!output.contains("project-doc"));
    }

    #[test]
    fn no_repository_loads_only_cwd_and_nearest_repository_stops_ancestors() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("AGENTS.md"), "outside-doc").unwrap();
        let nested = directory.path().join("nested");
        let cwd = nested.join("leaf");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(nested.join("AGENTS.md"), "nested-doc").unwrap();
        fs::write(cwd.join("AGENTS.md"), "cwd-doc").unwrap();
        let output = context(&cwd, None).unwrap();
        assert!(!output.contains("outside-doc"));
        assert!(!output.contains("nested-doc"));
        assert!(output.contains("cwd-doc"));
        fs::create_dir(directory.path().join(".git")).unwrap();
        fs::write(nested.join(".git"), "gitdir: elsewhere").unwrap();
        let output = context(&cwd, None).unwrap();
        assert!(output.contains("nested-doc\n\ncwd-doc"));
        assert!(!output.contains("outside-doc"));
    }

    #[test]
    fn global_and_project_budgets_are_separate_and_project_is_shared() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("AGENTS.md"),
            "g".repeat(INSTRUCTION_BYTES + 10),
        )
        .unwrap();
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::write(
            repo.path().join("AGENTS.md"),
            "r".repeat(INSTRUCTION_BYTES - 4),
        )
        .unwrap();
        let cwd = repo.path().join("child");
        fs::create_dir(&cwd).unwrap();
        fs::write(cwd.join("AGENTS.md"), "leaf-over-budget").unwrap();
        let output = context(&cwd, Some(home.path())).unwrap();
        assert!(output.contains(&"g".repeat(INSTRUCTION_BYTES)));
        assert!(!output.contains(&"g".repeat(INSTRUCTION_BYTES + 1)));
        assert!(output.contains(&format!(
            "{}\n\nleaf\n</INSTRUCTIONS>",
            "r".repeat(INSTRUCTION_BYTES - 4)
        )));
        assert!(output.contains("truncated"));
    }

    #[test]
    fn empty_project_docs_leave_budget_for_descendants() {
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join("AGENTS.md"), " ".repeat(INSTRUCTION_BYTES)).unwrap();
        let cwd = repo.path().join("child");
        fs::create_dir(&cwd).unwrap();
        fs::write(cwd.join("AGENTS.md"), "child-doc").unwrap();
        assert!(context(&cwd, None).unwrap().contains("child-doc"));
    }

    #[cfg(unix)]
    #[test]
    fn instruction_read_errors_are_reported_with_the_path() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let path = directory.path().join("AGENTS.override.md");
        symlink("AGENTS.override.md", &path).unwrap();
        fs::write(directory.path().join("AGENTS.md"), "fallback").unwrap();
        for home in [None, Some(directory.path())] {
            let error = context(directory.path(), home).unwrap_err();
            assert!(error.to_string().contains(path.to_str().unwrap()));
        }
    }

    #[test]
    fn invalid_utf8_fails_and_budget_boundary_keeps_complete_characters() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("AGENTS.md");
        fs::write(&path, b"invalid\xff").unwrap();
        assert!(
            context(directory.path(), None)
                .unwrap_err()
                .to_string()
                .contains("AGENTS.md")
        );
        fs::write(&path, "aéz").unwrap();
        let mut notices = Vec::new();
        assert_eq!(read_instructions(&path, 2, &mut notices).unwrap(), "a");
        assert_eq!(notices.len(), 1);
    }
}
