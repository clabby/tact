//! Registry, harness, and tool-flow tests that run against real child harnesses.

use super::{
    AgentDescriptor, AgentId, AgentStatus, AuthorityError, Registry, RootAgentAuthority, Subagents,
    TurnSlot, forward_events,
    state::{AgentReservation, ChildSession, RegistryState, complete_session},
};
use crate::{
    AgentContext, AgentUpdate, MessageDeliveryState, MessageDisposition, MessagePriority,
    MessagePurpose, Speed,
    error::{DeliveryFailure, SpawnError, SubagentError},
    output::OutputContract,
    test_support::{PendingService, pending_agent},
};
use nanocodex::{
    HarnessModel as Model, Model as CodexModel, Nanocodex, NanocodexError, OpenAi, ReasoningMode,
    Thinking,
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    time::timeout,
};

#[tokio::test]
async fn descendants_can_switch_providers_with_local_model_and_shared_effort_caps() {
    let (updates, _receiver) = mpsc::unbounded_channel();
    let registry = Registry::new(updates, 1);
    let codex = |model| Model::Codex(model);
    let denied = [
        (codex(CodexModel::Luna), codex(CodexModel::Sol)),
        (codex(CodexModel::Luna), codex(CodexModel::Astra)),
        (codex(CodexModel::Sol), codex(CodexModel::Astra)),
    ];
    for parent_model in crate::SUPPORTED_MODELS {
        let parent = registry.reserve("root").await.unwrap();
        let session_id = format!("parent-{parent_model}");
        {
            let mut state = registry.state.lock().await;
            insert_session(&mut state, "root", parent.id, &session_id, None);
            state
                .scopes
                .get_mut("root")
                .unwrap()
                .sessions
                .get_mut(&parent.id)
                .unwrap()
                .descriptor
                .model = parent_model;
        }
        let child = registry.reserve(&session_id).await.unwrap();
        for child_model in crate::SUPPORTED_MODELS {
            assert_eq!(
                child
                    .validate_child(
                        "ignored",
                        child_model,
                        Thinking::Medium,
                        ReasoningMode::Standard
                    )
                    .is_ok(),
                !denied.contains(&(parent_model, child_model))
            );
            assert!(
                child
                    .validate_child(
                        "ignored",
                        child_model,
                        Thinking::High,
                        ReasoningMode::Standard
                    )
                    .is_err()
            );
        }
    }
}

#[test]
fn agent_factory_enforces_thinking_cap() {
    let (updates, _receiver) = mpsc::unbounded_channel();
    let registry = Registry::new(updates, 1);
    let seen = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&seen);
    registry
        .set_agent_factory(
            Thinking::Medium,
            ReasoningMode::Standard,
            Speed::Standard,
            move |context: AgentContext, speed| {
                *captured.lock().unwrap() = Some((context.model, context.thinking, speed));
                Err(NanocodexError::InvalidRequest(
                    "stop after capture".to_owned(),
                ))
            },
        )
        .unwrap();
    registry.set_agent_speed(Speed::Ultrafast);

    for (maximum, requested, allowed) in [
        (Thinking::Medium, Thinking::Low, true),
        (Thinking::Medium, Thinking::Medium, true),
        (Thinking::Medium, Thinking::High, false),
        (Thinking::High, Thinking::Xhigh, false),
        (Thinking::Xhigh, Thinking::High, true),
        (Thinking::Max, Thinking::Max, true),
        (Thinking::Low, Thinking::Medium, false),
        (Thinking::Low, Thinking::Low, true),
        (Thinking::Max, Thinking::None, false),
    ] {
        registry.set_agent_max_thinking(maximum);
        let error = registry
            .spawn_agent(
                Model::Codex(CodexModel::Luna),
                requested,
                ReasoningMode::Standard,
            )
            .err()
            .expect("factory should stop after capture");
        let actual = seen.lock().unwrap().take();
        if allowed {
            assert!(matches!(error, SubagentError::Agent(_)));
            assert_eq!(
                actual,
                Some((Model::Codex(CodexModel::Luna), requested, Speed::Ultrafast))
            );
        } else {
            assert_eq!(actual, None);
            assert!(matches!(
                error,
                SubagentError::Spawn(
                    SpawnError::ThinkingExceedsMaximum { .. } | SpawnError::ThinkingDisabled
                )
            ));
        }
    }
}

