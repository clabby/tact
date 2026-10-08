//! Waiting on, interrupting, and closing child subtrees within bounded deadlines.

use super::{
    Registry,
    state::{AgentSummary, ChildSession, RegistryState},
};
use crate::{
    error::{RegistryEntry, ShutdownPhase, SubagentError},
    harness::HarnessHandle,
    model::{AgentId, AgentStatus, AgentUpdate},
};
use futures_util::future::join_all;
use std::time::Duration;
use tokio::{
    task::JoinHandle,
    time::{Instant, timeout_at},
};

pub(super) const AGENT_STOP_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct CloseRequest {
    pub(crate) root_session_id: String,
    pub(crate) ids: Vec<AgentId>,
    pub(crate) harnesses: Vec<HarnessHandle>,
    pub(crate) status_updates: Vec<(AgentId, AgentStatus)>,
}

pub(crate) struct ClosedSessions {
    pub(crate) summaries: Vec<AgentSummary>,
    pub(crate) harness_tasks: Vec<JoinHandle<()>>,
    pub(crate) event_tasks: Vec<JoinHandle<()>>,
}

impl RegistryState {
    pub(super) fn request_interrupt(
        &mut self,
        session_id: &str,
        id: AgentId,
    ) -> Result<(String, Vec<AgentId>, Vec<HarnessHandle>), SubagentError> {
        let root_session_id = self.authorize(session_id, id)?;
        let ids = self.subtree_shutdown_order(&root_session_id, id)?;
        let harnesses = self.harnesses(&root_session_id, &ids, false)?;
        Ok((root_session_id, ids, harnesses))
    }

    pub(super) fn request_close(
        &mut self,
        session_id: &str,
        id: AgentId,
    ) -> Result<CloseRequest, SubagentError> {
        let root_session_id = self.authorize(session_id, id)?;
        let ids = self.subtree_shutdown_order(&root_session_id, id)?;
        let harnesses = self.harnesses(&root_session_id, &ids, true)?;
        let status_updates = ids
            .iter()
            .copied()
            .map(|id| (id, AgentStatus::Closing))
            .collect();
        Ok(CloseRequest {
            root_session_id,
            ids,
            harnesses,
            status_updates,
        })
    }

    pub(super) fn request_close_all(
        &mut self,
        session_id: &str,
    ) -> Result<CloseRequest, SubagentError> {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let Some(scope) = self.scopes.get(&root_session_id) else {
            return Ok(CloseRequest {
                root_session_id,
                ids: Vec::new(),
                harnesses: Vec::new(),
                status_updates: Vec::new(),
            });
        };
        let ids = scope.topology.all_postorder();
        let harnesses = self.harnesses(&root_session_id, &ids, true)?;
        let status_updates = ids
            .iter()
            .copied()
            .map(|id| (id, AgentStatus::Closing))
            .collect();
        Ok(CloseRequest {
            root_session_id,
            ids,
            harnesses,
            status_updates,
        })
    }

    pub(super) fn request_interrupt_all(
        &mut self,
        session_id: &str,
    ) -> (String, Vec<AgentId>, Vec<HarnessHandle>) {
        let root_session_id = self.root_session_id(session_id).to_owned();
        let ids = self
            .scopes
            .get(&root_session_id)
            .map(|scope| scope.topology.ids())
            .unwrap_or_default();
        let harnesses = self
            .harnesses(&root_session_id, &ids, false)
            .unwrap_or_default();
        (root_session_id, ids, harnesses)
    }

    pub(super) fn harnesses(
        &mut self,
        root_session_id: &str,
        ids: &[AgentId],
        closing: bool,
    ) -> Result<Vec<HarnessHandle>, SubagentError> {
        let scope = self
            .scopes
            .get_mut(root_session_id)
            .ok_or(SubagentError::StateDisappeared(RegistryEntry::Scope))?;
        let mut harnesses = Vec::new();
        for id in ids {
            let session = scope
                .sessions
                .get_mut(id)
                .ok_or(SubagentError::UnknownAgent(*id))?;
            if closing {
                session.status = AgentStatus::Closing;
            }
            harnesses.extend(session.harness.iter().cloned());
        }
        Ok(harnesses)
    }

