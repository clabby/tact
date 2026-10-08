use super::{AgentPrompt, prompts::OverviewPrompt};
use crate::{
    core::protocol::{AuxiliaryError, Busy, Publication},
    web::{
        testing::{Harness, agent, idle_agent, repository, sse_event, worktree},
        wire::PROTOCOL_VERSION,
    },
};
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tact_vcs::{FilePatch, OverviewContext, OverviewRange};
use tokio::sync::Notify;

fn full_range() -> Value {
    json!({"from": 0, "to": 2})
}

fn overview_request(session: &str, instructions: Option<&str>) -> Value {
    json!({
        "session": session,
        "generation": 0,
        "range": full_range(),
        "instructions": instructions,
    })
}

fn question_request(session: &str) -> Value {
    json!({
        "session": session,
        "thread_id": "thread-1",
        "operation_id": "question-1",
        "generation": 0,
        "range": full_range(),
        "path": "tracked.txt",
        "side": "additions",
        "start_line": 1,
        "end_line": 1,
        "messages": [{ "role": "reviewer", "body": "Why was this changed?" }]
    })
}

fn counting_agent(answer: &'static str) -> (Arc<dyn super::ReviewAgent>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = agent({
        let calls = Arc::clone(&calls);
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(answer.to_owned()) }
        }
    });
    (agent, calls)
}

/// The files a review page's patch touches.
fn patch_paths(page: &Value) -> BTreeSet<String> {
    FilePatch::parse_all(page["patch"].as_str().unwrap())
        .iter()
        .filter_map(|file| file.path().map(str::to_owned))
        .collect()
}

/// The lines a review page's patch adds to `path`.
fn added_lines(page: &Value, path: &str) -> Vec<String> {
    FilePatch::parse_all(page["patch"].as_str().unwrap())
        .iter()
        .filter(|file| file.path() == Some(path))
        .flat_map(|file| file.hunks.iter().flat_map(|hunk| hunk.added()))
        .map(str::to_owned)
        .collect()
}

fn paths<const N: usize>(names: [&str; N]) -> BTreeSet<String> {
    names.into_iter().map(str::to_owned).collect()
}

