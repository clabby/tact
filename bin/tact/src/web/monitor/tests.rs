use super::{Monitor, MonitorState, input};
use crate::{
    app::config::{ReasoningEffort, ReasoningMode, Speed},
    core::{
        direct::DirectError,
        protocol::{Busy, Draft, Publication, SessionInfo},
        transcript::{LocalEvent, TranscriptRecord, TurnId, UserSubmitted},
    },
    web::{
        bridge::{self, LoopEnd},
        hub::{Hub, Subscription},
        testing::{FakeModel, Harness, sse_event},
    },
};
use axum::http::{Method, StatusCode};
use nanocodex::{
    ClaudeModel, HarnessModel,
    agent::events::{AgentEvent, AgentEventKind},
};
use serde_json::{Value, json, value::to_raw_value};
use std::{cell::Cell, collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const INTERVAL: Duration = Duration::from_secs(30);

struct Fixture {
    terminal: LoopEnd,
    hub: Hub,
    model: Arc<FakeModel>,
    monitor: Arc<Monitor>,
    shutdown: CancellationToken,
    sequence: Cell<u64>,
}

/// A monitor over `model`, enabled with [`INTERVAL`] between automatic checks.
fn fixture(mut model: FakeModel) -> Fixture {
    let (terminal, end) = bridge::bridge();
    let shutdown = CancellationToken::new();
    let hub = Hub::spawn(end.publications, shutdown.clone());
    model.settings.interval = INTERVAL;
    let model = Arc::new(model);
    let monitor = Monitor::new(hub.clone(), Arc::clone(&model) as _, shutdown.clone());
    Fixture {
        terminal,
        hub,
        model,
        monitor,
        shutdown,
        sequence: Cell::new(1),
    }
}

fn replies(replies: impl IntoIterator<Item = Result<String, DirectError>>) -> FakeModel {
    FakeModel {
        replies: std::sync::Mutex::new(replies.into_iter().collect::<VecDeque<_>>()),
        ..FakeModel::default()
    }
}

/// A reply with one note per \`(session, attention)\`.
fn notes(sessions: &[(&str, &str)]) -> Result<String, DirectError> {
    Ok(json!({
        "sessions": sessions
            .iter()
            .map(|(session, attention)| json!({
                "session": session,
                "attention": attention,
                "goal": "Fix the parser",
                "health": 55,
                "blocks": [{"type": "history", "items": [
                    {"action": "Ran the parser tests", "outcome": "failed", "detail": "exit 101", "at": 1},
                ]}],
                "reason": "It asked a question.",
                "suggestion": null,
            }))
            .collect::<Vec<_>>(),
    })
    .to_string())
}

fn noted_sessions(state: &Value) -> Vec<Value> {
    state["check"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["session"].clone())
        .collect()
}

impl Fixture {
    fn publish(&self, publication: Publication) {
        self.terminal.publisher.publish(publication);
    }

    fn next_sequence(&self) -> u64 {
        let sequence = self.sequence.get();
        self.sequence.set(sequence + 1);
        sequence
    }

    fn user_record(&self, text: &str) -> Arc<TranscriptRecord> {
        let sequence = self.next_sequence();
        Arc::new(
            TranscriptRecord::from_local(
                sequence,
                1_000 + sequence,
                LocalEvent::UserSubmitted(UserSubmitted {
                    id: TurnId::new(sequence),
                    text: text.to_owned(),
                }),
            )
            .unwrap(),
        )
    }

    fn assistant_record(&self, text: &str) -> Arc<TranscriptRecord> {
        let sequence = self.next_sequence();
        Arc::new(TranscriptRecord::from_agent(
            sequence,
            1_000 + sequence,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("request"),
                seq: sequence,
                kind: AgentEventKind::AssistantMessage,
                payload: to_raw_value(&json!({"model_call_index": 0, "phase": null, "text": text}))
                    .unwrap()
                    .into(),
            },
        ))
    }

    async fn open(&self, id: &str, prompt: &str) {
        self.open_with(id, vec![self.user_record(prompt)], Draft::default())
            .await;
    }

    async fn open_with(&self, id: &str, records: Vec<Arc<TranscriptRecord>>, draft: Draft) {
        self.publish(Publication::Opened {
            info: SessionInfo {
                id: id.to_owned(),
                workspace: "/work".into(),
                model: "gpt-6.1-sol".to_owned(),
                effort: ReasoningEffort::Low,
                reasoning_mode: ReasoningMode::Standard,
                speed: Speed::Standard,
            },
            records,
            draft,
            queue: Vec::new(),
            busy: Busy::default(),
        });
        while !self.hub.is_live(id) {
            tokio::task::yield_now().await;
        }
    }

    /// Adds a prompt to \`session\`, which changes what the monitor last read.
    async fn prompt(&self, session: &str, text: &str) {
        self.publish(Publication::Record {
            session: session.to_owned(),
            record: self.user_record(text),
        });
        settle().await;
    }

    async fn close(&self, session: &str) {
        self.publish(Publication::Closed {
            session: session.to_owned(),
        });
        while self.hub.is_live(session) {
            tokio::task::yield_now().await;
        }
    }

    fn calls(&self) -> usize {
        self.model.inputs.lock().unwrap().len()
    }

    fn last_input(&self) -> Value {
        serde_json::from_str(self.model.inputs.lock().unwrap().last().unwrap()).unwrap()
    }

    async fn touch(&self, refresh: bool) -> Value {
        let state = self.monitor.touch(refresh).await;
        settle().await;
        json(&state)
    }

    fn state(&self) -> Value {
        json(&self.monitor.state())
    }
}