#[tokio::test]
async fn registered_parent_caps_and_turn_context_follow_its_descriptor() {
    let (updates, _receiver) = mpsc::unbounded_channel();
    let registry = Registry::new(updates, 1);
    let (captured, mut arguments) = mpsc::unbounded_channel();
    registry
        .set_agent_factory(
            Thinking::High,
            ReasoningMode::Standard,
            Speed::Standard,
            move |context: AgentContext, speed| {
                captured
                    .send((context.model, context.thinking, speed))
                    .unwrap();
                Err(NanocodexError::InvalidRequest(
                    "stop after capture".to_owned(),
                ))
            },
        )
        .unwrap();
    let parent = registry.reserve("root").await.unwrap();
    parent
        .validate_child(
            Model::Codex(CodexModel::Astra).as_str(),
            Model::Codex(CodexModel::Sol),
            Thinking::Medium,
            ReasoningMode::Standard,
        )
        .unwrap();
    {
        let mut state = registry.state.lock().await;
        insert_session(&mut state, "root", parent.id, "sol-child", None);
    }
    let reservation = registry.reserve("sol-child").await.unwrap();
    assert_eq!(reservation.parent, Some(parent.id));
    for (model, thinking, allowed) in [
        (Model::Codex(CodexModel::Astra), Thinking::Medium, false),
        (Model::Codex(CodexModel::Sol), Thinking::High, false),
        (Model::Codex(CodexModel::Sol), Thinking::Medium, true),
        (Model::Codex(CodexModel::Luna), Thinking::Low, true),
    ] {
        let result = reservation
            .validate_child(
                Model::Codex(CodexModel::Astra).as_str(),
                model,
                thinking,
                ReasoningMode::Standard,
            )
            .map_err(SubagentError::from)
            .and_then(|()| registry.spawn_agent(model, thinking, ReasoningMode::Standard));
        let error = result.err().unwrap();
        if allowed {
            assert!(matches!(error, SubagentError::Agent(_)));
            assert_eq!(
                arguments.try_recv().unwrap(),
                (model, thinking, Speed::Standard)
            );
        } else {
            assert!(arguments.try_recv().is_err());
            assert!(matches!(
                error,
                SubagentError::Spawn(
                    SpawnError::ModelExceedsParent { .. }
                        | SpawnError::ThinkingExceedsParent { .. }
                )
            ));
        }
    }
    for (cap, allowed) in [(Thinking::Low, false), (Thinking::High, true)] {
        registry.set_agent_max_thinking(cap);
        reservation
            .validate_child(
                "ignored for registered children",
                Model::Codex(CodexModel::Sol),
                Thinking::Medium,
                ReasoningMode::Standard,
            )
            .unwrap();
        let error = registry
            .spawn_agent(
                Model::Codex(CodexModel::Sol),
                Thinking::Medium,
                ReasoningMode::Standard,
            )
            .err()
            .unwrap();
        if allowed {
            assert!(matches!(error, SubagentError::Agent(_)));
            assert_eq!(
                arguments.try_recv().unwrap(),
                (
                    Model::Codex(CodexModel::Sol),
                    Thinking::Medium,
                    Speed::Standard
                )
            );
        } else {
            assert!(matches!(
                error,
                SubagentError::Spawn(SpawnError::ThinkingExceedsMaximum {
                    requested: Thinking::Medium,
                    maximum: Thinking::Low,
                })
            ));
            assert!(arguments.try_recv().is_err());
        }
    }
    for expected_token in [1, 2] {
        let (token, context) = registry
            .harness_turn_started("root", parent.id)
            .await
            .unwrap();
        assert_eq!(token, expected_token);
        assert_eq!(context.model, Model::Codex(CodexModel::Sol));
        assert_eq!(context.thinking, Thinking::Medium);
        let mut state = registry.state.lock().await;
        let session = state
            .scopes
            .get_mut("root")
            .unwrap()
            .sessions
            .get_mut(&parent.id)
            .unwrap();
        session.turn.finish();
        session.status = AgentStatus::Completed { output: json!({}) };
    }
}

#[tokio::test]
async fn harness_injects_descriptor_context_on_initial_and_reused_turns() {
    let (updates, _receiver) = mpsc::unbounded_channel();
    let registry = Arc::new(Registry::new(updates, 1));
    let (prompts, mut received) = mpsc::unbounded_channel();
    let openai = OpenAi::builder("test-key")
        .service(move || PendingService {
            called: Arc::new(Notify::new()),
            prompts: Some(prompts.clone()),
        })
        .build()
        .unwrap();
    let (agent, events) = Nanocodex::builder(openai)
        .model(nanocodex::Model::Sol)
        .thinking(Thinking::Medium)
        .reasoning_mode(ReasoningMode::Pro)
        .build()
        .unwrap();
    let reservation = registry.reserve("root").await.unwrap();
    insert_runtime_session(
        &registry,
        &reservation,
        None,
        ReasoningMode::Pro,
        agent,
        events,
    )
    .await;
    for token in [1, 2] {
        registry
            .launch_initial_turn(
                "root",
                reservation.id,
                format!("task {token}"),
                registry.reserve_turn().unwrap(),
            )
            .await
            .unwrap();
        let (model, thinking, prompt) = timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model, nanocodex::Model::Sol);
        assert_eq!(thinking, Thinking::Medium);
        assert!(prompt.contains(&format!("task {token}")), "{prompt}");
        assert!(prompt.contains(&format!("turn_token: {token}")));
        assert!(prompt.contains("<agent_context>"));
        assert!(
            prompt.contains(
                "This turn runs on sol with medium reasoning effort in pro reasoning mode."
            )
        );
        let directory = registry.directory("root", true, false).await;
        assert_eq!(directory[0].reasoning_mode, ReasoningMode::Pro);
        registry.interrupt("root", reservation.id).await.unwrap();
    }
    registry.close("root", reservation.id).await.unwrap();
}