    pub(super) fn finish_close(
        &mut self,
        root_session_id: &str,
        ids: &[AgentId],
    ) -> Result<ClosedSessions, SubagentError> {
        let scope = self
            .scopes
            .get_mut(root_session_id)
            .ok_or(SubagentError::StateDisappeared(RegistryEntry::Scope))?;
        let mut harness_tasks = Vec::new();
        let mut event_tasks = Vec::new();
        for id in ids {
            let session = scope
                .sessions
                .get_mut(id)
                .ok_or(SubagentError::UnknownAgent(*id))?;
            if session.turn.is_active() {
                return Err(SubagentError::StillRunning(*id));
            }
            session.harness = None;
            harness_tasks.extend(session.harness_task.take());
            event_tasks.extend(session.event_task.take());
            session.status = AgentStatus::Closed;
        }
        let summaries = ids
            .iter()
            .filter_map(|id| scope.sessions.get(id).map(ChildSession::summary))
            .collect();
        Ok(ClosedSessions {
            summaries,
            harness_tasks,
            event_tasks,
        })
    }

    pub(super) fn all_inactive(
        &self,
        root_session_id: &str,
        ids: &[AgentId],
    ) -> Result<bool, SubagentError> {
        let scope = self
            .scopes
            .get(root_session_id)
            .ok_or(SubagentError::StateDisappeared(RegistryEntry::Scope))?;
        Ok(ids.iter().all(|id| {
            scope
                .sessions
                .get(id)
                .is_some_and(|session| !session.turn.is_active())
        }))
    }

    pub(super) fn subtree_shutdown_order(
        &self,
        root_session_id: &str,
        id: AgentId,
    ) -> Result<Vec<AgentId>, SubagentError> {
        self.scopes
            .get(root_session_id)
            .ok_or(SubagentError::StateDisappeared(RegistryEntry::Scope))?
            .topology
            .subtree_postorder(id)
    }
}

impl Registry {
    pub(crate) async fn wait(
        &self,
        session_id: &str,
        ids: &[AgentId],
        duration: Duration,
    ) -> Result<(Vec<AgentSummary>, bool), SubagentError> {
        if ids.is_empty() {
            return Err(SubagentError::EmptyWaitSet);
        }
        let mut revision = self.revision.subscribe();
        let deadline = Instant::now() + duration;
        {
            let summaries = self.state.lock().await.summaries(session_id, ids)?;
            let terminal = summaries
                .iter()
                .filter(|summary| summary.status.is_wait_terminal())
                .map(|summary| summary.agent_id)
                .collect::<Vec<_>>();
            if !terminal.is_empty() {
                let active = summaries
                    .iter()
                    .filter(|summary| summary.status.is_active())
                    .map(|summary| summary.agent_id)
                    .collect();
                return Err(SubagentError::AlreadyTerminal { terminal, active });
            }
        }
        loop {
            let summaries = self.state.lock().await.summaries(session_id, ids)?;
            if summaries
                .iter()
                .any(|summary| summary.status.is_wait_terminal())
            {
                return Ok((summaries, false));
            }
            if timeout_at(deadline, revision.changed()).await.is_err() {
                let summaries = self.state.lock().await.summaries(session_id, ids)?;
                return Ok((summaries, true));
            }
        }
    }

    pub(crate) async fn interrupt(
        &self,
        session_id: &str,
        id: AgentId,
    ) -> Result<Vec<AgentSummary>, SubagentError> {
        let _message_guard = self.message_lock.lock().await;
        let (root_session_id, ids, harnesses) = {
            let mut state = self.state.lock().await;
            state.request_interrupt(session_id, id)?
        };
        self.changed();
        let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
        self.interrupt_harnesses(&root_session_id, &ids, harnesses, deadline)
            .await?;
        self.state
            .lock()
            .await
            .summaries_in_scope(&root_session_id, &ids)
    }

    pub(crate) async fn close(
        &self,
        session_id: &str,
        id: AgentId,
    ) -> Result<Vec<AgentSummary>, SubagentError> {
        let _message_guard = self.message_lock.lock().await;
        let CloseRequest {
            root_session_id,
            ids,
            harnesses,
            status_updates,
        } = {
            let mut state = self.state.lock().await;
            state.request_close(session_id, id)?
        };
        for (id, status) in status_updates {
            self.send(&root_session_id, AgentUpdate::Status { id, status });
        }
        self.changed();
        self.stop_and_close(root_session_id, ids, harnesses).await
    }

