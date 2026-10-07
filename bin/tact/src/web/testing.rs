//! Shared fixtures for the server's tests.

use super::{
    api::{self, AppState},
    assets::AssetStore,
    bridge::{self, Busy, Draft, LoopEnd, Publication, SessionInfo},
    hub::Hub,
    review::{self, ReviewAgent, ReviewState},
    token::MachineToken,
};
use crate::app::config::{ReasoningEffort, ReasoningMode, Speed};
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use std::{fs, path::Path, process::Command, sync::Arc};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

/// A complete router over a fake terminal loop.
pub(super) struct Harness {
    pub(super) app: Router,
    pub(super) terminal: LoopEnd,
    pub(super) hub: Hub,
    pub(super) state: Arc<AppState>,
    pub(super) workspace: TempDir,
    pub(super) shutdown: CancellationToken,
    _home: TempDir,
}

pub(super) fn idle_agent() -> ReviewAgent {
    Arc::new(|_, _, _| Box::pin(async { Ok("<p>Overview</p>".to_owned()) }))
}

impl Harness {
    pub(super) fn new() -> Self {
        Self::with(repository(), idle_agent())
    }

    pub(super) fn with(workspace: TempDir, agent: ReviewAgent) -> Self {
        crate::install_tls_provider();
        let home = tempfile::tempdir().unwrap();
        let (terminal, end) = bridge::bridge();
        let shutdown = CancellationToken::new();
        let hub = Hub::spawn(end.publications, shutdown.clone());
        let review = ReviewState::new(
            workspace.path().to_owned(),
            hub.clone(),
            agent,
            shutdown.clone(),
        );
        let token = MachineToken::load_or_create(&home.path().join("web")).unwrap();
        let state = Arc::new(AppState {
            token,
            hub: hub.clone(),
            requests: end.requests,
            queries: end.queries,
            workspace: workspace.path().to_owned(),
            port: 7878,
            public_origin: None,
            registry_directory: home.path().join("web/instances"),
            assets: AssetStore::new(home.path().to_owned()),
            client: reqwest::Client::new(),
            shutdown: shutdown.clone(),
        });
        let app = api::router(Arc::clone(&state), review::router(review));
        Self {
            app,
            terminal,
            hub,
            state,
            workspace,
            shutdown,
            _home: home,
        }
    }

    pub(super) fn cookie(&self) -> String {
        format!("tact={}", self.state.token.expose())
    }

    /// Sends an authenticated request the way the browser application does.
    pub(super) async fn call(
        &self,
        method: Method,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, self.cookie())
            .header("x-tact", "1");
        let body = match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let (status, _, bytes) = self.send(builder.body(body).unwrap()).await;
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    pub(super) async fn send(&self, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (parts.status, parts.headers, bytes.to_vec())
    }

    /// Posts a command and answers it from the fake terminal loop, returning the HTTP result and
    /// the command the loop received.
    pub(super) async fn command(
        &mut self,
        body: serde_json::Value,
        answer: Result<bridge::Reply, bridge::CommandError>,
    ) -> (StatusCode, serde_json::Value, bridge::Request) {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/cmd")
            .header(header::COOKIE, self.cookie())
            .header("x-tact", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let app = self.app.clone();
        let response = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
        let mut received = self.terminal.requests.recv().await.unwrap();
        let (reply, _) = tokio::sync::oneshot::channel();
        let sender = std::mem::replace(&mut received.reply, reply);
        sender.send(answer).unwrap();
        let response = response.await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value, received)
    }

    /// Makes `id` a live, active session, as the terminal loop would.
    pub(super) async fn open_session(&self, id: &str) {
        self.terminal.publisher.publish(Publication::Opened {
            info: SessionInfo {
                id: id.to_owned(),
                model: "gpt-6.1-sol".to_owned(),
                effort: ReasoningEffort::Low,
                reasoning_mode: ReasoningMode::Standard,
                speed: Speed::Standard,
            },
            records: Vec::new(),
            draft: Draft::default(),
            queue: Vec::new(),
            busy: Busy::default(),
        });
        self.terminal.publisher.publish(Publication::Active {
            session: id.to_owned(),
        });
        while !self.hub.is_live(id) {
            tokio::task::yield_now().await;
        }
    }
}

pub(super) fn repository() -> TempDir {
    let directory = TempDir::new().unwrap();
    git(
        directory.path(),
        ["init", "--quiet", "--initial-branch=main"],
    );
    git(
        directory.path(),
        ["config", "user.email", "test@example.com"],
    );
    git(directory.path(), ["config", "user.name", "Test User"]);
    git(directory.path(), ["config", "commit.gpgSign", "false"]);
    fs::write(directory.path().join("tracked.txt"), "initial\n").unwrap();
    git(directory.path(), ["add", "tracked.txt"]);
    git(directory.path(), ["commit", "--quiet", "-m", "initial"]);
    git(directory.path(), ["checkout", "--quiet", "-b", "feature"]);
    fs::write(directory.path().join("tracked.txt"), "feature\n").unwrap();
    git(directory.path(), ["add", "tracked.txt"]);
    git(directory.path(), ["commit", "--quiet", "-m", "feature"]);
    fs::write(directory.path().join("working.txt"), "working\n").unwrap();
    directory
}

fn git<const N: usize>(root: &Path, arguments: [&str; N]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
}
