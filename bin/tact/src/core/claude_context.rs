//! Workspace instructions for Claude sessions, following the AGENTS.md conventions.
//!
//! Instructions come from one global file in the Codex home and from project files on the path
//! from the repository root (the nearest ancestor holding `.git`) down to the workspace. In each
//! directory `AGENTS.override.md` takes precedence over `AGENTS.md`. Global instructions have
//! their own byte budget; project files share a second budget from root to leaf, and deeper files
//! are dropped once it is exhausted. Read failures abort loading so a session never silently
//! omits configured instructions.

use chrono::Utc;
use nanocodex::NanocodexError;
use std::{
    env,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};
use thiserror::Error;

const INSTRUCTION_BYTES: usize = 32 * 1024;
const CANDIDATES: [&str; 2] = ["AGENTS.override.md", "AGENTS.md"];

#[derive(Debug, Error)]
pub(super) enum ContextError {
    #[error(transparent)]
    Workspace(NanocodexError),
    #[error("could not load instructions at {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl From<ContextError> for NanocodexError {
    fn from(error: ContextError) -> Self {
        match error {
            ContextError::Workspace(error) => error,
            error @ ContextError::Read { .. } => Self::InvalidRequest(error.to_string()),
        }
    }
}

/// Something the model should know about how its instructions were loaded.
#[derive(Debug, Eq, PartialEq)]
enum Notice {
    Truncated { path: PathBuf, limit: usize },
    ProjectBudgetExhausted,
}

/// The instructions that apply to one workspace, in the order they are presented.
#[derive(Debug)]
pub(super) struct ProjectContext {
    workspace: String,
    global: Option<String>,
    /// Non-empty project files from the repository root down to the workspace.
    project: Vec<String>,
    notices: Vec<Notice>,
}

impl ProjectContext {
    /// Loads workspace instructions before a fresh Claude conversation is persisted.
    pub(super) fn load(workspace: &Path, codex_home: Option<&Path>) -> Result<Self, ContextError> {
        let workspace = fs::canonicalize(workspace).map_err(|source| {
            ContextError::Workspace(NanocodexError::ResolveWorkspace {
                path: workspace.to_owned(),
                source,
            })
        })?;
        if !workspace.is_dir() {
            return Err(ContextError::Workspace(
                NanocodexError::WorkspaceNotDirectory { path: workspace },
            ));
        }
        let Some(cwd) = workspace.to_str().map(str::to_owned) else {
            return Err(ContextError::Workspace(NanocodexError::WorkspaceNotUtf8 {
                path: workspace,
            }));
        };
        let mut notices = Vec::new();
        let mut global = None;
        if let Some(home) = codex_home {
            for filename in CANDIDATES {
                let path = home.join(filename);
                if !is_file(&path)? {
                    continue;
                }
                let text = read_instructions(&path, INSTRUCTION_BYTES, &mut notices)?;
                if !text.trim().is_empty() {
                    global = Some(text.trim().to_owned());
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
        if remaining == 0 {
            notices.push(Notice::ProjectBudgetExhausted);
        }
        Ok(Self {
            workspace: cwd,
            global,
            project,
            notices,
        })
    }

    /// Renders the context block appended to a Claude system prompt.
    pub(super) fn render(&self) -> String {
        let mut instructions = Vec::new();
        if let Some(global) = &self.global {
            instructions.push(global.clone());
        }
        if !self.project.is_empty() {
            instructions.push(self.project.join("\n\n"));
        }
        let mut output = String::new();
        if !instructions.is_empty() {
            output.push_str(&format!(
                "# AGENTS.md instructions for {}\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>\n\n",
                self.workspace,
                instructions.join("\n\n--- project-doc ---\n\n")
            ));
        }
        if !self.notices.is_empty() {
            output.push_str("<instruction_loading_notices>\n");
            let notices = self.notices.iter().map(Notice::render).collect::<Vec<_>>();
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
            xml_text(&self.workspace),
            xml_text(&shell),
            Utc::now().format("%Y-%m-%d")
        ));
        output
    }
}

impl Notice {
    fn render(&self) -> String {
        match self {
            Self::Truncated { path, limit } => format!(
                "Instructions at {} were truncated to the remaining {limit}-byte budget.",
                path.display()
            ),
            Self::ProjectBudgetExhausted => {
                "Project instruction budget exhausted; deeper files were not loaded.".to_owned()
            }
        }
    }
}

fn project_root(workspace: &Path) -> Result<PathBuf, ContextError> {
    for directory in workspace.ancestors() {
        let path = directory.join(".git");
        match fs::metadata(&path) {
            Ok(_) => return Ok(directory.to_owned()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(ContextError::Read { path, source }),
        }
    }
    Ok(workspace.to_owned())
}

fn is_file(path: &Path) -> Result<bool, ContextError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(read_error(path, source)),
    }
}

fn read_instructions(
    path: &Path,
    limit: usize,
    notices: &mut Vec<Notice>,
) -> Result<String, ContextError> {
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
        notices.push(Notice::Truncated {
            path: path.to_owned(),
            limit,
        });
    }
    String::from_utf8(bytes)
        .map_err(|source| read_error(path, io::Error::new(io::ErrorKind::InvalidData, source)))
}