#[tokio::test]
async fn the_review_is_prepared_lazily_and_matches_the_browser_protocol() {
    let harness = Harness::new();

    let (status, review) = harness.call(Method::GET, "/api/review", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(review["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(review["generation"], 0);
    assert_eq!(review["turn_running"], false);
    assert_eq!(
        patch_paths(&review["page"]),
        paths(["tracked.txt", "working.txt"])
    );
    assert_eq!(review["range_targets"].as_array().unwrap().len(), 3);
    assert_eq!(review["overview"], Value::Null);
    assert_eq!(review["questions"], json!([]));
}

#[tokio::test]
async fn other_review_routes_refuse_until_the_review_is_loaded() {
    let harness = Harness::new();

    let (status, body) = harness
        .call(
            Method::POST,
            "/api/range",
            Some(json!({"generation": 0, "range": full_range()})),
        )
        .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "stale_snapshot");
}

#[tokio::test]
async fn a_directory_that_is_not_a_repository_cannot_be_reviewed_or_retried() {
    let harness = Harness::with(tempfile::tempdir().unwrap(), idle_agent());

    let (status, body) = harness.call(Method::GET, "/api/review", None).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "workspace_changed");
    assert_eq!(body["retryable"], false);
    assert_eq!(body["snapshot_valid"], false);
}

#[tokio::test]
async fn refresh_replaces_the_snapshot_and_range_loads_use_the_new_generation() {
    let harness = Harness::new();
    harness.call(Method::GET, "/api/review", None).await;
    fs::write(
        harness.workspace.path().join("working.txt"),
        "changed again\n",
    )
    .unwrap();

    let (status, refreshed) = harness
        .call(Method::POST, "/api/refresh", Some(json!({"generation": 0})))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(refreshed["generation"], 1);
    assert_eq!(
        added_lines(&refreshed["page"], "working.txt"),
        ["changed again"]
    );
    let (status, stale) = harness
        .call(
            Method::POST,
            "/api/range",
            Some(json!({"generation": 0, "range": full_range()})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["code"], "stale_snapshot");
}

#[tokio::test]
async fn overviews_are_generated_on_demand_cached_and_scoped_to_their_session() {
    let (agent, calls) = counting_agent("<p>Overview</p>");
    let harness = Harness::with(repository(), agent);
    harness.open_session("s1").await;
    harness.open_session("s2").await;
    harness.call(Method::GET, "/api/review", None).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    for _ in 0..2 {
        let (status, body) = harness
            .call(
                Method::POST,
                "/api/overview",
                Some(overview_request("s1", None)),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["overview_mdx"], "<p>Overview</p>");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (_, own) = harness
        .call(Method::GET, "/api/review?session=s1", None)
        .await;
    assert_eq!(own["overview"]["status"], "ready");
    let (_, other) = harness
        .call(Method::GET, "/api/review?session=s2", None)
        .await;
    assert_eq!(other["overview"], Value::Null);
}

#[tokio::test]
async fn overview_prompts_carry_the_range_repository_and_instructions_to_the_session() {
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let agent = agent({
        let prompts = Arc::clone(&prompts);
        move |prompt: AgentPrompt| {
            prompts
                .lock()
                .unwrap()
                .push((prompt.session, prompt.prompt));
            async { Ok("```mdx\n## Overview\n```".to_owned()) }
        }
    });
    let harness = Harness::with(repository(), agent);
    harness.open_session("s1").await;
    let (_, review) = harness.call(Method::GET, "/api/review", None).await;

    let (_, body) = harness
        .call(
            Method::POST,
            "/api/overview",
            Some(overview_request("s1", Some("Focus on the migration."))),
        )
        .await;

    assert_eq!(body["overview_mdx"], "## Overview");
    let context = OverviewContext {
        repository: fs::canonicalize(harness.workspace.path()).unwrap(),
        range: OverviewRange::WorkingTree {
            base: review["page"]["base"].as_str().unwrap().to_owned(),
        },
    };
    let expected = OverviewPrompt {
        label: review["page"]["scope"].as_str().unwrap(),
        context: &context,
        instructions: Some("Focus on the migration."),
    }
    .render();
    assert_eq!(*prompts.lock().unwrap(), [("s1".to_owned(), expected)]);
}

#[tokio::test]
async fn a_running_session_blocks_review_actions() {
    let harness = Harness::new();
    harness.open_session("s1").await;
    harness.call(Method::GET, "/api/review", None).await;
    harness.terminal.publisher.publish(Publication::Busy {
        session: "s1".into(),
        busy: Busy {
            turns: 1,
            shells: 0,
        },
    });
    while !harness.hub.any_busy() {
        tokio::task::yield_now().await;
    }

    for (route, request) in [
        ("/api/overview", overview_request("s1", None)),
        (
            "/api/ai-review",
            json!({"session": "s1", "generation": 0, "range": full_range()}),
        ),
        ("/api/question", question_request("s1")),
    ] {
        let (status, body) = harness.call(Method::POST, route, Some(request)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{route}");
        assert_eq!(body["code"], "turn_running", "{route}");
    }
    let (_, review) = harness.call(Method::GET, "/api/review", None).await;
    assert_eq!(review["turn_running"], true);
}

#[tokio::test]
async fn closing_a_session_cancels_its_running_overview() {
    let started = Arc::new(Notify::new());
    let agent = agent({
        let started = Arc::clone(&started);
        move |prompt: AgentPrompt| {
            let started = Arc::clone(&started);
            async move {
                started.notify_one();
                prompt.shutdown.cancelled().await;
                Err(AuxiliaryError::Cancelled)
            }
        }
    });
    let harness = Arc::new(Harness::with(repository(), agent));
    harness.open_session("s1").await;
    harness.call(Method::GET, "/api/review", None).await;
    let request = tokio::spawn({
        let harness = Arc::clone(&harness);
        async move {
            harness
                .call(
                    Method::POST,
                    "/api/overview",
                    Some(overview_request("s1", None)),
                )
                .await
        }
    });
    started.notified().await;

    harness.terminal.publisher.publish(Publication::Closed {
        session: "s1".into(),
    });

    let (status, body) = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "session_cancelled");
}

#[tokio::test]
async fn question_threads_belong_to_their_session() {
    let (agent, _) = counting_agent("It supports the feature.");
    let harness = Harness::with(repository(), agent);
    harness.open_session("s1").await;
    harness.call(Method::GET, "/api/review", None).await;

    let (status, answer) = harness
        .call(Method::POST, "/api/question", Some(question_request("s1")))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["answer"], "It supports the feature.");

    let list = |session: &str| {
        harness.call(
            Method::POST,
            "/api/questions",
            Some(json!({"generation": 0, "session": session})),
        )
    };
    let (_, own) = list("s1").await;
    assert_eq!(own["questions"][0]["thread_id"], "thread-1");
    assert_eq!(own["questions"][0]["status"], "idle");
    let (_, other) = list("s2").await;
    assert_eq!(other["questions"], json!([]));
}

#[tokio::test]
async fn questions_must_anchor_to_the_reviewed_patch() {
    let harness = Harness::new();
    harness.open_session("s1").await;
    harness.call(Method::GET, "/api/review", None).await;
    let mut request = question_request("s1");
    request["path"] = json!("missing.txt");

    let (status, body) = harness
        .call(Method::POST, "/api/question", Some(request))
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_thread");
}

#[tokio::test]
async fn compose_renders_anchored_comments_and_refuses_unanchored_or_stale_ones() {
    let harness = Harness::new();
    harness.call(Method::GET, "/api/review", None).await;
    let decision = |comments: Value, generation: u64| {
        json!({
            "generation": generation,
            "range": full_range(),
            "decision": "request_changes",
            "summary": "Please address this.",
            "comments": comments,
        })
    };
    let comment = json!({
        "path": "tracked.txt", "side": "additions", "start_line": 1, "end_line": 1,
        "body": "Handle the error.\nThis can fail."
    });

    let (status, body) = harness
        .call(
            Method::POST,
            "/api/review/compose",
            Some(decision(json!([comment]), 0)),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["markdown"],
        "## Review: Changes requested\n\n**Scope:** Full branch\n\nPlease address this.\n\n\
         ### Comments\n\n- `tracked.txt:1` (new)\n  Handle the error.\n  This can fail.\n"
    );

    let unanchored = json!({
        "path": "tracked.txt", "side": "additions", "start_line": 99, "end_line": 99, "body": "x"
    });
    let (status, body) = harness
        .call(
            Method::POST,
            "/api/review/compose",
            Some(decision(json!([unanchored]), 0)),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_comment_anchor");

    let (status, body) = harness
        .call(
            Method::POST,
            "/api/review/compose",
            Some(decision(json!([]), 7)),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "stale_snapshot");
}

#[tokio::test]
async fn the_workspace_watcher_reports_changes_only_to_connected_streams() {
    let harness = Harness::new();
    let mut stream = harness.hub.subscribe().unwrap();
    // Opening the review starts watching its checkout.
    harness.call(Method::GET, "/api/review", None).await;

    let mut seen = false;
    'attempts: for attempt in 0..20 {
        fs::write(
            harness.workspace.path().join("working.txt"),
            format!("edit {attempt}\n"),
        )
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
        while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, stream.frames.recv()).await {
            if sse_event(&frame).0 == "workspace" {
                seen = true;
                break 'attempts;
            }
        }
    }
    harness.shutdown.cancel();
    assert!(seen, "an edit should produce a workspace event");
}

#[tokio::test]
async fn review_actions_for_a_session_that_is_not_live_are_unknown_session() {
    let harness = Harness::new();
    harness.call(Method::GET, "/api/review", None).await;

    for (route, request) in [
        ("/api/overview", overview_request("ghost", None)),
        ("/api/question", question_request("ghost")),
    ] {
        let (status, body) = harness.call(Method::POST, route, Some(request)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{route}");
        assert_eq!(body["code"], "unknown_session", "{route}");
    }
    let (status, _) = harness
        .call(
            Method::POST,
            "/api/question/cancel",
            Some(json!({
                "session": "ghost", "operation_id": "q", "generation": 0, "range": full_range()
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_checkout_of_the_repository_is_reviewed_on_its_own() {
    let harness = Harness::new();
    harness.open_session("s1").await;
    let (_directory, worktree) = worktree(harness.workspace.path());
    fs::write(worktree.join("only-here.txt"), "elsewhere\n").unwrap();
    let uri = format!("/api/review?session=s1&checkout={}", worktree.display());

    let (status, other) = harness.call(Method::GET, &uri, None).await;
    let (_, own) = harness
        .call(Method::GET, "/api/review?session=s1", None)
        .await;

    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(other["checkout"]["path"], worktree.to_str().unwrap());
    assert_eq!(other["checkout"]["label"], "elsewhere");
    assert_eq!(
        patch_paths(&other["page"]),
        paths(["only-here.txt", "tracked.txt"])
    );
    assert_eq!(
        patch_paths(&own["page"]),
        paths(["tracked.txt", "working.txt"])
    );
    assert_ne!(own["checkout"]["path"], other["checkout"]["path"]);
}

#[tokio::test]
async fn a_directory_outside_the_repository_cannot_be_reviewed() {
    let harness = Harness::new();
    harness.open_session("s1").await;
    let stranger = tempfile::tempdir().unwrap();
    let uri = format!(
        "/api/review?session=s1&checkout={}",
        stranger.path().display()
    );

    let (status, body) = harness.call(Method::GET, &uri, None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_checkout");
}

#[tokio::test]
async fn a_review_of_another_checkout_names_it_when_composed() {
    let harness = Harness::new();
    harness.open_session("s1").await;
    let (_directory, worktree) = worktree(harness.workspace.path());
    let uri = format!("/api/review?session=s1&checkout={}", worktree.display());
    harness.call(Method::GET, &uri, None).await;
    let request = |checkout: Option<&Path>| {
        let mut request = json!({
            "session": "s1",
            "generation": 0,
            "range": full_range(),
            "decision": "approve",
        });
        if let Some(checkout) = checkout {
            request["checkout"] = json!(checkout);
        }
        request
    };

    let (status, other) = harness
        .call(
            Method::POST,
            "/api/review/compose",
            Some(request(Some(&worktree))),
        )
        .await;
    harness
        .call(Method::GET, "/api/review?session=s1", None)
        .await;
    let (_, own) = harness
        .call(Method::POST, "/api/review/compose", Some(request(None)))
        .await;

    assert_eq!(status, StatusCode::OK, "{other}");
    let checkout: &PathBuf = &worktree;
    assert_eq!(
        other["markdown"],
        format!(
            "## Review: Approved\n\n**Scope:** Full branch\n**Checkout:** `{}`\n",
            checkout.display()
        )
    );
    assert_eq!(
        own["markdown"],
        "## Review: Approved\n\n**Scope:** Full branch\n"
    );
}
