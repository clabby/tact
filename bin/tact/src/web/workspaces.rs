//! The checkouts a client may start sessions in and review.
//!
//! Every checkout of the repositories behind the live sessions is addressable; nothing else is. A
//! request names a checkout by path, and the path must resolve to one of those checkouts, so a
//! client cannot point the review at an arbitrary directory.

use super::hub::Hub;
use crate::vcs::{Checkout, CheckoutKind, FamilyMember};
use futures_util::future::join_all;
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;

/// How long a repository's list of checkouts is reused. Checkouts come and go rarely, and the
/// review polls frequently.
const FAMILY_TTL: Duration = Duration::from_secs(5);
/// The most checkouts counted and listed for one repository.
const MAX_LISTED: usize = 24;
/// The most other workspaces offered besides the repository's own checkouts.
const RECENT_LIMIT: usize = 5;
/// How long counting one checkout's changed files may take before the count is left out.
const COUNT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Error)]
pub(super) enum WorkspaceError {
    #[error("unknown session")]
    UnknownSession,
    #[error("{0} is not a checkout of this repository")]
    NotACheckout(PathBuf),
}

/// A checkout a request resolved to.
#[derive(Clone, Debug)]
pub(super) struct Target {
    /// The canonical path of the checkout.
    pub(super) path: PathBuf,
    pub(super) name: String,
    pub(super) label: String,
    pub(super) kind: CheckoutKind,
    /// The directory the session's agent runs in. The review labels a target that differs.
    pub(super) session_workspace: PathBuf,
}

/// One checkout as the workspaces query lists it.
#[derive(Serialize)]
pub(super) struct CheckoutEntry {
    path: PathBuf,
    name: String,
    label: String,
    kind: CheckoutKind,
    head: Option<String>,
    changed_files: Option<usize>,
    /// The session's workspace, or the default workspace when no session is named.
    current: bool,
    missing: bool,
    /// The session's recent tool calls refer to a path inside this checkout.
    touched: bool,
}

#[derive(Serialize)]
pub(super) struct WorkspacesReply {
    default: PathBuf,
    checkouts: Vec<CheckoutEntry>,
    recent: Vec<PathBuf>,
}

/// A repository's checkouts and when they were listed.
type CachedFamily = (Instant, Arc<Vec<FamilyMember>>);

pub(super) struct Workspaces {
    default: PathBuf,
    hub: Hub,
    families: Mutex<HashMap<PathBuf, CachedFamily>>,
}

impl Workspaces {
    pub(super) fn new(default: PathBuf, hub: Hub) -> Self {
        Self {
            default,
            hub,
            families: Mutex::new(HashMap::new()),
        }
    }

    /// The directory a request without an explicit checkout refers to.
    pub(super) fn session_workspace(
        &self,
        session: Option<&str>,
    ) -> Result<PathBuf, WorkspaceError> {
        match session {
            Some(session) => self
                .hub
                .session_workspace(session)
                .ok_or(WorkspaceError::UnknownSession),
            None => Ok(self.default.clone()),
        }
    }

    /// The checkouts of the repository `workspace` belongs to, always including `workspace`.
    async fn family(&self, workspace: &Path) -> Arc<Vec<FamilyMember>> {
        let now = Instant::now();
        if let Some((at, members)) = self.families.lock().unwrap().get(workspace)
            && now.duration_since(*at) < FAMILY_TTL
        {
            return Arc::clone(members);
        }
        let mut members = match Checkout::detect(workspace).await {
            Ok(checkout) => checkout.family().await,
            Err(_) => Vec::new(),
        };
        members.truncate(MAX_LISTED);
        if !members.iter().any(|member| member.path == workspace) {
            members.push(FamilyMember::standalone(workspace));
        }
        let members = Arc::new(members);
        self.families
            .lock()
            .unwrap()
            .insert(workspace.to_owned(), (now, Arc::clone(&members)));
        members
    }

    /// Resolves the checkout a request names, or the session's workspace when it names none.
    pub(super) async fn resolve(
        &self,
        session: Option<&str>,
        checkout: Option<&str>,
    ) -> Result<Target, WorkspaceError> {
        let session_workspace = canonical(&self.session_workspace(session)?).await;
        let requested = match checkout {
            Some(path) => canonical(Path::new(path)).await,
            None => session_workspace.clone(),
        };
        let mut sources = vec![session_workspace.clone(), self.default.clone()];
        sources.extend(self.hub.live_workspaces());
        for source in sources {
            for member in self.family(&source).await.iter() {
                if canonical(&member.path).await == requested {
                    return Ok(Target::new(requested, member, session_workspace));
                }
            }
        }
        if requested == session_workspace {
            return Ok(Target::new(
                requested,
                &FamilyMember::standalone(&session_workspace),
                session_workspace,
            ));
        }
        Err(WorkspaceError::NotACheckout(requested))
    }