fn json(state: &MonitorState) -> Value {
    serde_json::to_value(state).unwrap()
}

/// Lets spawned checks and the hub run.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

fn monitor_events(subscription: &mut Subscription) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(frame) = subscription.frames.try_recv() {
        let (name, data) = sse_event(&frame);
        if name == "monitor" {
            events.push(data);
        }
    }
    events
}

#[tokio::test(start_paused = true)]
async fn nothing_runs_without_live_sessions_or_an_available_model() {
    let idle = fixture(FakeModel::default());
    let state = idle.touch(true).await;
    assert_eq!(state["availability"], "ready");
    assert_eq!(state["detail"], Value::Null);
    assert_eq!(state["interval_seconds"], 30);
    assert_eq!(state["check"], Value::Null);
    assert_eq!(idle.calls(), 0, "no session is live");

    let unavailable = fixture(FakeModel {
        unavailable: Some("Sol needs OpenAI credentials.".to_owned()),
        ..FakeModel::default()
    });
    unavailable.open("s1", "Fix the parser").await;
    let state = unavailable.touch(true).await;
    assert_eq!(state["availability"], "unavailable");
    assert_eq!(state["detail"], "Sol needs OpenAI credentials.");
    assert_eq!(unavailable.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_disabled_monitor_reports_why_and_never_runs() {
    let (_terminal, end) = bridge::bridge();
    let shutdown = CancellationToken::new();
    let hub = Hub::spawn(end.publications, shutdown.clone());
    let mut model = FakeModel::default();
    model.settings.enabled = false;
    let model = Arc::new(model);
    let monitor = Monitor::new(hub, Arc::clone(&model) as _, shutdown);
    let state = json(&monitor.touch(true).await);
    assert_eq!(state["availability"], "disabled");
    assert!(
        state["detail"]
            .as_str()
            .unwrap()
            .contains("[web.monitor] enabled = false")
    );
    assert!(model.inputs.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn one_check_runs_at_a_time_and_its_progress_is_broadcast() {
    let gate = Arc::new(Semaphore::new(0));
    let fixture = fixture(FakeModel {
        gate: Some(Arc::clone(&gate)),
        ..replies([notes(&[("s1", "needs_input")])])
    });
    fixture.open("s1", "Fix the parser").await;
    let mut subscription = fixture.hub.subscribe().unwrap();
    settle().await;
    monitor_events(&mut subscription);

    let state = fixture.touch(false).await;
    assert_eq!(state["running"], true);
    assert_eq!(fixture.calls(), 1);
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!(fixture.touch(true).await["running"], true);
    assert_eq!(
        fixture.calls(),
        1,
        "a running check is not joined by another"
    );
    let started = monitor_events(&mut subscription);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["running"], true);

    gate.add_permits(1);
    settle().await;
    let finished = monitor_events(&mut subscription);
    assert_eq!(finished.len(), 1);
    let state = &finished[0];
    assert_eq!(state["running"], false);
    assert_eq!(state["error"], Value::Null);
    let check = &state["check"];
    assert_eq!(check["model"], "gpt-6-luna");
    assert_eq!(
        check["sessions"],
        json!([{
            "session": "s1",
            "attention": "needs_input",
            "goal": "Fix the parser",
            "health": 55,
            "previous_health": null,
            "blocks": [{"type": "history", "items": [
                {"action": "Ran the parser tests", "outcome": "failed", "detail": "exit 101"},
            ]}],
            "reason": "It asked a question.",
            "suggestion": null,
        }])
    );
    assert!(check["generated_at_ms"].as_u64() >= check["as_of_ms"].as_u64());
}

#[tokio::test(start_paused = true)]
async fn automatic_checks_wait_for_changes_and_the_interval_and_refreshes_for_the_floor() {
    let fixture = fixture(FakeModel::default());
    fixture.open("s1", "Fix the parser").await;
    fixture.touch(false).await;
    assert_eq!(fixture.calls(), 1, "the first check is due at once");

    tokio::time::sleep(INTERVAL + Duration::from_secs(5)).await;
    fixture.touch(false).await;
    assert_eq!(fixture.calls(), 1, "nothing changed, however long ago");

    fixture.touch(true).await;
    assert_eq!(
        fixture.calls(),
        2,
        "a refresh after the floor runs without changes"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    fixture.touch(true).await;
    assert_eq!(fixture.calls(), 2, "a refresh within the floor waits");

    fixture.prompt("s1", "Also update the docs").await;
    fixture.touch(false).await;
    assert_eq!(fixture.calls(), 2, "a change waits for the interval");
    tokio::time::sleep(INTERVAL).await;
    assert_eq!(
        fixture.calls(),
        3,
        "the periodic check starts the check while the lease is live"
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_runs_once_the_lease_expires() {
    let fixture = fixture(FakeModel::default());
    fixture.open("s1", "Fix the parser").await;
    fixture.touch(false).await;
    assert_eq!(fixture.calls(), 1);

    tokio::time::sleep(Duration::from_secs(61)).await;
    fixture.prompt("s1", "Also update the docs").await;
    tokio::time::sleep(INTERVAL * 4).await;
    assert_eq!(fixture.calls(), 1, "no browser shows the Monitor");

    fixture.touch(false).await;
    assert_eq!(fixture.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn failures_are_reported_until_the_next_success() {
    let fixture = fixture(replies([
        Err(DirectError::Timeout(Duration::from_secs(120))),
        Ok(r#"{"headline": "All good."}"#.to_owned()),
        notes(&[("s1", "ok")]),
    ]));
    fixture.open("s1", "Fix the parser").await;

    fixture.touch(true).await;
    assert_eq!(
        fixture.state()["error"],
        "The monitor's model did not answer within 120 seconds."
    );
    tokio::time::sleep(Duration::from_secs(10)).await;
    fixture.touch(true).await;
    let state = fixture.state();
    assert_eq!(
        state["error"],
        "The monitor's reply could not be read: its JSON object has no sessions list."
    );
    assert_eq!(state["check"], Value::Null);
    tokio::time::sleep(Duration::from_secs(10)).await;
    fixture.touch(true).await;
    let state = fixture.state();
    assert_eq!(state["error"], Value::Null);
    assert_eq!(noted_sessions(&state), ["s1"]);
    assert_eq!(
        state["check"]["sessions"][0]["reason"],
        Value::Null,
        "an ok note carries no reason"
    );
}

#[tokio::test(start_paused = true)]
async fn notes_follow_input_order_and_drop_sessions_that_closed() {
    let gate = Arc::new(Semaphore::new(0));
    let fixture = fixture(FakeModel {
        gate: Some(Arc::clone(&gate)),
        ..replies([notes(&[
            ("s3", "ready"),
            ("ghost", "stuck"),
            ("s2", "stuck"),
            ("s1", "ok"),
        ])])
    });
    for session in ["s1", "s2", "s3"] {
        fixture.open(session, "Work").await;
    }
    fixture.touch(false).await;
    let read: Vec<Value> = fixture.last_input()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session"].clone())
        .collect();
    assert_eq!(read, ["s1", "s2", "s3"]);

    fixture.close("s2").await;
    gate.add_permits(1);
    settle().await;
    assert_eq!(noted_sessions(&fixture.state()), ["s1", "s3"]);

    fixture.close("s3").await;
    assert_eq!(
        noted_sessions(&fixture.state()),
        ["s1"],
        "the state shows live sessions only"
    );
}

#[tokio::test(start_paused = true)]
async fn the_next_check_reads_the_previous_notes() {
    let fixture = fixture(replies([
        notes(&[("s1", "stuck")]),
        Ok(json!({"sessions": [{"session": "s1", "attention": "ok", "health": 80}]}).to_string()),
    ]));
    fixture.open("s1", "Work").await;
    fixture.touch(true).await;
    assert_eq!(fixture.last_input()["previous_check"], Value::Null);
    assert_eq!(
        fixture.state()["check"]["sessions"][0]["previous_health"],
        Value::Null,
        "nothing came before the first check"
    );

    tokio::time::sleep(Duration::from_secs(10)).await;
    fixture.touch(true).await;
    let previous = &fixture.last_input()["previous_check"];
    assert!(previous["as_of"].is_string());
    assert_eq!(
        previous["sessions"],
        json!([{"session": "s1", "attention": "stuck", "goal": "Fix the parser", "health": 55}])
    );
    let note = &fixture.state()["check"]["sessions"][0];
    assert_eq!(note["health"], 80);
    assert_eq!(
        note["previous_health"], 55,
        "the server carries the previous score"
    );
}

#[tokio::test(start_paused = true)]
async fn a_different_model_judges_without_the_previous_notes() {
    let fixture = fixture(replies([
        notes(&[("s1", "stuck")]),
        Ok(json!({"sessions": [{"session": "s1", "attention": "ok", "health": 80}]}).to_string()),
    ]));
    fixture.open("s1", "Work").await;
    fixture.touch(true).await;

    *fixture.model.model.lock().unwrap() = HarnessModel::Claude(ClaudeModel::Sonnet55);
    tokio::time::sleep(Duration::from_secs(10)).await;
    fixture.touch(true).await;
    assert_eq!(fixture.last_input()["previous_check"], Value::Null);
    let check = &fixture.state()["check"];
    assert_eq!(check["model"], "claude-sonnet-5-5");
    assert_eq!(
        check["sessions"][0]["previous_health"], 55,
        "the trend still compares with the last check"
    );
}

#[tokio::test(start_paused = true)]
async fn figures_must_come_from_what_the_model_read_about_the_session() {
    let metric = |value: &str| json!({"type": "metric", "label": "Failing tests", "value": value, "previous": null});
    let fixture = fixture(replies([Ok(json!({"sessions": [
        {"session": "s1", "attention": "ok", "blocks": [metric("387 failing"), metric("9154 failing")]},
    ]})
    .to_string())]));
    fixture.open("s1", "Fix the 387 failing parser tests").await;
    fixture.touch(true).await;
    let blocks = fixture.state()["check"]["sessions"][0]["blocks"].clone();
    assert_eq!(
        blocks,
        json!([{"type": "metric", "label": "Failing tests", "value": "387 failing", "previous": null, "tone": "neutral"}]),
        "387 is in the prompt; 9154 is not in the input"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_abandons_a_running_check() {
    let gate = Arc::new(Semaphore::new(0));
    let fixture = fixture(FakeModel {
        gate: Some(Arc::clone(&gate)),
        ..FakeModel::default()
    });
    fixture.open("s1", "Work").await;
    fixture.touch(false).await;
    let waiting = Arc::strong_count(&gate);

    fixture.shutdown.cancel();
    settle().await;
    assert_eq!(
        Arc::strong_count(&gate),
        waiting - 1,
        "the pending request was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn the_input_is_bounded_and_keeps_the_newest_prompts() {
    let fixture = fixture(FakeModel::default());
    let huge = "word ".repeat(20_000);
    for index in 0..20 {
        let mut records = vec![
            fixture.user_record(&format!("first prompt {index} {huge}")),
            fixture.user_record(&format!("second prompt {index}")),
        ];
        for _ in 0..30 {
            records.push(fixture.assistant_record(&huge));
        }
        fixture
            .open_with(
                &format!("s{index}"),
                records,
                Draft {
                    rev: 1,
                    text: "DRAFT-SECRET".to_owned(),
                    ..Draft::default()
                },
            )
            .await;
    }
    let built = fixture
        .hub
        .read_sessions(|views| input::build(views, None, 10_000_000));

    assert!(built.text.len() <= 100_000, "{} bytes", built.text.len());
    assert!(
        !built.text.contains("DRAFT-SECRET"),
        "drafts are never sent"
    );
    let document: Value = serde_json::from_str(&built.text).unwrap();
    let sessions = document["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), input::MAX_SESSIONS);
    assert_eq!(built.sessions.len(), input::MAX_SESSIONS);
    for session in sessions {
        let numbers: Vec<u64> = session["transcript"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["n"].as_u64().unwrap())
            .collect();
        let expected: Vec<u64> = (1..=numbers.len() as u64).rev().collect();
        assert_eq!(numbers, expected, "items are numbered, larger is newer");
        let prompts: Vec<&str> = session["transcript"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["kind"] == "user")
            .map(|item| item["text"].as_str().unwrap())
            .collect();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].starts_with("second prompt"), "newest first");
        assert!(prompts[1].starts_with("first prompt"));
        assert!(prompts[1].chars().count() <= 1_500);
    }
}

#[tokio::test]
async fn the_route_is_served_behind_the_guard() {
    let harness = Harness::new();
    let (status, state) = harness
        .call(Method::POST, "/api/monitor", Some(json!({"refresh": true})))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["availability"], "disabled");
    assert_eq!(state["check"], Value::Null);

    let (status, error) = harness
        .call(
            Method::POST,
            "/api/monitor",
            Some(json!({"refresh": "yes"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["code"], "invalid_request");
}
