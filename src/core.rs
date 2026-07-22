//! Nanocodex construction, turn execution, and graceful shutdown.

use crate::{
    config::{Config, ReasoningEffort},
    error::{Result, RuntimeError},
    mcp,
    subagents::{self, AgentUpdate, SubagentControl},
};
use nanocodex::{
    AgentEvents, Nanocodex, NanocodexError, Responses, SessionSnapshot, Tools, TurnControl,
};
use std::{
    io,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct ConfiguredAgent {
    pub(crate) agent: Nanocodex,
    pub(crate) events: AgentEvents,
    pub(crate) subagent_updates: mpsc::UnboundedReceiver<AgentUpdate>,
    pub(crate) subagent_control: SubagentControl,
}

enum Cancellation {
    NotRequested,
    Requested,
    Failed(NanocodexError),
}

impl ConfiguredAgent {
    pub(crate) async fn run_from_config(
        config: &Config,
        prompt: String,
        shutdown: CancellationToken,
    ) -> Result<()> {
        Self::from_config(config)?
            .run(prompt, shutdown, io::stdout())
            .await
    }

    pub(crate) fn from_config(config: &Config) -> Result<Self> {
        Self::from_config_with_session(config, config.agent().thinking(), None, None)
    }

    pub(crate) fn from_config_with_session(
        config: &Config,
        thinking: ReasoningEffort,
        session_id: Option<&str>,
        snapshot: Option<SessionSnapshot>,
    ) -> Result<Self> {
        let agent_config = config.agent();
        let workspace = Self::resolve_workspace(agent_config.workspace())?;
        let mcp = mcp::provider(config)?;
        let auth = config.auth().load()?;

        let mut responses = Responses::builder();
        if let Some(url) = agent_config.websocket_url() {
            responses = responses.websocket_url(url);
        }
        if let Some(url) = agent_config.api_base_url() {
            responses = responses.api_base_url(url);
        }

        let mut tools = Tools::builder()
            .web_search(agent_config.web_search())
            .image_generation(agent_config.image_generation());
        if let Some(mcp) = mcp {
            tools = tools.provider(mcp);
        }
        let tools = tools.build().map_err(NanocodexError::from)?;
        let (subagents, subagent_control, subagent_updates) = subagents::channel();
        let mut builder = Nanocodex::builder(auth)
            .workspace(workspace)
            .thinking(thinking.into())
            .responses(responses.build())
            .tools_factory(move |agent| {
                subagents::root_tools(tools.clone(), agent, Arc::clone(&subagents))
            });
        if let Some(instructions) = agent_config.instructions() {
            builder = builder.instructions(instructions);
        }
        if let Some(session_id) = session_id {
            builder = builder.session_id(session_id);
        }
        if let Some(snapshot) = snapshot {
            builder = builder.resume(snapshot);
        }

        let (agent, events) = builder.build()?;
        Ok(Self {
            agent,
            events,
            subagent_updates,
            subagent_control,
        })
    }

    async fn run(
        mut self,
        prompt: String,
        shutdown: CancellationToken,
        mut output: impl Write,
    ) -> Result<()> {
        let (_unused_sender, empty_updates) = mpsc::unbounded_channel();
        let mut subagent_updates = std::mem::replace(&mut self.subagent_updates, empty_updates);
        let subagent_drain =
            tokio::spawn(async move { while subagent_updates.recv().await.is_some() {} });
        if shutdown.is_cancelled() {
            self.shutdown().await;
            subagent_drain.abort();
            return Ok(());
        }

        let turn = match self.agent.prompt(prompt).await {
            Ok(turn) => turn,
            Err(error) => {
                self.shutdown().await;
                subagent_drain.abort();
                return Err(error.into());
            }
        };
        let control = turn.control();
        let mut cancellation = Cancellation::NotRequested;
        let event_result = tokio::select! {
            biased;
            result = self.events.write_turn_jsonl(&mut output) => result,
            () = shutdown.cancelled() => {
                cancellation = Cancellation::request(&control).await;
                self.subagent_control.cancel_all().await;
                self.events.write_turn_jsonl(&mut output).await
            }
        };

        if event_result.is_err() && matches!(cancellation, Cancellation::NotRequested) {
            cancellation = Cancellation::request(&control).await;
            self.subagent_control.cancel_all().await;
        }

        let turn_result = turn.result().await;
        let was_cancelled = matches!(cancellation, Cancellation::Requested);
        drop(control);
        self.shutdown().await;
        subagent_drain.abort();

        event_result?;
        if let Cancellation::Failed(error) = cancellation {
            return Err(error.into());
        }
        match turn_result {
            Err(NanocodexError::TurnCancelled) if was_cancelled => Ok(()),
            result => result.map(|_| ()).map_err(Into::into),
        }
    }

    async fn shutdown(mut self) {
        drop(self.agent);
        while self.events.recv().await.is_some() {}
    }

    fn resolve_workspace(path: &Path) -> Result<PathBuf> {
        let workspace = path
            .canonicalize()
            .map_err(|source| RuntimeError::ResolveWorkspace {
                path: path.to_path_buf(),
                source,
            })?;
        if !workspace.is_dir() {
            return Err(RuntimeError::WorkspaceNotDirectory(workspace).into());
        }

        Ok(workspace)
    }
}

impl Cancellation {
    async fn request(control: &TurnControl) -> Self {
        match control.cancel().await {
            Ok(()) => Self::Requested,
            Err(NanocodexError::TurnNotCancellable) => Self::NotRequested,
            Err(error) => Self::Failed(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConfiguredAgent;
    use crate::error::{Error, RuntimeError};
    use nanocodex::{
        Nanocodex, NanocodexError, Responses, ResponsesAttempt, ResponsesServiceResponse,
    };
    use nanocodex_core::{
        ModelConfig, OpenAiAuth, OpenAiAuthError, OpenAiAuthFuture, OpenAiAuthMode,
        OpenAiAuthSnapshot, OpenAiAuthSource, Thinking,
        responses::{RequestProfile, ResponseCreate},
    };
    use std::{
        fs,
        future::{Pending, pending},
        result::Result as StdResult,
        sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };
    use tempfile::tempdir;
    use tokio::{sync::Notify, time::timeout};
    use tokio_util::sync::CancellationToken;
    use tower::Service;

    struct TestChatGptAuth;

    impl OpenAiAuthSource for TestChatGptAuth {
        fn validate(&self) -> StdResult<(), OpenAiAuthError> {
            Ok(())
        }

        fn snapshot(&self) -> OpenAiAuthFuture<'_, StdResult<OpenAiAuthSnapshot, OpenAiAuthError>> {
            Box::pin(async {
                Ok(OpenAiAuthSnapshot::new(
                    OpenAiAuthMode::ChatGpt,
                    "test-token",
                    Some("test-account"),
                    false,
                    1,
                ))
            })
        }

        fn recover_unauthorized(
            &self,
            _rejected: &OpenAiAuthSnapshot,
        ) -> OpenAiAuthFuture<'_, StdResult<(), OpenAiAuthError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Clone)]
    struct PendingService {
        called: Arc<Notify>,
    }

    impl Service<ResponsesAttempt> for PendingService {
        type Response = ResponsesServiceResponse;
        type Error = NanocodexError;
        type Future = Pending<StdResult<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: ResponsesAttempt) -> Self::Future {
            self.called.notify_one();
            pending()
        }
    }

    #[test]
    fn workspace_must_be_a_directory() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("file");
        fs::write(&file, "contents").unwrap();

        let error = ConfiguredAgent::resolve_workspace(&file).unwrap_err();
        let file = file.canonicalize().unwrap();

        assert!(matches!(
            error,
            Error::Runtime(RuntimeError::WorkspaceNotDirectory(path)) if path == file
        ));
    }

    #[test]
    fn chatgpt_requests_disable_response_storage() {
        let auth = OpenAiAuth::managed_chatgpt(Arc::new(TestChatGptAuth));
        let config = ModelConfig {
            auth,
            ..ModelConfig::default()
        };
        let profile = RequestProfile::new("session", "lineage", Arc::from([]));

        let request = serde_json::to_value(ResponseCreate::warmup(
            &config,
            Thinking::Medium,
            &profile,
            None,
        ))
        .unwrap();

        assert_eq!(request["store"], false);
    }

    #[tokio::test]
    async fn cancellation_stops_the_turn_and_waits_for_the_driver() {
        let called = Arc::new(Notify::new());
        let service_called = Arc::clone(&called);
        let responses = Responses::builder()
            .service(move || PendingService {
                called: Arc::clone(&service_called),
            })
            .build();
        let (agent, events) = Nanocodex::builder("test-key")
            .responses(responses)
            .build()
            .unwrap();
        let (_registry, subagent_control, subagent_updates) = crate::subagents::channel();
        let configured = ConfiguredAgent {
            agent,
            events,
            subagent_updates,
            subagent_control,
        };
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            configured
                .run("keep running".to_owned(), task_shutdown, Vec::new())
                .await
        });

        timeout(Duration::from_secs(5), called.notified())
            .await
            .expect("the model request should start");
        shutdown.cancel();

        timeout(Duration::from_secs(5), task)
            .await
            .expect("graceful shutdown should finish")
            .expect("the core task should not panic")
            .expect("cancellation should be a successful shutdown");
    }
}
