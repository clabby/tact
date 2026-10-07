use super::*;
use futures_util::{FutureExt, Stream};
use nanocodex::{
    AgentSessionContext, HarnessFamily, HarnessModel, PromptRequest, TurnControl, TurnResult,
    agent::{ChildSnapshot, SpawnOptions},
    oai::Prompt,
};
use nanocodex_agent::backend::{
    BackendFuture, BackendPrompt, BackendPromptRoute, BackendRuntime, BackendTurn, BackendTurnKey,
    LifecycleBackend,
};
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio_util::sync::CancellationToken;

pub(super) fn wrap(
    native: Nanocodex,
    bridge: Arc<Bridge>,
    spawn: Option<CleanAgentFactory>,
    fast_mode: bool,
) -> (Nanocodex, AgentEvents) {
    let (runtime, events) = BackendRuntime::new(native.session_id());
    let state = Arc::new(State {
        native,
        bridge,
        spawn,
        fast_mode: AtomicBool::new(fast_mode),
        active: AsyncMutex::new(None),
        stopped: AtomicBool::new(false),
        shutdown: AsyncMutex::new(()),
    });
    (runtime.bind(Driver(state)), events)
}

#[derive(Clone)]
struct Active {
    key: BackendTurnKey,
    control: TurnControl,
    done: CancellationToken,
}
struct State {
    native: Nanocodex,
    bridge: Arc<Bridge>,
    spawn: Option<CleanAgentFactory>,
    // Clean recipes inherit the last successfully applied fast-mode setting.
    fast_mode: AtomicBool,
    active: AsyncMutex<Option<Active>>,
    stopped: AtomicBool,
    shutdown: AsyncMutex<()>,
}
struct Driver(Arc<State>);

impl State {
    async fn submit(self: Arc<Self>, request: BackendPrompt) -> Result<BackendTurn> {
        let mut active = self.active.lock().await;
        if self.stopped.load(Ordering::Acquire) {
            return Err(NanocodexError::AgentStopped);
        }
        if active.is_some() {
            return Err(invalid("Claude already has an active turn"));
        }
        if request.request_id.is_some() {
            return Err(invalid("Claude request IDs require native durability"));
        }
        self.bridge.runtime.control().begin_turn();
        let mut native_request = PromptRequest::from(request.prompt);
        if request.cancel_on_admission {
            native_request = native_request.cancel_on_admission();
        }
        let turn = self.native.prompt(native_request).await?;
        let id = turn.id().to_owned();
        self.bridge
            .install_events(request.events.with_turn_id(id.clone()), id.clone());
        let control = turn.control();
        let done = CancellationToken::new();
        *active = Some(Active {
            key: request.key,
            control,
            done: done.clone(),
        });
        let (result_tx, result_rx) = oneshot::channel();
        let state = self.clone();
        tokio::spawn(async move {
            let (mut result, terminal) = match std::panic::AssertUnwindSafe(state.drive(turn))
                .catch_unwind()
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    state.stopped.store(true, Ordering::Release);
                    let _ = state.native.shutdown().await;
                    state.bridge.runtime.control().cancel().await;
                    (Err(invalid("Claude turn task panicked")), None)
                }
            };
            // Terminal publication and admission share this boundary with resource cleanup.
            let mut active = state.active.lock().await;
            *active = None;
            if let Some(event) = terminal {
                let emitted = serde_json::from_str(event.payload.get())
                    .map_err(invalid)
                    .and_then(|payload| state.bridge.emit(event.kind, payload));
                if let Err(error) = emitted {
                    result = Err(error);
                }
            }
            *state.bridge.events.lock().unwrap() = None;
            done.cancel();
            let _ = result_tx.send(result);
        });
        Ok(BackendTurn {
            request_id: Some(id),
            result: Box::pin(
                async move { result_rx.await.unwrap_or(Err(NanocodexError::TurnStopped)) },
            ),
        })
    }

    async fn drive(&self, mut turn: nanocodex::Turn) -> (Result<TurnResult>, Option<AgentEvent>) {
        let mut terminal = None;
        let mut event_error = None;
        let result = loop {
            enum Next {
                Event(Option<AgentEvent>),
                Result(Box<Result<TurnResult>>),
            }
            match poll_fn(|cx| {
                if let Poll::Ready(event) = Pin::new(&mut turn).poll_next(cx) {
                    return Poll::Ready(Next::Event(event));
                }
                Pin::new(&mut turn)
                    .poll(cx)
                    .map(|result| Next::Result(Box::new(result)))
            })
            .await
            {
                Next::Event(Some(event)) => {
                    if let Err(error) = self.forward(event, &mut terminal) {
                        event_error = Some(error);
                        let _ = turn.control().cancel().await;
                    }
                }
                Next::Event(None) => break (&mut turn).await,
                Next::Result(result) => break *result,
            }
        };
        while let Some(event) = StreamExt::next(&mut turn).await {
            if let Err(error) = self.forward(event, &mut terminal) {
                event_error = Some(error);
            }
        }
        let mut observer = Observer {
            bridge: &self.bridge,
            error: None,
        };
        if result.is_err() || event_error.is_some() {
            self.bridge
                .runtime
                .control()
                .cancel_with_updates(&mut observer)
                .await;
        } else {
            // Each published turn owns its Code Mode producers until they quiesce.
            self.bridge
                .runtime
                .control()
                .cancel_turn_with_updates(&mut observer)
                .await;
        }
        if let Some(error) = observer.error {
            event_error = Some(invalid(error));
        }
        (event_error.map_or(result, Err), terminal)
    }

    fn forward(&self, event: AgentEvent, terminal: &mut Option<AgentEvent>) -> Result<()> {
        if event.kind.is_terminal() {
            *terminal = Some(event);
            return Ok(());
        }
        let payload: Value = serde_json::from_str(event.payload.get()).map_err(invalid)?;
        self.bridge.emit(event.kind, payload.clone())?;
        if event.kind == AgentEventKind::ToolCall
            && let Some(id) = payload.get("call_id").and_then(Value::as_str)
        {
            self.bridge.mark_started(id);
        }
        Ok(())
    }

    async fn control(&self, key: BackendTurnKey) -> Result<Active> {
        self.active
            .lock()
            .await
            .as_ref()
            .filter(|active| active.key == key)
            .cloned()
            .ok_or(NanocodexError::TurnNotCancellable)
    }

    async fn idle(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Active>>> {
        let active = self.active.lock().await;
        if self.stopped.load(Ordering::Acquire) {
            return Err(NanocodexError::AgentStopped);
        }
        if active.is_some() {
            return Err(invalid("Claude operation requires an idle agent"));
        }
        Ok(active)
    }

    async fn stop(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        let _shutdown = self.shutdown.lock().await;
        // Native shutdown cancels compaction, which holds the admission lock.
        let result = self.native.shutdown().await;
        let active = self.active.lock().await.clone();
        if let Some(active) = active {
            active.done.cancelled().await;
        }
        self.bridge.runtime.control().cancel().await;
        result
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = self.0.clone();
            runtime.spawn(async move {
                let _ = state.stop().await;
            });
        }
    }
}