fn test_contract() -> OutputContract {
    OutputContract {
        validator: jsonschema::validator_for(&json!({})).unwrap(),
        schema: "{}".to_owned(),
    }
}

#[tokio::test]
async fn root_agent_authority_rejects_registered_child_sessions() {
    let (updates, _updates_receiver) = mpsc::unbounded_channel();
    let registry = Arc::new(Registry::new(updates, 1));
    registry
        .state
        .lock()
        .await
        .root_by_session
        .insert("child".to_owned(), "root".to_owned());
    let guard = RootAgentAuthority {
        registry: Arc::downgrade(&registry),
    };

    assert!(guard.require_root("root").await.is_ok());
    assert!(guard.require_root("fork").await.is_ok());
    assert_eq!(
        guard.require_root("child").await,
        Err(AuthorityError::ChildSession)
    );
    drop(registry);
    assert_eq!(
        guard.require_root("root").await,
        Err(AuthorityError::RuntimeClosed)
    );
}

#[tokio::test]
async fn runtime_owned_agent_factory_does_not_keep_the_runtime_alive() {
    let (subagents, mut updates) = Subagents::new(1);
    let weak = subagents.downgrade();
    let factory_weak = weak.clone();
    subagents
        .set_agent_factory(
            Thinking::Medium,
            ReasoningMode::Standard,
            Speed::Standard,
            move |_, _| {
                let _ = &factory_weak;
                Err(NanocodexError::InvalidRequest("unused factory".to_owned()))
            },
        )
        .unwrap();

    drop(subagents);

    assert!(weak.registry.upgrade().is_none());
    assert!(updates.recv().await.is_none());
    assert_eq!(
        weak.root_agent_authority().require_root("root").await,
        Err(AuthorityError::RuntimeClosed)
    );
}

async fn insert_runtime_session(
    registry: &Arc<Registry>,
    reservation: &AgentReservation,
    parent: Option<AgentId>,
    reasoning_mode: ReasoningMode,
    agent: Nanocodex,
    events: nanocodex::AgentEvents,
) -> String {
    let session_id = events.request_id().to_owned();
    let descriptor = AgentDescriptor {
        id: reservation.id,
        session_id: session_id.clone(),
        model: Model::Codex(CodexModel::Sol),
        thinking: Thinking::Medium,
        reasoning_mode,
        role: format!("agent-{}", reservation.id),
        task: "wait forever".to_owned(),
        parent,
    };
    let (start_events, events_ready) = oneshot::channel();
    let event_task = forward_events(
        reservation.root_session_id.clone(),
        reservation.id,
        events,
        events_ready,
        Arc::downgrade(registry),
        registry.updates.clone(),
    );
    registry
        .insert(
            reservation.root_session_id.clone(),
            descriptor,
            agent,
            event_task,
            test_contract(),
        )
        .await
        .unwrap();
    start_events.send(()).unwrap();
    session_id
}

async fn insert_pending_runtime_session(
    registry: &Arc<Registry>,
    root_session_id: &str,
    parent: Option<AgentId>,
    called: Arc<Notify>,
) -> (AgentId, String) {
    let reservation = registry.reserve(root_session_id).await.unwrap();
    let id = reservation.id;
    let (agent, events) = pending_agent(called);
    let session_id = insert_runtime_session(
        registry,
        &reservation,
        parent,
        ReasoningMode::Standard,
        agent,
        events,
    )
    .await;
    (id, session_id)
}

async fn next_message_update(
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<super::ScopedAgentUpdate>,
) -> crate::AgentMessageUpdate {
    timeout(Duration::from_secs(5), async {
        loop {
            let update = updates
                .recv()
                .await
                .expect("the update channel should remain open");
            if let AgentUpdate::Message(message) = update.update {
                return message;
            }
        }
    })
    .await
    .expect("a message update should arrive")
}

async fn mark_reusable(registry: &Arc<Registry>, root_session_id: &str, id: AgentId) {
    registry
        .state
        .lock()
        .await
        .scopes
        .get_mut(root_session_id)
        .unwrap()
        .sessions
        .get_mut(&id)
        .unwrap()
        .status = AgentStatus::Completed {
        output: json!({ "report": "ready for another turn" }),
    };
}