    /// Whether `path` may be given to a new session: a checkout of the default workspace's
    /// repository or of a live session's.
    pub(super) async fn startable(&self, path: &Path) -> Result<PathBuf, WorkspaceError> {
        let requested = canonical(path).await;
        let mut sources = vec![self.default.clone()];
        sources.extend(self.hub.live_workspaces());
        for source in sources {
            for member in self.family(&source).await.iter() {
                if canonical(&member.path).await == requested {
                    return Ok(requested);
                }
            }
        }
        Err(WorkspaceError::NotACheckout(requested))
    }

    pub(super) async fn list(
        &self,
        session: Option<&str>,
    ) -> Result<WorkspacesReply, WorkspaceError> {
        let workspace = canonical(&self.session_workspace(session)?).await;
        let family = self.family(&workspace).await;
        let tools = session.and_then(|session| self.hub.recent_tool_arguments(session));
        let counts = join_all(family.iter().map(|member| async move {
            if member.missing {
                return None;
            }
            tokio::time::timeout(COUNT_TIMEOUT, async {
                Checkout::detect(&member.path)
                    .await
                    .ok()?
                    .changed_files()
                    .await
            })
            .await
            .ok()
            .flatten()
        }))
        .await;
        let mut current = None;
        for (index, member) in family.iter().enumerate() {
            let path = canonical(&member.path).await;
            if path == workspace {
                current = Some(index);
                break;
            }
            // A workspace below a checkout's root belongs to that checkout.
            if workspace.starts_with(&path)
                && current.is_none_or(|known: usize| {
                    family[known].path.components().count() < member.path.components().count()
                })
            {
                current = Some(index);
            }
        }
        let checkouts = family
            .iter()
            .zip(counts)
            .enumerate()
            // A checkout whose directory is gone cannot be opened or reviewed, so only the
            // session's own is kept, to say that it vanished.
            .filter(|(index, (member, _))| !member.missing || current == Some(*index))
            .map(|(index, (member, changed_files))| CheckoutEntry {
                name: member
                    .path
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                label: member.label.clone(),
                kind: member.kind,
                head: None,
                changed_files,
                current: current == Some(index),
                missing: member.missing,
                touched: tools
                    .as_deref()
                    .is_some_and(|text| mentions_checkout(text, &member.path, &workspace)),
                path: member.path.clone(),
            })
            .collect::<Vec<_>>();
        let mut recent = Vec::new();
        for path in self.hub.live_workspaces() {
            let path = canonical(&path).await;
            let known = family.iter().any(|member| member.path == path);
            if !known && !recent.contains(&path) && recent.len() < RECENT_LIMIT {
                recent.push(path);
            }
        }
        Ok(WorkspacesReply {
            default: self.default.clone(),
            checkouts,
            recent,
        })
    }
}

impl Target {
    fn new(path: PathBuf, member: &FamilyMember, session_workspace: PathBuf) -> Self {
        Self {
            name: path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            path,
            label: member.label.clone(),
            kind: member.kind,
            session_workspace,
        }
    }
}

async fn canonical(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_owned())
}

/// Whether `text` names `checkout` as an absolute path, or as a sibling path relative to
/// `from`, such as `../project-feature`.
fn mentions_checkout(text: &str, checkout: &Path, from: &Path) -> bool {
    let absolute = checkout.to_string_lossy();
    let mut forms = vec![absolute.into_owned()];
    if let (Some(parent), Some(name)) = (from.parent(), checkout.file_name())
        && checkout.parent() == Some(parent)
    {
        forms.push(format!("../{}", name.to_string_lossy()));
    }
    forms.iter().any(|form| {
        text.match_indices(form.as_str()).any(|(start, _)| {
            let before = text[..start].chars().next_back();
            let after = text[start + form.len()..].chars().next();
            let inside = |character: char| {
                character.is_alphanumeric() || matches!(character, '-' | '_' | '.')
            };
            !before.is_some_and(|c| inside(c) || c == '/') && !after.is_some_and(inside)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::mentions_checkout;
    use std::path::Path;

    #[test]
    fn a_checkout_is_mentioned_only_by_its_whole_path() {
        let text =
            r#"{"workdir":"/work/tact-ws2","cmd":"cd ../tact-feature && ls /work/tact/src"}"#;
        let from = Path::new("/work/tact");

        assert!(mentions_checkout(text, Path::new("/work/tact-ws2"), from));
        assert!(mentions_checkout(text, Path::new("/work/tact"), from));
        assert!(mentions_checkout(
            text,
            Path::new("/work/tact-feature"),
            from
        ));
        assert!(!mentions_checkout(text, Path::new("/work/tact-ws"), from));
        assert!(!mentions_checkout(
            r#"{"workdir":"/work/tact-ws2"}"#,
            Path::new("/work/tact"),
            from
        ));
        assert!(!mentions_checkout(
            r#"{"cmd":"cat /home/other/work/tact/a"}"#,
            Path::new("/work/tact"),
            from
        ));
    }
}
