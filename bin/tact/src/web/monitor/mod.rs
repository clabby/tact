//! The monitor: a small model that checks every live session while some browser shows the
//! Monitor, and triages open tasks. For each session it names an attention level, the evidence
//! when that level is not "ok", and optionally a message the developer could send to steer or
//! unblock the agent.
//!
//! Browsers declare interest by calling `POST /api/monitor`; each call holds a lease for [`LEASE`].
//! While a lease is live, a check starts when none is running, at least [`MIN_LIVE_SESSIONS`]
//! sessions are live, the model is available, and either the caller asked for a refresh at least
//! [`REFRESH_FLOOR`] after the last check started, or the sessions changed since the last check
//! read them and the configured interval has passed since it started. Calls and a periodic check
//! apply these rules, so automatic checks do not depend on how often browsers call. Without a
//! browser showing the Monitor, the model is never called.
//!
//! The state sits behind one synchronous lock that is never held across an await or while the
//! hub's lock is taken. Every change is broadcast to all streams as a `monitor` event.

mod blocks;
mod input;
mod model;
mod reply;
#[cfg(test)]
mod tests;

use super::{
    api::{ApiJson, secure_json},
    hub::{Hub, now_unix_ms},
    wire::SummaryState,
};
use crate::core::direct::DirectError;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Response, StatusCode},
    routing::post,
};
#[cfg(test)]
pub(super) use model::{Choice, Settings};
pub(super) use model::{ConfiguredModel, MonitorModel};
use reply::{INSTRUCTIONS, Note, Unreadable};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::Duration,
};
use tact_subagents::AgentId;
use thiserror::Error;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// How long one browser call keeps the monitor interested.
const LEASE: Duration = Duration::from_secs(60);
/// The shortest time between checks a browser can ask for explicitly.
const REFRESH_FLOOR: Duration = Duration::from_secs(10);
/// How often the automatic rule is applied between browser calls.
const CHECK_PERIOD: Duration = Duration::from_secs(5);
/// The Monitor exists to compare sessions, so the model never checks fewer than this many.
const MIN_LIVE_SESSIONS: usize = 2;

pub(super) struct Monitor {
    hub: Hub,
    /// The model and the current settings; both follow configuration reloads.
    model: Arc<dyn MonitorModel>,
    shutdown: CancellationToken,
    inner: Mutex<Inner>,
}

struct Inner {
    availability: Availability,
    lease_until: Option<Instant>,
    running: bool,
    started_at: Option<Instant>,
    /// The sessions as the last check read them.
    fingerprint: Option<Fingerprint>,
    error: Option<String>,
    check: Option<Check>,
}

#[derive(Clone)]
enum Availability {
    Ready,
    Disabled,
    Unavailable(String),
}

/// What a check's freshness is judged by. It holds no times, so time passing alone never makes
/// sessions look changed.
#[derive(Debug, Eq, PartialEq)]
struct Fingerprint(Vec<SessionMark>);

