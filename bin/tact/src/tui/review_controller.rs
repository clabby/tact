use crate::tui::pane::PaneId;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(crate) type ReviewResult = std::result::Result<Option<String>, crate::review::ReviewError>;
pub(crate) type ReviewTask = JoinHandle<ReviewCompletion>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReviewIdentity {
    pub(crate) pane: PaneId,
    pub(crate) pane_generation: u64,
    pub(crate) controller_generation: u64,
}

pub(crate) struct ReviewCompletion {
    pub(crate) identity: ReviewIdentity,
    pub(crate) result: ReviewResult,
}

struct ActiveReview {
    identity: ReviewIdentity,
    url: Option<String>,
    cancellation: CancellationToken,
    task: ReviewTask,
}

/// Owns the complete lifetime of the one browser review exposed by the TUI.
pub(crate) struct ReviewController {
    next_generation: u64,
    active: Option<ActiveReview>,
}

impl ReviewController {
    pub(crate) const fn new() -> Self {
        Self {
            next_generation: 0,
            active: None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn start(
        &mut self,
        pane: PaneId,
        pane_generation: u64,
        spawn: impl FnOnce(ReviewIdentity, CancellationToken) -> ReviewTask,
    ) -> Option<ReviewIdentity> {
        if self.active.is_some() {
            return None;
        }

        let identity = ReviewIdentity {
            pane,
            pane_generation,
            controller_generation: self.next_generation,
        };
        self.next_generation = self.next_generation.saturating_add(1);
        let cancellation = CancellationToken::new();
        let task = spawn(identity, cancellation.clone());
        self.active = Some(ActiveReview {
            identity,
            url: None,
            cancellation,
            task,
        });
        Some(identity)
    }

    pub(crate) fn task_mut(&mut self) -> Option<&mut ReviewTask> {
        self.active.as_mut().map(|review| &mut review.task)
    }

    pub(crate) fn accepts(&self, identity: ReviewIdentity, shutdown: &CancellationToken) -> bool {
        !shutdown.is_cancelled()
            && self
                .active
                .as_ref()
                .is_some_and(|review| review.identity == identity)
    }

    pub(crate) fn set_url(&mut self, identity: ReviewIdentity, url: String) -> bool {
        let Some(review) = self
            .active
            .as_mut()
            .filter(|review| review.identity == identity)
        else {
            return false;
        };
        review.url = Some(url);
        true
    }

    pub(crate) fn identity(&self) -> Option<ReviewIdentity> {
        self.active.as_ref().map(|review| review.identity)
    }

    pub(crate) fn url(&self, pane: PaneId) -> Option<&str> {
        self.active
            .as_ref()
            .filter(|review| review.identity.pane == pane)
            .and_then(|review| review.url.as_deref())
    }

    pub(crate) fn cancel(&mut self) -> Option<ReviewIdentity> {
        let review = self.active.take()?;
        review.cancellation.cancel();
        review.task.abort();
        Some(review.identity)
    }

    pub(crate) fn complete(&mut self, identity: ReviewIdentity) -> bool {
        let matches = self
            .active
            .as_ref()
            .is_some_and(|review| review.identity == identity);
        if matches {
            self.active = None;
        }
        matches
    }
}

#[cfg(test)]
mod tests {
    use super::{ReviewCompletion, ReviewController, ReviewIdentity};
    use crate::tui::pane::PaneId;

    fn pending_review(
        identity: ReviewIdentity,
        _: tokio_util::sync::CancellationToken,
    ) -> super::ReviewTask {
        tokio::spawn(async move {
            std::future::pending::<()>().await;
            ReviewCompletion {
                identity,
                result: Ok(None),
            }
        })
    }

    #[tokio::test]
    async fn controller_rejects_stale_generations_and_cancels_owned_task() {
        let mut controller = ReviewController::new();
        let identity = controller
            .start(PaneId::Main, 4, pending_review)
            .expect("the first review should start");
        assert!(controller.set_url(identity, "http://127.0.0.1/review".to_owned()));
        assert_eq!(
            controller.url(PaneId::Main),
            Some("http://127.0.0.1/review")
        );

        let stale = ReviewIdentity {
            controller_generation: identity.controller_generation + 1,
            ..identity
        };
        assert!(!controller.accepts(stale, &tokio_util::sync::CancellationToken::new()));
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();
        assert!(!controller.accepts(identity, &cancelled));
        let abort = controller.task_mut().unwrap().abort_handle();
        assert_eq!(controller.cancel(), Some(identity));
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
        assert!(!controller.is_active());
    }
}
