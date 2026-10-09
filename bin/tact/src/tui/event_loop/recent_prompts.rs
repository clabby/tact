//! Recent prompts across stored sessions, for the recent-prompt picker.
//!
//! Loading reads every stored transcript, so it is warmed at startup and the result is cached for
//! the rest of the run. Prompts submitted later are added to the cache as they are journaled. A
//! picker opened before the cache is ready waits for the one load in flight.

use super::{
    EventLoop,
    background::{TaskKind, TaskOutput},
};
use crate::{
    app::error::Result,
    core::{
        pane::PaneId,
        session::{self, RecentPrompt, SessionStore},
    },
    tui::components::{AppEvent, RecentPromptDraft},
};
use std::{
    cmp::Reverse,
    future::Future,
    path::{Path, PathBuf},
    time::Instant,
};

/// Loads the recent prompts of every stored session.
pub(super) fn load(store: SessionStore) -> impl Future<Output = TaskOutput> {
    async move {
        TaskOutput::RecentPrompts(
            store
                .run_blocking(SessionStore::recent_prompts)
                .await
                .map_err(Into::into),
        )
    }
}

/// A picker waiting for recent prompts, with the prompts of its pane's live session.
struct RecentPromptRequest {
    pane: PaneId,
    session_id: String,
    workspace: PathBuf,
    current_prompts: Vec<RecentPromptDraft>,
}

impl RecentPromptRequest {
    /// The picker's prompts: the stored ones, with the pane's live session represented by its
    /// current prompts instead of its stored snapshot.
    fn into_event(self, persisted: Vec<RecentPrompt>) -> AppEvent {
        let prompts = merge_recent_prompts(
            persisted,
            self.current_prompts,
            &self.session_id,
            &self.workspace,
        );
        AppEvent::RecentPromptsLoaded {
            pane: self.pane,
            session_id: self.session_id,
            prompts,
        }
    }
}

/// The cached recent prompts and the picker waiting for them, if any.
#[derive(Default)]
pub(super) struct RecentPrompts {
    cache: Option<Vec<RecentPrompt>>,
    request: Option<RecentPromptRequest>,
}

impl RecentPrompts {
    pub(super) fn cached(&self) -> Option<&[RecentPrompt]> {
        self.cache.as_deref()
    }

    /// Adds a just-journaled prompt to the cache, newest first, once the cache is loaded.
    pub(super) fn remember(&mut self, prompt: RecentPrompt) {
        let Some(cache) = &mut self.cache else {
            return;
        };
        let index = cache
            .partition_point(|existing| existing.recorded_at_unix_ms >= prompt.recorded_at_unix_ms);
        cache.insert(index, prompt);
        cache.truncate(session::MAX_RECENT_PROMPTS);
    }
}

pub(super) fn merge_recent_prompts(
    mut persisted: Vec<RecentPrompt>,
    current: Vec<RecentPromptDraft>,
    session_id: &str,
    workspace: &Path,
) -> Vec<RecentPrompt> {
    persisted.retain(|prompt| prompt.session_id != session_id);
    let mut prompts = current
        .into_iter()
        .rev()
        .map(|prompt| RecentPrompt {
            text: prompt.text,
            recorded_at_unix_ms: prompt.recorded_at_unix_ms,
            session_id: session_id.to_owned(),
            workspace: workspace.to_path_buf(),
        })
        .collect::<Vec<_>>();
    prompts.extend(persisted);
    prompts.sort_by_key(|prompt| Reverse(prompt.recorded_at_unix_ms));
    prompts
}

impl EventLoop {
    /// Shows the recent-prompt picker's prompts, waiting for the load in flight when the cache is
    /// not ready yet.
    pub(super) fn load_recent_prompts(
        &mut self,
        pane: PaneId,
        current_prompts: Vec<RecentPromptDraft>,
        workspace: &Path,
    ) {
        let session_id = self
            .panes
            .session_id(pane)
            .expect("recent-prompt pane must exist")
            .to_owned();
        let request = RecentPromptRequest {
            pane,
            session_id,
            workspace: workspace.to_path_buf(),
            current_prompts,
        };
        if let Some(prompts) = self.recent_prompts.cache.clone() {
            self.show(request.into_event(prompts));
            return;
        }

        self.frontend.detach_input();
        self.recent_prompts.request = Some(request);
        if !self.tasks.is_active(TaskKind::RecentPrompts) {
            self.tasks.spawn(
                TaskKind::RecentPrompts,
                load(SessionStore::new(self.config.path())),
            );
        }
    }

    pub(super) fn on_recent_prompts_loaded(&mut self, prompts: Result<Vec<RecentPrompt>>) {
        match (prompts, self.recent_prompts.request.take()) {
            (Ok(prompts), Some(request)) => {
                self.recent_prompts.cache = Some(prompts.clone());
                self.frontend.attach_input();
                self.show(request.into_event(prompts));
            }
            (Ok(prompts), None) => self.recent_prompts.cache = Some(prompts),
            (Err(error), Some(request)) => {
                self.frontend.attach_input();
                self.show(AppEvent::RecentPromptLoadFailed {
                    pane: request.pane,
                    error: format!("Could not load recent prompts: {error}"),
                });
            }
            (Err(_), None) => {}
        }
        self.scheduler.request_immediate(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::{RecentPrompts, merge_recent_prompts};
    use crate::{
        core::session::{MAX_RECENT_PROMPTS, RecentPrompt},
        tui::components::RecentPromptDraft,
    };
    use std::path::Path;

    fn prompt(text: &str, recorded_at_unix_ms: u64, session_id: &str) -> RecentPrompt {
        RecentPrompt {
            text: text.to_owned(),
            recorded_at_unix_ms,
            session_id: session_id.to_owned(),
            workspace: "/work".into(),
        }
    }

    fn texts(prompts: &[RecentPrompt]) -> Vec<&str> {
        prompts.iter().map(|prompt| prompt.text.as_str()).collect()
    }

    #[test]
    fn current_session_prompts_replace_the_persisted_snapshot() {
        let persisted = vec![
            prompt("stale current", 20, "current"),
            prompt("other", 15, "other"),
        ];
        let current = vec![
            RecentPromptDraft {
                text: "first".to_owned(),
                recorded_at_unix_ms: 10,
            },
            RecentPromptDraft {
                text: "just submitted".to_owned(),
                recorded_at_unix_ms: 20,
            },
        ];

        let prompts = merge_recent_prompts(persisted, current, "current", Path::new("/work"));

        assert_eq!(texts(&prompts), ["just submitted", "other", "first"]);
    }

    #[test]
    fn remembered_prompts_join_a_loaded_cache_newest_first_and_stay_bounded() {
        let mut recent = RecentPrompts::default();
        recent.remember(prompt("before load", 5, "session"));
        assert!(recent.cached().is_none());

        recent.cache = Some(vec![
            prompt("newer", 30, "other"),
            prompt("older", 10, "other"),
        ]);
        recent.remember(prompt("middle", 20, "session"));
        assert_eq!(
            texts(recent.cached().unwrap()),
            ["newer", "middle", "older"]
        );

        for index in 0..MAX_RECENT_PROMPTS {
            recent.remember(prompt("flood", 40 + index as u64, "session"));
        }
        assert_eq!(recent.cached().unwrap().len(), MAX_RECENT_PROMPTS);
    }
}
