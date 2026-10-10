//! Session-local prompt history and restoration of the draft being edited.

use crate::core::prompt::Submission;

#[derive(Default)]
pub(super) struct PromptHistory {
    entries: Vec<Submission>,
    browsing: Option<Browsing>,
}

struct Browsing {
    index: usize,
    saved_draft: Submission,
}

impl PromptHistory {
    pub(super) fn record(&mut self, prompt: Submission) {
        self.entries.push(prompt);
        self.browsing = None;
    }

    /// Steps back to the previous prompt. Browsing starts by saving `draft`, the unsent draft that
    /// stepping forward past the newest prompt restores.
    pub(super) fn previous(&mut self, draft: impl FnOnce() -> Submission) -> Option<Submission> {
        if self.entries.is_empty() {
            return None;
        }

        let browsing = self.browsing.get_or_insert_with(|| Browsing {
            index: self.entries.len(),
            saved_draft: draft(),
        });
        if browsing.index == 0 {
            return None;
        }

        browsing.index -= 1;
        Some(self.entries[browsing.index].clone())
    }

    pub(super) fn next(&mut self) -> Option<Submission> {
        let browsing = self.browsing.as_mut()?;
        browsing.index += 1;
        if browsing.index < self.entries.len() {
            return Some(self.entries[browsing.index].clone());
        }

        Some(self.browsing.take()?.saved_draft)
    }

    pub(super) fn is_browsing(&self) -> bool {
        self.browsing.is_some()
    }

    pub(super) fn detach(&mut self) {
        self.browsing = None;
    }
}