fn test_session(id: AgentId, session_id: &str, parent: Option<AgentId>) -> ChildSession {
    let descriptor = AgentDescriptor {
        id,
        session_id: session_id.to_owned(),
        model: Model::Codex(CodexModel::Sol),
        thinking: Thinking::Medium,
        reasoning_mode: ReasoningMode::Standard,
        role: format!("agent-{id}"),
        task: "test lifecycle".to_owned(),
        parent,
    };
    ChildSession {
        descriptor,
        event_task: Some(tokio::spawn(async {})),
        harness: None,
        harness_task: None,
        status: AgentStatus::Pending,
        turn: TurnSlot::default(),
        output_validator: test_contract().validator,
        last_output: None,
    }
}

#[tokio::test]
async fn submitted_outputs_are_validated_and_completed_as_json() {
    let mut registry = RegistryState::default();
    let reservation = registry.reserve("main", None).unwrap();
    let mut session = test_session(reservation.id, "child-session", None);
    session.turn.start();
    session.status = AgentStatus::Running;
    session.output_validator = jsonschema::validator_for(&json!({
        "type": "object",
        "properties": { "answer": { "type": "integer" } },
        "required": ["answer"],
        "additionalProperties": false
    }))
    .unwrap();
    registry
        .insert(
            reservation.root_session_id,
            reservation.id,
            session.descriptor.session_id.clone(),
            session,
        )
        .unwrap();

    let invalid = registry.submit_result("child-session", 1, json!({ "answer": "42" }));
    assert!(matches!(
        invalid,
        Err(SubagentError::OutputMismatch { violations }) if violations.len() == 1
    ));
    registry
        .submit_result("child-session", 1, json!({ "answer": 42 }))
        .unwrap();
    assert!(matches!(
        registry.submit_result("child-session", 1, json!({ "answer": 43 })),
        Err(SubagentError::AlreadySubmitted)
    ));

    let session = registry
        .scopes
        .get_mut("main")
        .unwrap()
        .sessions
        .get_mut(&reservation.id)
        .unwrap();
    let output = session.turn.finish().flatten();
    let status = complete_session(session, output);

    assert_eq!(
        status,
        AgentStatus::Completed {
            output: json!({ "answer": 42 })
        }
    );
    assert_eq!(session.last_output, Some(json!({ "answer": 42 })));
}

#[test]
fn root_cannot_submit_a_subagent_result() {
    let mut registry = RegistryState::default();

    let error = registry.submit_result("main", 1, json!({ "report": "no" }));

    assert!(matches!(error, Err(SubagentError::NotSubagent)));
}

#[tokio::test]
async fn successful_turn_without_submission_fails_completion() {
    let mut session = test_session(AgentId::new(1), "child-session", None);

    let status = complete_session(&mut session, None);

    assert!(matches!(status, AgentStatus::Failed { .. }));
    assert_eq!(session.last_output, None);
}