#[derive(Debug, Eq, PartialEq)]
struct SessionMark {
    session: String,
    /// The newest visible entry's id and revision.
    newest: Option<(usize, u64)>,
    state: SummaryState,
    queued: usize,
    unread: bool,
    agents: Vec<(AgentId, &'static str)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Attention {
    NeedsInput,
    Stuck,
    OffTrack,
    Ready,
    Ok,
}

/// The result of the last successful check.
#[derive(Clone, Debug, Serialize)]
struct Check {
    /// When the sessions were read.
    as_of_ms: u64,
    /// When the reply arrived.
    generated_at_ms: u64,
    model: &'static str,
    /// In the order the sessions were given to the model.
    sessions: Vec<Note>,
}

/// The monitor as browsers see it.
#[derive(Debug, Serialize)]
pub(super) struct MonitorState {
    availability: &'static str,
    detail: Option<String>,
    running: bool,
    error: Option<String>,
    interval_seconds: u64,
    check: Option<Check>,
}

/// Why a check failed, as a sentence for display.
#[derive(Debug, Error)]
enum CheckError {
    #[error("{0}")]
    Unavailable(String),
    #[error("The monitor's request failed: {0}")]
    Request(String),
    #[error("The monitor's model did not answer within {} seconds.", .0.as_secs())]
    Timeout(Duration),
    #[error("The monitor's reply could not be read: {0}.")]
    Unreadable(#[from] Unreadable),
}

impl From<DirectError> for CheckError {
    fn from(error: DirectError) -> Self {
        match error {
            DirectError::Unavailable(detail) => Self::Unavailable(detail),
            DirectError::Request(detail) => Self::Request(detail),
            DirectError::Timeout(after) => Self::Timeout(after),
        }
    }
}

impl Monitor {
    /// Creates the monitor and its periodic check, which stops with `shutdown`. The check runs
    /// even while the monitor is disabled, because a reload can enable it.
    pub(super) fn new(
        hub: Hub,
        model: Arc<dyn MonitorModel>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let monitor = Arc::new(Self {
            hub,
            model,
            shutdown: shutdown.clone(),
            inner: Mutex::new(Inner {
                availability: Availability::Unavailable(
                    "The monitor has not checked its model yet.".to_owned(),
                ),
                lease_until: None,
                running: false,
                started_at: None,
                fingerprint: None,
                error: None,
                check: None,
            }),
        });
        tokio::spawn(check_periodically(Arc::downgrade(&monitor), shutdown));
        monitor
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // The critical sections never panic while holding the lock; recover the data regardless.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a browser's interest, starts a check if one is due, and returns the state.
    pub(super) async fn touch(self: &Arc<Self>, refresh: bool) -> MonitorState {
        let availability = self.availability().await;
        {
            let mut inner = self.lock();
            inner.availability = availability;
            inner.lease_until = Some(Instant::now() + LEASE);
        }
        self.consider(refresh);
        self.state()
    }

    async fn availability(&self) -> Availability {
        if !self.model.settings().enabled {
            return Availability::Disabled;
        }
        match self.model.resolve().await {
            Ok(_) => Availability::Ready,
            Err(detail) => Availability::Unavailable(detail),
        }
    }

    /// Starts a check when the scheduling rules in the module documentation allow one.
    fn consider(self: &Arc<Self>, refresh: bool) {
        let now = Instant::now();
        let settings = self.model.settings();
        if !settings.enabled {
            return;
        }
        let (fingerprint, live) = self
            .hub
            .read_sessions(|views| (input::fingerprint(views), views.len()));
        let previous = {
            let mut inner = self.lock();
            if !matches!(inner.availability, Availability::Ready) {
                return;
            }
            let leased = inner.lease_until.is_some_and(|until| now < until);
            if inner.running || live < MIN_LIVE_SESSIONS || !leased {
                return;
            }
            let waited = |at_least: Duration| {
                inner
                    .started_at
                    .is_none_or(|started| now.duration_since(started) >= at_least)
            };
            let changed = inner.fingerprint.as_ref() != Some(&fingerprint);
            let due = (refresh && waited(REFRESH_FLOOR)) || (changed && waited(settings.interval));
            if !due {
                return;
            }
            inner.running = true;
            inner.started_at = Some(now);
            inner.fingerprint = Some(fingerprint);
            inner.check.clone()
        };
        self.broadcast();
        tokio::spawn(Arc::clone(self).run_check(previous));
    }

    /// Runs one check with the model the configuration names when it starts, which may differ
    /// from the one the last browser call found after a reload.
    async fn run_check(self: Arc<Self>, previous: Option<Check>) {
        let choice = match self.model.resolve().await {
            Ok(choice) => choice,
            Err(detail) => {
                {
                    let mut inner = self.lock();
                    inner.running = false;
                    inner.availability = Availability::Unavailable(detail);
                }
                self.broadcast();
                return;
            }
        };
        let as_of_ms = now_unix_ms();
        // Another model judges the sessions independently instead of continuing an assessment it
        // did not make.
        let continued = previous
            .as_ref()
            .filter(|check| check.model == choice.model.as_str());
        let input = self
            .hub
            .read_sessions(|views| input::build(views, continued, as_of_ms));
        let result =
            if input.sessions.len() < MIN_LIVE_SESSIONS {
                None
            } else {
                let reply = tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    reply = self.model.complete(choice, INSTRUCTIONS, input.text) => reply,
                };
                Some(reply.map_err(CheckError::from).and_then(|reply| {
                    reply::parse(&reply, &input.sessions).map_err(CheckError::from)
                }))
            };
        {
            let mut inner = self.lock();
            inner.running = false;
            match result {
                // Sessions closed before the input was read, leaving too few to check.
                None => {}
                Some(Ok(mut notes)) => {
                    for note in &mut notes {
                        note.previous_health = previous
                            .as_ref()
                            .and_then(|check| {
                                check
                                    .sessions
                                    .iter()
                                    .find(|earlier| earlier.session == note.session)
                            })
                            .and_then(|earlier| earlier.health);
                    }
                    inner.error = None;
                    inner.check = Some(Check {
                        as_of_ms,
                        generated_at_ms: now_unix_ms(),
                        model: choice.model.as_str(),
                        sessions: notes,
                    });
                }
                Some(Err(error)) => inner.error = Some(error.to_string()),
            }
        }
        self.broadcast();
    }

    fn state(&self) -> MonitorState {
        // Notes stay with the check; sessions that closed since are left out when it is shown.
        let live: Vec<String> = self
            .hub
            .read_sessions(|views| views.iter().map(|view| view.summary.id.clone()).collect());
        let inner = self.lock();
        let (availability, detail) = match &inner.availability {
            Availability::Ready => ("ready", None),
            Availability::Disabled => (
                "disabled",
                Some("The monitor is turned off ([web.monitor] enabled = false).".to_owned()),
            ),
            Availability::Unavailable(detail) => ("unavailable", Some(detail.clone())),
        };
        MonitorState {
            availability,
            detail,
            running: inner.running,
            error: inner.error.clone(),
            interval_seconds: self.model.settings().interval.as_secs(),
            check: inner.check.clone().map(|mut check| {
                check.sessions.retain(|note| live.contains(&note.session));
                check
            }),
        }
    }

    fn broadcast(&self) {
        self.hub.broadcast("monitor", &self.state());
    }
}

/// Applies the automatic rule between browser calls until the server shuts down.
async fn check_periodically(monitor: Weak<Monitor>, shutdown: CancellationToken) {
    let mut ticks = tokio::time::interval(CHECK_PERIOD);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticks.tick() => {}
        }
        let Some(monitor) = monitor.upgrade() else {
            return;
        };
        monitor.consider(false);
    }
}

/// The monitor's route. The caller supplies authentication and request limits.
pub(super) fn router<S>(monitor: Arc<Monitor>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::<Arc<Monitor>>::new()
        .route("/api/monitor", post(touch))
        .with_state(monitor)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TouchRequest {
    #[serde(default)]
    refresh: bool,
}

async fn touch(
    State(monitor): State<Arc<Monitor>>,
    ApiJson(request): ApiJson<TouchRequest>,
) -> Response<Body> {
    secure_json(StatusCode::OK, monitor.touch(request.refresh).await)
}