    pub(super) async fn close_all(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentSummary>, SubagentError> {
        let _message_guard = self.message_lock.lock().await;
        let CloseRequest {
            root_session_id,
            ids,
            harnesses,
            status_updates,
        } = {
            let mut state = self.state.lock().await;
            state.request_close_all(session_id)?
        };
        for (id, status) in status_updates {
            self.send(&root_session_id, AgentUpdate::Status { id, status });
        }
        self.changed();
        self.stop_and_close(root_session_id, ids, harnesses).await
    }

    pub(super) async fn stop_and_close(
        &self,
        root_session_id: String,
        ids: Vec<AgentId>,
        harnesses: Vec<HarnessHandle>,
    ) -> Result<Vec<AgentSummary>, SubagentError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
        let closing_result = self.close_harnesses(harnesses, deadline).await;
        self.wait_until_inactive(&root_session_id, &ids, deadline)
            .await?;
        // Closing an already-finished driver can race with its natural shutdown.
        // Once every harness reports inactive, command errors no longer identify
        // a live resource and must not prevent task handles from being joined.
        drop(closing_result);
        let ClosedSessions {
            summaries,
            harness_tasks,
            event_tasks,
        } = self
            .state
            .lock()
            .await
            .finish_close(&root_session_id, &ids)?;
        for summary in &summaries {
            self.send(
                &root_session_id,
                AgentUpdate::Status {
                    id: summary.agent_id,
                    status: AgentStatus::Closed,
                },
            );
        }
        self.changed();
        self.wait_for_tasks(harness_tasks, deadline, ShutdownPhase::JoinHarnesses)
            .await?;
        self.wait_for_tasks(event_tasks, deadline, ShutdownPhase::JoinEventStreams)
            .await?;
        Ok(summaries)
    }

    pub(super) async fn cancel_all(&self, session_id: &str) {
        let _message_guard = self.message_lock.lock().await;
        let (root_session_id, ids, harnesses) = {
            let mut state = self.state.lock().await;
            state.request_interrupt_all(session_id)
        };
        self.changed();
        let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
        drop(
            self.interrupt_harnesses(&root_session_id, &ids, harnesses, deadline)
                .await,
        );
    }

    pub(super) async fn interrupt_harnesses(
        &self,
        root_session_id: &str,
        ids: &[AgentId],
        harnesses: Vec<HarnessHandle>,
        deadline: Instant,
    ) -> Result<(), SubagentError> {
        let interruption = async move {
            let results = join_all(
                harnesses
                    .into_iter()
                    .map(|harness| async move { harness.interrupt().await }),
            )
            .await;
            first_error(results)
        };
        let interruption_result = timeout_at(deadline, interruption)
            .await
            .map_err(|_| SubagentError::ShutdownTimedOut(ShutdownPhase::InterruptHarnesses))?;
        self.wait_until_inactive(root_session_id, ids, deadline)
            .await?;
        drop(interruption_result);
        Ok(())
    }

    pub(super) async fn close_harnesses(
        &self,
        harnesses: Vec<HarnessHandle>,
        deadline: Instant,
    ) -> Result<(), SubagentError> {
        let closing = async move {
            let results = join_all(
                harnesses
                    .into_iter()
                    .map(|harness| async move { harness.close().await }),
            )
            .await;
            first_error(results)
        };
        timeout_at(deadline, closing)
            .await
            .map_err(|_| SubagentError::ShutdownTimedOut(ShutdownPhase::CloseHarnesses))?
    }

    pub(super) async fn wait_for_tasks(
        &self,
        mut tasks: Vec<JoinHandle<()>>,
        deadline: Instant,
        phase: ShutdownPhase,
    ) -> Result<(), SubagentError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let completion = join_all(tasks.iter_mut());
        match timeout_at(deadline, completion).await {
            Ok(results) => results
                .into_iter()
                .find_map(Result::err)
                .map_or(Ok(()), |source| {
                    Err(SubagentError::ShutdownTask { phase, source })
                }),
            Err(_) => {
                for task in tasks {
                    task.abort();
                }
                Err(SubagentError::ShutdownTimedOut(phase))
            }
        }
    }

    pub(super) async fn wait_until_inactive(
        &self,
        root_session_id: &str,
        ids: &[AgentId],
        deadline: Instant,
    ) -> Result<(), SubagentError> {
        let mut revision = self.revision.subscribe();
        loop {
            if self.state.lock().await.all_inactive(root_session_id, ids)? {
                return Ok(());
            }
            timeout_at(deadline, revision.changed())
                .await
                .map_err(|_| SubagentError::ShutdownTimedOut(ShutdownPhase::StopTurns))?
                .map_err(|_| SubagentError::RuntimeClosed)?;
        }
    }
}

pub(super) fn first_error(results: Vec<Result<(), SubagentError>>) -> Result<(), SubagentError> {
    results.into_iter().find(Result::is_err).unwrap_or(Ok(()))
}