#[tokio::test]
async fn agent_directory_keeps_results_after_reuse_and_close() {
    let (registry, _control, _updates) = super::channel(32);
    let reservation = registry.reserve("main").await.unwrap();
    let mut session = test_session(reservation.id, "child-session", None);
    session.status = AgentStatus::Completed {
        output: json!({ "report": "completed work" }),
    };
    session.last_output = Some(json!({ "report": "completed work" }));
    registry
        .state
        .lock()
        .await
        .insert(
            reservation.root_session_id.clone(),
            reservation.id,
            session.descriptor.session_id.clone(),
            session,
        )
        .unwrap();

    let directory = serde_json::to_value(registry.directory("main", true, false).await).unwrap();
    assert_eq!(
        directory[0]["status"]["output"],
        json!({ "report": "completed work" })
    );
    assert!(directory[0].get("last_output").is_none());
    assert!(
        registry
            .wait("main", &[reservation.id], Duration::from_secs(1))
            .await
            .is_err()
    );

    for error in [
        NanocodexError::TurnCancelled,
        NanocodexError::InvalidRequest("failed follow-up".to_owned()),
    ] {
        registry
            .harness_turn_started("main", reservation.id)
            .await
            .unwrap();
        registry
            .harness_turn_finished("main", reservation.id, Err(error))
            .await;
        let directory =
            serde_json::to_value(registry.directory("main", true, false).await).unwrap();
        assert_eq!(
            directory[0]["last_output"],
            json!({ "report": "completed work" })
        );
        assert!(
            registry
                .wait("main", &[reservation.id], Duration::from_secs(1))
                .await
                .is_err()
        );
    }
    let summaries = registry.close("main", reservation.id).await.unwrap();

    assert_eq!(summaries[0].status, AgentStatus::Closed);
    assert_eq!(
        summaries[0].last_output,
        Some(json!({ "report": "completed work" }))
    );
    let directory = serde_json::to_value(registry.directory("main", true, false).await).unwrap();
    assert_eq!(directory[0]["status"]["state"], "closed");
    assert_eq!(
        directory[0]["last_output"],
        json!({ "report": "completed work" })
    );
    assert!(
        registry
            .wait("main", &[reservation.id], Duration::from_secs(1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn interrupt_and_close_stop_recursive_turns_and_preserve_continuation() {
    let (registry, _control, _updates) = super::channel(32);
    let parent_called = Arc::new(Notify::new());
    let child_called = Arc::new(Notify::new());
    let sibling_called = Arc::new(Notify::new());

    let parent = registry.reserve("main").await.unwrap();
    let (parent_agent, parent_events) = pending_agent(Arc::clone(&parent_called));
    let parent_session = insert_runtime_session(
        &registry,
        &parent,
        None,
        ReasoningMode::Standard,
        parent_agent,
        parent_events,
    )
    .await;
    registry
        .launch_initial_turn(
            &parent.root_session_id,
            parent.id,
            "parent work".to_owned(),
            registry.reserve_turn().unwrap(),
        )
        .await
        .unwrap();

    let child = registry.reserve(&parent_session).await.unwrap();
    let (child_agent, child_events) = pending_agent(Arc::clone(&child_called));
    insert_runtime_session(
        &registry,
        &child,
        Some(parent.id),
        ReasoningMode::Standard,
        child_agent,
        child_events,
    )
    .await;
    registry
        .launch_initial_turn(
            &child.root_session_id,
            child.id,
            "child work".to_owned(),
            registry.reserve_turn().unwrap(),
        )
        .await
        .unwrap();

    let sibling = registry.reserve("main").await.unwrap();
    let (sibling_agent, sibling_events) = pending_agent(Arc::clone(&sibling_called));
    insert_runtime_session(
        &registry,
        &sibling,
        None,
        ReasoningMode::Standard,
        sibling_agent,
        sibling_events,
    )
    .await;
    registry
        .launch_initial_turn(
            &sibling.root_session_id,
            sibling.id,
            "sibling work".to_owned(),
            registry.reserve_turn().unwrap(),
        )
        .await
        .unwrap();

    timeout(Duration::from_secs(5), parent_called.notified())
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child_called.notified())
        .await
        .unwrap();
    timeout(Duration::from_secs(5), sibling_called.notified())
        .await
        .unwrap();

    let (running, timed_out) = registry
        .wait("main", &[parent.id, child.id], Duration::from_millis(1))
        .await
        .unwrap();
    assert!(timed_out);
    assert!(
        running
            .iter()
            .all(|summary| summary.status == AgentStatus::Running)
    );

    let ids = [parent.id, child.id];
    let mut waiting = Box::pin(registry.wait("main", &ids, Duration::from_secs(5)));
    assert!(futures_util::poll!(&mut waiting).is_pending());

    let interrupted = registry.interrupt("main", parent.id).await.unwrap();
    assert_eq!(
        interrupted
            .iter()
            .map(|summary| (&summary.agent_id, &summary.status))
            .collect::<Vec<_>>(),
        [
            (&child.id, &AgentStatus::Interrupted),
            (&parent.id, &AgentStatus::Interrupted),
        ]
    );
    let (finished, timed_out) = waiting.await.unwrap();
    assert!(!timed_out);
    assert_eq!(finished.len(), 2);
    let ids = [parent.id, child.id, sibling.id];
    let mut rejected = Box::pin(registry.wait("main", &ids, Duration::from_secs(1)));
    let Poll::Ready(Err(error)) = futures_util::poll!(&mut rejected) else {
        panic!("a terminal member must reject the mixed wait immediately");
    };
    let SubagentError::AlreadyTerminal { terminal, active } = error else {
        panic!("expected an already-terminal rejection, got {error}");
    };
    assert_eq!(terminal, [parent.id, child.id]);
    assert_eq!(active, [sibling.id]);
    assert_eq!(
        registry
            .state
            .lock()
            .await
            .summaries("main", &[sibling.id])
            .unwrap()[0]
            .status,
        AgentStatus::Running
    );

    let receipt = registry
        .send_message(
            "main",
            parent.id,
            MessagePriority::Deferred,
            MessagePurpose::Delegate,
            None,
            "continue".to_owned(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.disposition, MessageDisposition::Started);
    timeout(Duration::from_secs(5), parent_called.notified())
        .await
        .unwrap();

    let closed = registry.close("main", parent.id).await.unwrap();
    assert_eq!(
        closed
            .iter()
            .map(|summary| (&summary.agent_id, &summary.status))
            .collect::<Vec<_>>(),
        [
            (&child.id, &AgentStatus::Closed),
            (&parent.id, &AgentStatus::Closed),
        ]
    );
    assert_eq!(registry.directory("main", true, false).await.len(), 3);

    let all_closed = registry.close_all("main").await.unwrap();
    assert_eq!(all_closed.len(), 3);
    assert!(
        all_closed
            .iter()
            .all(|summary| summary.status == AgentStatus::Closed)
    );
    let state = registry.state.lock().await;
    assert!(
        state.scopes["main"]
            .sessions
            .values()
            .all(|session| session.harness.is_none()
                && session.harness_task.is_none()
                && session.event_task.is_none())
    );
}

#[tokio::test]
async fn same_root_agents_can_message_across_sibling_branches() {
    let (registry, _control, mut updates) = super::channel(32);
    let sender_called = Arc::new(Notify::new());
    let target_called = Arc::new(Notify::new());
    let (_sender, sender_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::clone(&sender_called)).await;
    let (target, _target_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::clone(&target_called)).await;
    mark_reusable(&registry, "main", target).await;

    let receipt = registry
        .send_message(
            &sender_session,
            target,
            MessagePriority::Deferred,
            MessagePurpose::Coordinate,
            None,
            "Compare our findings before either of us edits.".to_owned(),
        )
        .await
        .unwrap();

    assert_eq!(receipt.disposition, MessageDisposition::Started);
    timeout(Duration::from_secs(5), target_called.notified())
        .await
        .unwrap();
    let update = next_message_update(&mut updates).await;
    assert_eq!(update.message_id, receipt.message_id);
    assert_eq!(update.thread.messages.len(), 1);
    assert_eq!(
        update.delivery,
        MessageDeliveryState::Admitted {
            disposition: MessageDisposition::Started,
        }
    );

    registry.close_all("main").await.unwrap();
}

#[tokio::test]
async fn pending_agents_cannot_receive_messages_before_their_initial_turn() {
    let (registry, _control, _updates) = super::channel(32);
    let (_sender, sender_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;
    let (target, _target_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;

    let error = registry
        .send_message(
            &sender_session,
            target,
            MessagePriority::Deferred,
            MessagePurpose::Coordinate,
            None,
            "Do not overtake the assigned initial task.".to_owned(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, SubagentError::RecipientPending(id) if id == target));
    registry.close_all("main").await.unwrap();
}

#[tokio::test]
async fn sibling_messages_cannot_take_management_authority() {
    let (registry, _control, _updates) = super::channel(32);
    let (_sender, sender_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;
    let (target, _target_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;

    let error = registry
        .send_message(
            &sender_session,
            target,
            MessagePriority::Deferred,
            MessagePurpose::Delegate,
            None,
            "Replace the sibling's assigned task.".to_owned(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, SubagentError::NotDescendant { .. }));
    registry.close_all("main").await.unwrap();
}

#[tokio::test]
async fn delegate_messages_replace_assigned_tasks_for_descendants() {
    let (registry, _control, _updates) = super::channel(32);
    let (parent, parent_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;
    let child_called = Arc::new(Notify::new());
    let (child, _child_session) =
        insert_pending_runtime_session(&registry, "main", Some(parent), Arc::clone(&child_called))
            .await;
    mark_reusable(&registry, "main", child).await;

    let receipt = registry
        .send_message(
            &parent_session,
            child,
            MessagePriority::Deferred,
            MessagePurpose::Delegate,
            None,
            "Own the parser tests and report every uncovered branch.".to_owned(),
        )
        .await
        .unwrap();

    assert_eq!(receipt.disposition, MessageDisposition::Started);
    timeout(Duration::from_secs(5), child_called.notified())
        .await
        .unwrap();
    let task = registry.state.lock().await.scopes["main"].sessions[&child]
        .descriptor
        .task
        .clone();
    assert_eq!(
        task,
        "Own the parser tests and report every uncovered branch."
    );
    registry.close_all("main").await.unwrap();
}

#[tokio::test]
async fn urgent_messages_steer_running_agents() {
    let (registry, _control, _updates) = super::channel(32);
    let (_sender, sender_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;
    let target_called = Arc::new(Notify::new());
    let (target, _target_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::clone(&target_called)).await;
    registry
        .launch_initial_turn(
            "main",
            target,
            "Keep working until interrupted.".to_owned(),
            registry.reserve_turn().unwrap(),
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(5), target_called.notified())
        .await
        .unwrap();

    let receipt = registry
        .send_message(
            &sender_session,
            target,
            MessagePriority::Urgent,
            MessagePurpose::Finding,
            None,
            "Stop duplicating the parser investigation.".to_owned(),
        )
        .await
        .unwrap();

    assert_eq!(receipt.disposition, MessageDisposition::Steered);
    registry.close_all("main").await.unwrap();
}

#[tokio::test]
async fn interruption_marks_queued_messages_as_failed() {
    let (registry, _control, mut updates) = super::channel(32);
    let (_sender, sender_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;
    let target_called = Arc::new(Notify::new());
    let (target, _target_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::clone(&target_called)).await;
    registry
        .launch_initial_turn(
            "main",
            target,
            "Keep working until interrupted.".to_owned(),
            registry.reserve_turn().unwrap(),
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(5), target_called.notified())
        .await
        .unwrap();

    let receipt = registry
        .send_message(
            &sender_session,
            target,
            MessagePriority::Deferred,
            MessagePurpose::Question,
            None,
            "What remains in your investigation?".to_owned(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.disposition, MessageDisposition::Queued);
    let admitted = next_message_update(&mut updates).await;
    assert_eq!(admitted.message_id, receipt.message_id);

    registry.interrupt("main", target).await.unwrap();

    let failed = next_message_update(&mut updates).await;
    assert_eq!(failed.message_id, receipt.message_id);
    assert!(matches!(
        failed.delivery,
        MessageDeliveryState::Failed { .. }
    ));
    registry.close_all("main").await.unwrap();
}

#[tokio::test]
async fn queued_delegation_changes_the_task_only_when_delivery_starts() {
    let (registry, _control, _updates) = super::channel(32);
    let target_called = Arc::new(Notify::new());
    let (target, _target_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::clone(&target_called)).await;
    registry
        .launch_initial_turn(
            "main",
            target,
            "Keep working until interrupted.".to_owned(),
            registry.reserve_turn().unwrap(),
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(5), target_called.notified())
        .await
        .unwrap();

    let receipt = registry
        .send_message(
            "main",
            target,
            MessagePriority::Deferred,
            MessagePurpose::Delegate,
            None,
            "This task must not become current before delivery.".to_owned(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.disposition, MessageDisposition::Queued);
    let task = registry.state.lock().await.scopes["main"].sessions[&target]
        .descriptor
        .task
        .clone();
    assert_eq!(task, "wait forever");

    registry.interrupt("main", target).await.unwrap();
    let task = registry.state.lock().await.scopes["main"].sessions[&target]
        .descriptor
        .task
        .clone();
    assert_eq!(task, "wait forever");
    registry.close_all("main").await.unwrap();
}

#[tokio::test]
async fn message_priorities_have_independent_mailbox_bounds() {
    let (registry, _control, _updates) = super::channel(0);
    let (_sender, sender_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;
    let (target, _target_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;
    mark_reusable(&registry, "main", target).await;

    for index in 0..crate::harness::DEFERRED_CAPACITY {
        let receipt = registry
            .send_message(
                &sender_session,
                target,
                MessagePriority::Deferred,
                MessagePurpose::Coordinate,
                None,
                format!("queued message {index}"),
            )
            .await
            .unwrap();
        assert_eq!(receipt.disposition, MessageDisposition::Queued);
    }
    let normal_error = registry
        .send_message(
            &sender_session,
            target,
            MessagePriority::Deferred,
            MessagePurpose::Coordinate,
            None,
            "one message too many".to_owned(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        normal_error,
        SubagentError::Delivery(DeliveryFailure::QueueFull {
            priority: MessagePriority::Deferred,
            agent,
        }) if agent == target
    ));

    for index in 0..crate::harness::URGENT_CAPACITY {
        let receipt = registry
            .send_message(
                &sender_session,
                target,
                MessagePriority::Urgent,
                MessagePurpose::Coordinate,
                None,
                format!("urgent queued message {index}"),
            )
            .await
            .unwrap();
        assert_eq!(receipt.disposition, MessageDisposition::Queued);
    }
    let urgent_error = registry
        .send_message(
            &sender_session,
            target,
            MessagePriority::Urgent,
            MessagePurpose::Coordinate,
            None,
            "one urgent message too many".to_owned(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        urgent_error,
        SubagentError::Delivery(DeliveryFailure::QueueFull {
            priority: MessagePriority::Urgent,
            agent,
        }) if agent == target
    ));
    registry.close_all("main").await.unwrap();
}

#[tokio::test]
async fn messages_do_not_cross_root_scopes() {
    let (registry, _control, _updates) = super::channel(32);
    let (target, _target_session) =
        insert_pending_runtime_session(&registry, "main", None, Arc::new(Notify::new())).await;

    let error = registry
        .send_message(
            "other-root",
            target,
            MessagePriority::Deferred,
            MessagePurpose::Coordinate,
            None,
            "This must not reach the main tree.".to_owned(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, SubagentError::UnknownAgent(id) if id == target));
    registry.close_all("main").await.unwrap();
}

fn insert_session(
    registry: &mut RegistryState,
    root_session_id: &str,
    id: AgentId,
    session_id: &str,
    parent: Option<AgentId>,
) {
    let session = test_session(id, session_id, parent);
    registry
        .insert(
            root_session_id.to_owned(),
            id,
            session.descriptor.session_id.clone(),
            session,
        )
        .unwrap();
}

#[test]
fn root_sessions_number_subagents_independently() {
    let mut registry = RegistryState::default();

    let main = registry.reserve("main", None).unwrap();
    let fork = registry.reserve("fork", None).unwrap();

    assert_eq!(main.id, AgentId::new(1));
    assert_eq!(main.root_session_id, "main");
    assert_eq!(fork.id, AgentId::new(1));
    assert_eq!(fork.root_session_id, "fork");
}

#[test]
fn descendant_sessions_use_their_root_namespace() {
    let mut registry = RegistryState::default();
    let root = registry.reserve("main", None).unwrap();
    registry
        .root_by_session
        .insert("child".to_owned(), root.root_session_id);

    let descendant = registry.reserve("child", None).unwrap();

    assert_eq!(descendant.id, AgentId::new(2));
    assert_eq!(descendant.root_session_id, "main");
}

#[tokio::test]
async fn child_sessions_automatically_own_new_subagents() {
    let mut registry = RegistryState::default();
    let parent = registry.reserve("main", None).unwrap();
    insert_session(
        &mut registry,
        &parent.root_session_id,
        parent.id,
        "parent-session",
        None,
    );

    let child = registry.reserve_for("parent-session").unwrap();

    assert_eq!(child.root_session_id, "main");
    assert_eq!(child.parent, Some(parent.id));
}

#[tokio::test]
async fn subagents_can_manage_descendants_but_not_siblings_or_ancestors() {
    let mut registry = RegistryState::default();
    let first = registry.reserve("main", None).unwrap();
    insert_session(
        &mut registry,
        &first.root_session_id,
        first.id,
        "first-session",
        None,
    );
    let second = registry.reserve("main", None).unwrap();
    insert_session(
        &mut registry,
        &second.root_session_id,
        second.id,
        "second-session",
        None,
    );
    let child = registry.reserve_for("first-session").unwrap();
    insert_session(
        &mut registry,
        &child.root_session_id,
        child.id,
        "child-session",
        Some(first.id),
    );

    assert!(registry.summaries("first-session", &[child.id]).is_ok());
    assert!(registry.summaries("first-session", &[second.id]).is_err());
    assert!(registry.summaries("second-session", &[child.id]).is_err());
    assert!(registry.summaries("child-session", &[first.id]).is_err());
    assert_eq!(registry.summaries("main", &[child.id]).unwrap().len(), 1);
}

#[tokio::test]
async fn directory_separates_same_tree_messaging_from_management() {
    let mut registry = RegistryState::default();
    let parent = registry.reserve("main", None).unwrap();
    insert_session(
        &mut registry,
        &parent.root_session_id,
        parent.id,
        "parent-session",
        None,
    );
    let child = registry.reserve("main", Some(parent.id)).unwrap();
    insert_session(
        &mut registry,
        &child.root_session_id,
        child.id,
        "child-session",
        Some(parent.id),
    );
    let sibling = registry.reserve("main", None).unwrap();
    insert_session(
        &mut registry,
        &sibling.root_session_id,
        sibling.id,
        "sibling-session",
        None,
    );

    for session in registry
        .scopes
        .get_mut("main")
        .unwrap()
        .sessions
        .values_mut()
    {
        session.status = AgentStatus::Completed {
            output: json!({ "report": "ready" }),
        };
    }

    let directory = registry.directory("parent-session", true, false);

    assert_eq!(
        directory
            .iter()
            .map(|entry| (entry.agent_id, entry.can_message, entry.can_manage))
            .collect::<Vec<_>>(),
        [(child.id, true, true), (sibling.id, true, false)]
    );
}

#[tokio::test]
async fn child_spawn_is_rejected_when_parent_closes_after_reservation() {
    let mut registry = RegistryState::default();
    let parent = registry.reserve("main", None).unwrap();
    insert_session(
        &mut registry,
        &parent.root_session_id,
        parent.id,
        "parent-session",
        None,
    );
    let child = registry.reserve_for("parent-session").unwrap();
    registry
        .scopes
        .get_mut("main")
        .unwrap()
        .sessions
        .get_mut(&parent.id)
        .unwrap()
        .status = AgentStatus::Closed;
    let session = test_session(child.id, "child-session", Some(parent.id));

    let result = registry.insert(
        child.root_session_id,
        child.id,
        session.descriptor.session_id.clone(),
        session,
    );

    assert!(result.is_err());
}

#[tokio::test]
async fn subtree_shutdown_order_includes_every_descendant_before_its_parent() {
    let mut registry = RegistryState::default();
    let parent = registry.reserve("main", None).unwrap();
    insert_session(
        &mut registry,
        &parent.root_session_id,
        parent.id,
        "parent-session",
        None,
    );
    let child = registry.reserve("parent-session", Some(parent.id)).unwrap();
    insert_session(
        &mut registry,
        &child.root_session_id,
        child.id,
        "child-session",
        Some(parent.id),
    );
    let grandchild = registry.reserve("child-session", Some(child.id)).unwrap();
    insert_session(
        &mut registry,
        &grandchild.root_session_id,
        grandchild.id,
        "grandchild-session",
        Some(child.id),
    );

    assert_eq!(
        registry.subtree_shutdown_order("main", parent.id).unwrap(),
        [grandchild.id, child.id, parent.id]
    );
}

#[tokio::test]
async fn root_sessions_cannot_access_each_others_subagents() {
    let mut registry = RegistryState::default();
    let main = registry.reserve("main", None).unwrap();
    let session = test_session(main.id, "main-child", None);
    registry
        .insert(
            main.root_session_id,
            main.id,
            session.descriptor.session_id.clone(),
            session,
        )
        .unwrap();

    assert!(registry.summaries("fork", &[main.id]).is_err());
    assert!(registry.reserve("fork", Some(main.id)).is_err());
}