fn read_error(path: &Path, source: io::Error) -> ContextError {
    ContextError::Read {
        path: path.to_owned(),
        source,
    }
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

        let context = ProjectContext::load(&leaf, Some(home.path())).unwrap();

        assert_eq!(context.global.as_deref(), Some("global"));
        assert_eq!(context.project, ["root-doc", "child-doc", "leaf-doc"]);
        assert!(context.notices.is_empty());
        assert_eq!(
            context.workspace,
            fs::canonicalize(leaf).unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn rendering_presents_instructions_notices_and_environment() {
        let context = ProjectContext {
            workspace: "/work/<a&b>".to_owned(),
            global: Some("global".to_owned()),
            project: vec!["root-doc".to_owned(), "leaf-doc".to_owned()],
            notices: vec![Notice::ProjectBudgetExhausted],
        };

        let rendered = context.render();
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let (prefix, environment) = rendered.split_once("<environment_context>").unwrap();

        assert_eq!(
            prefix,
            "# AGENTS.md instructions for /work/<a&b>\n\n<INSTRUCTIONS>\nglobal\n\n--- project-doc ---\n\nroot-doc\n\nleaf-doc\n</INSTRUCTIONS>\n\n<instruction_loading_notices>\nProject instruction budget exhausted; deeper files were not loaded.\n</instruction_loading_notices>\n\n"
        );
        assert!(environment.starts_with("\n  <cwd>/work/&lt;a&amp;b&gt;</cwd>\n  <shell>"));
        assert!(environment.ends_with(&format!(
            "</shell>\n  <current_date>{date}</current_date>\n  <timezone>UTC</timezone>\n</environment_context>"
        )));
    }

    #[test]
    fn rendering_without_instructions_has_only_the_environment() {
        let context = ProjectContext {
            workspace: "/work".to_owned(),
            global: None,
            project: Vec::new(),
            notices: Vec::new(),
        };

        assert!(context.render().starts_with("<environment_context>"));
    }

    #[test]
    fn empty_global_override_falls_back_but_empty_project_override_suppresses() {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        for directory in [home.path(), workspace.path()] {
            fs::write(directory.join("AGENTS.override.md"), " \n").unwrap();
            fs::write(directory.join("AGENTS.md"), "fallback").unwrap();
        }

        let context = ProjectContext::load(workspace.path(), Some(home.path())).unwrap();

        assert_eq!(context.global.as_deref(), Some("fallback"));
        assert!(context.project.is_empty());
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
        assert_eq!(
            ProjectContext::load(&cwd, None).unwrap().project,
            ["cwd-doc"]
        );

        fs::create_dir(directory.path().join(".git")).unwrap();
        fs::write(nested.join(".git"), "gitdir: elsewhere").unwrap();
        assert_eq!(
            ProjectContext::load(&cwd, None).unwrap().project,
            ["nested-doc", "cwd-doc"]
        );
    }

    #[test]
    fn global_and_project_budgets_are_separate_and_project_is_shared() {
        let home = tempdir().unwrap();
        let global_path = home.path().join("AGENTS.md");
        fs::write(&global_path, "g".repeat(INSTRUCTION_BYTES + 10)).unwrap();
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::write(
            repo.path().join("AGENTS.md"),
            "r".repeat(INSTRUCTION_BYTES - 4),
        )
        .unwrap();
        let cwd = repo.path().join("child");
        fs::create_dir(&cwd).unwrap();
        let leaf_path = cwd.join("AGENTS.md");
        fs::write(&leaf_path, "leaf-over-budget").unwrap();

        let context = ProjectContext::load(&cwd, Some(home.path())).unwrap();

        assert_eq!(context.global, Some("g".repeat(INSTRUCTION_BYTES)));
        assert_eq!(
            context.project,
            ["r".repeat(INSTRUCTION_BYTES - 4), "leaf".to_owned()]
        );
        let leaf_path = fs::canonicalize(&cwd).unwrap().join("AGENTS.md");
        assert_eq!(
            context.notices,
            [
                Notice::Truncated {
                    path: global_path,
                    limit: INSTRUCTION_BYTES,
                },
                Notice::Truncated {
                    path: leaf_path,
                    limit: 4,
                },
                Notice::ProjectBudgetExhausted,
            ]
        );
    }

    #[test]
    fn empty_project_docs_leave_budget_for_descendants() {
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join("AGENTS.md"), " ".repeat(INSTRUCTION_BYTES)).unwrap();
        let cwd = repo.path().join("child");
        fs::create_dir(&cwd).unwrap();
        fs::write(cwd.join("AGENTS.md"), "child-doc").unwrap();

        assert_eq!(
            ProjectContext::load(&cwd, None).unwrap().project,
            ["child-doc"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn instruction_read_errors_identify_the_path() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let path = directory.path().join("AGENTS.override.md");
        symlink("AGENTS.override.md", &path).unwrap();
        fs::write(directory.path().join("AGENTS.md"), "fallback").unwrap();
        let workspace = fs::canonicalize(directory.path()).unwrap();
        for home in [None, Some(directory.path())] {
            let error = ProjectContext::load(directory.path(), home).unwrap_err();
            let ContextError::Read { path: failed, .. } = error else {
                panic!("expected a read error, got {error:?}");
            };
            assert!(
                [path.clone(), workspace.join("AGENTS.override.md")].contains(&failed),
                "{failed:?}"
            );
        }
    }

    #[test]
    fn invalid_utf8_fails_and_budget_boundary_keeps_complete_characters() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("AGENTS.md");
        fs::write(&path, b"invalid\xff").unwrap();
        let error = ProjectContext::load(directory.path(), None).unwrap_err();
        assert!(matches!(
            error,
            ContextError::Read { path, source }
                if path.ends_with("AGENTS.md") && source.kind() == io::ErrorKind::InvalidData
        ));

        fs::write(&path, "aéz").unwrap();
        let mut notices = Vec::new();
        assert_eq!(read_instructions(&path, 2, &mut notices).unwrap(), "a");
        assert_eq!(notices, [Notice::Truncated { path, limit: 2 }]);
    }
}