impl LifecycleBackend for Driver {
    fn harness_family(&self) -> HarnessFamily {
        HarnessFamily::Claude
    }
    fn submit(&self, request: BackendPrompt) -> BackendFuture<Result<BackendTurn>> {
        let state = self.0.clone();
        Box::pin(async move {
            // Admission remains owned even if the caller drops its prompt future.
            tokio::spawn(state.submit(request)).await.map_err(invalid)?
        })
    }
    fn route(&self, _request: BackendPrompt) -> BackendFuture<Result<BackendPromptRoute>> {
        Box::pin(async { Err(invalid("use Tact's turn queue and steering for Claude")) })
    }
    fn steer(&self, key: BackendTurnKey, prompt: Prompt) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move { state.control(key).await?.control.steer(prompt).await })
    }
    fn steer_with_id(
        &self,
        key: BackendTurnKey,
        id: String,
        prompt: Prompt,
    ) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            state
                .control(key)
                .await?
                .control
                .steer_with_id(id, prompt)
                .await
        })
    }
    fn withdraw_steer(&self, key: BackendTurnKey, id: String) -> BackendFuture<Result<bool>> {
        let state = self.0.clone();
        Box::pin(async move { state.control(key).await?.control.withdraw_steer(id).await })
    }
    fn cancel(&self, key: BackendTurnKey) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            let active = state.control(key).await?;
            let result = active.control.cancel().await;
            active.done.cancelled().await;
            result
        })
    }
    fn set_model(&self, model: nanocodex::Model) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _idle = state.idle().await?;
            state.native.set_model(model).await
        })
    }
    fn set_harness_model(&self, model: HarnessModel) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _idle = state.idle().await?;
            state.native.set_harness_model(model).await
        })
    }
    fn set_thinking(&self, thinking: Thinking) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _idle = state.idle().await?;
            state.native.set_thinking(thinking).await
        })
    }
    fn set_fast_mode(&self, enabled: bool) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _admission = state.active.lock().await;
            if state.stopped.load(Ordering::Acquire) {
                return Err(NanocodexError::AgentStopped);
            }
            state.native.set_fast_mode(enabled).await?;
            state.fast_mode.store(enabled, Ordering::Release);
            Ok(())
        })
    }
    fn compact(&self) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _idle = state.idle().await?;
            state.native.compact().await
        })
    }
    fn append_developer_message(&self, text: String) -> BackendFuture<Result<AgentSessionContext>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _idle = state.idle().await?;
            state.native.append_developer_message(text).await
        })
    }
    fn context(&self) -> BackendFuture<Result<AgentSessionContext>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _idle = state.idle().await?;
            state.native.context().await
        })
    }
    fn runtime_snapshot(&self) -> BackendFuture<Result<ChildSnapshot>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _idle = state.idle().await?;
            state.native.runtime_snapshot().await
        })
    }
    fn spawn(&self, options: SpawnOptions) -> BackendFuture<Result<(Nanocodex, AgentEvents)>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _admission = state.active.lock().await;
            if state.stopped.load(Ordering::Acquire) {
                return Err(NanocodexError::AgentStopped);
            }
            let spawn = state
                .spawn
                .as_ref()
                .ok_or_else(|| invalid("Claude clean spawn requires an agent recipe"))?;
            let owner = state
                .bridge
                .owner
                .get()
                .ok_or_else(|| invalid("Claude tool owner is unavailable"))?;
            let (model, thinking) = owner.settings().await?;
            let options = options.resolve(model, thinking)?;
            if options.selected_harness_model() != Some(model) {
                return Err(invalid(
                    "Claude clean spawn cannot change models; use spawn_agent",
                ));
            }
            spawn(
                AgentContext {
                    model,
                    thinking: options.selected_thinking().unwrap_or(thinking),
                },
                state.fast_mode.load(Ordering::Acquire),
            )
        })
    }
    fn fork(
        &self,
        _completed: Option<TurnResult>,
    ) -> BackendFuture<Result<(Nanocodex, AgentEvents)>> {
        Box::pin(async { Err(invalid("native Claude does not support forks")) })
    }
    fn flush(&self) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            let _idle = state.idle().await?;
            state.native.flush_rollout().await
        })
    }
    fn shutdown(&self) -> BackendFuture<Result<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            tokio::spawn(async move { state.stop().await })
                .await
                .map_err(invalid)?
        })
    }
}
