//! Pane settings and configuration: effort, speed, subagent limits, and configuration reloads.
//!
//! Effort and speed changes are persisted on a blocking task first, then applied by the worker,
//! which reports back before the pane's journal and runtime record the new setting. Terminal input
//! stays detached until the worker has reported, so the next keystroke sees the applied setting.

use super::{
    EventLoop,
    background::{TaskKind, TaskOutput},
};
use crate::{
    app::{
        config::{Config, ReasoningEffort, ReasoningMode, Setting, Speed},
        error::Result,
    },
    core::{
        pane::PaneId,
        protocol::CommandError,
        transcript::{EffortChanged, LocalEvent, SpeedChanged},
        worker::WorkerCommand,
    },
    tui::components::AppEvent,
};
use crossterm::event::EventStream;
use nanocodex::NanocodexError;
use std::time::Instant;
use tact_subagents::Subagents;

/// A persisted effort change that the worker has yet to apply.
pub(super) struct EffortUpdate {
    pane: PaneId,
    to: ReasoningEffort,
    preferred_reasoning_mode: ReasoningMode,
}

/// A persisted speed change that the worker has yet to apply.
pub(super) struct SpeedUpdate {
    pane: PaneId,
    speed: Speed,
}

impl EventLoop {
    /// Persists a pane's new effort and reasoning mode. Only the main pane's effort becomes the
    /// configured default.
    pub(super) fn set_effort(
        &mut self,
        pane: PaneId,
        effort: ReasoningEffort,
        reasoning_mode: ReasoningMode,
    ) {
        self.input = None;
        let config = self.config.clone();
        let is_main = self.app.main_pane() == Some(pane);
        self.tasks.spawn_blocking(TaskKind::Effort, move || {
            let persisted = if is_main {
                config.persist(Setting::Thinking(effort))
            } else {
                Ok(())
            }
            .and_then(|()| config.persist(Setting::ReasoningMode(reasoning_mode)));
            TaskOutput::Effort(persisted.map(|()| EffortUpdate {
                pane,
                to: effort,
                preferred_reasoning_mode: reasoning_mode,
            }))
        });
    }

    pub(super) fn on_effort_persisted(&mut self, update: EffortUpdate) -> Result<()> {
        self.config
            .set_reasoning_mode(update.preferred_reasoning_mode);
        if self.app.main_pane() == Some(update.pane) {
            self.config.set_thinking(update.to);
            for runtime in self.panes.runtimes() {
                runtime.subagent_control.set_max_thinking(update.to.into());
            }
        }
        self.app
            .set_preferred_reasoning_mode(update.preferred_reasoning_mode);
        self.worker.send(WorkerCommand::SetThinking {
            pane: update.pane,
            effort: update.to,
        })
    }

    /// Records the effort the worker applied, or reports why it could not.
    pub(super) async fn on_effort_updated(
        &mut self,
        pane: PaneId,
        effort: ReasoningEffort,
        result: std::result::Result<(), NanocodexError>,
    ) -> Result<()> {
        let runtime = self.panes.runtime(pane)?;
        let previous = runtime.settings.effort;
        if let Err(error) = result {
            self.input.get_or_insert_with(EventStream::new);
            return self
                .apply(AppEvent::EffortUpdateFailed {
                    pane,
                    effort: previous,
                    error: format!("Could not change effort: {error}"),
                })
                .await;
        }
        let journal = runtime.journal_mut()?;
        if journal.is_empty() {
            journal.set_initial_effort(effort);
        } else {
            let record = journal.append_local(LocalEvent::EffortChanged(EffortChanged {
                from: previous,
                to: effort,
            }))?;
            self.show(AppEvent::Transcript { pane, record });
        }
        self.panes.runtime(pane)?.settings.effort = effort;
        self.input.get_or_insert_with(EventStream::new);
        self.scheduler.request_immediate(Instant::now());
        Ok(())
    }

    /// Persists a pane's new speed. Only the main pane's speed becomes the configured default.
    pub(super) fn set_speed(&mut self, pane: PaneId, speed: Speed) {
        self.input = None;
        let config = (self.app.main_pane() == Some(pane)).then(|| self.config.clone());
        self.tasks.spawn_blocking(TaskKind::Speed, move || {
            let persisted = config.map_or(Ok(()), |config| config.persist(Setting::Speed(speed)));
            TaskOutput::Speed(persisted.map(|()| SpeedUpdate { pane, speed }))
        });
    }

    pub(super) fn on_speed_persisted(&mut self, update: SpeedUpdate) -> Result<()> {
        self.worker.send(WorkerCommand::SetSpeed {
            pane: update.pane,
            speed: update.speed,
        })
    }

    /// Records the speed the worker applied and passes it on to the pane's subagents.
    pub(super) fn on_speed_updated(&mut self, pane: PaneId, speed: Speed) -> Result<()> {
        let runtime = self.panes.runtime(pane)?;
        let previous = runtime.settings.speed;
        let journal = runtime.journal_mut()?;
        if journal.is_empty() {
            journal.set_initial_speed(speed);
        } else {
            let record = journal.append_local(LocalEvent::SpeedChanged(SpeedChanged {
                from: previous,
                to: speed,
            }))?;
            self.show(AppEvent::Transcript { pane, record });
        }
        let runtime = self.panes.runtime(pane)?;
        runtime.settings.speed = speed;
        runtime.subagent_control.set_speed(speed);
        if self.app.main_pane() == Some(pane) {
            self.config.set_speed(speed);
        }
        self.input.get_or_insert_with(EventStream::new);
        self.scheduler.request_immediate(Instant::now());
        Ok(())
    }

    pub(super) fn set_max_subagents(&mut self, limit: usize) -> Result<()> {
        self.config.persist(Setting::MaxSubagents(limit))?;
        self.config.set_max_subagents(limit);
        self.app.set_max_subagents(limit);
        for runtime in self.panes.runtimes() {
            runtime.subagent_control.set_max_concurrency(limit);
        }
        Ok(())
    }

    /// Reloads the configuration and applies what can change in-process. The outcome is shown in
    /// the terminal as a notification on `pane` and returned to a web caller.
    pub(super) fn reload_config(&mut self, pane: PaneId) -> std::result::Result<(), CommandError> {
        let reload = match self.config.reload() {
            Ok(reload) => reload,
            Err(error) => {
                return self.refuse_reload(pane, format!("Could not reload config: {error}"));
            }
        };
        let (config, workspace_changed) = reload.into_parts();
        let claude_sessions = self
            .panes
            .runtimes()
            .try_for_each(|runtime| config.claude().ensure_model_enabled(runtime.settings.model));
        if let Err(error) = claude_sessions {
            return self.refuse_reload(
                pane,
                format!("Could not reload config while a Claude session is open: {error}"),
            );
        }
        let memory_store = match crate::core::configured_memory_store(&config, &self.workspace) {
            Ok(store) => store,
            Err(error) => {
                return self.refuse_reload(
                    pane,
                    format!("Could not apply memory configuration: {error}"),
                );
            }
        };
        let theme = config.theme().clone();
        let tui = *config.tui();
        let preferred_reasoning_mode = config.agent().reasoning_mode();
        let memory_enabled = config.memory().enabled();
        self.memory.replace_store(memory_store);
        self.app.set_max_subagents(config.agent().max_subagents());
        self.app.set_claude_enabled(config.claude().enabled());
        for runtime in self.panes.runtimes() {
            apply_subagent_config(&runtime.subagent_control, &config);
        }
        self.config = config;
        let message = if workspace_changed {
            "Reloaded config · theme, UI, memory browser, and subagent limits applied · agent/auth/tool settings apply to new sessions · workspace requires restart"
        } else {
            "Reloaded config · theme, UI, memory browser, and subagent limits applied · agent/auth/tool settings apply to new sessions"
        };
        self.show(AppEvent::ConfigReloaded {
            pane,
            theme,
            tui,
            preferred_reasoning_mode,
            memory_enabled,
            message: message.to_owned(),
        });
        Ok(())
    }

    fn refuse_reload(
        &mut self,
        pane: PaneId,
        error: String,
    ) -> std::result::Result<(), CommandError> {
        self.show(AppEvent::ConfigReloadFailed {
            pane,
            error: error.clone(),
        });
        Err(CommandError::Failed(error))
    }
}

/// Applies the reloadable subagent settings to a session's subagent registry.
fn apply_subagent_config(subagents: &Subagents, config: &Config) {
    subagents.set_claude_enabled(config.claude().enabled());
    subagents.set_max_concurrency(config.agent().max_subagents());
    subagents.set_max_thinking(config.agent().thinking().into());
}

#[cfg(test)]
mod tests {
    use super::apply_subagent_config;
    use crate::app::config::{Config, ConfigOverrides, Speed};
    use nanocodex::{
        HarnessModel as Model, Model as CodexModel, NanocodexError, ReasoningMode, Thinking, Tools,
        tools::{
            contract::{ToolContext, ToolInput},
            runtime::ToolRuntime,
        },
    };
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[tokio::test]
    async fn config_reload_updates_claude_admission_in_existing_registry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "[claude]\nenabled = true\n").unwrap();
        let mut config = Config::load(ConfigOverrides {
            path: Some(path.clone()),
            auth_file: Some(directory.path().join("codex-auth")),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let (subagents, _updates) = tact_subagents::Subagents::new(1);
        subagents.set_claude_enabled(true);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        subagents
            .set_agent_factory(
                Thinking::Max,
                ReasoningMode::Standard,
                Speed::Standard,
                move |_, _| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Err(NanocodexError::InvalidRequest(
                        "test factory admitted".to_owned(),
                    ))
                },
            )
            .unwrap();
        let tools = subagents
            .downgrade()
            .install_tools(Tools::builder())
            .build()
            .unwrap();
        let runtime = ToolRuntime::new_with_tools(directory.path(), None, None, &tools);
        for (enabled, expected_calls) in [(false, 0), (true, 1), (false, 1)] {
            fs::write(&path, format!("[claude]\nenabled = {enabled}\n")).unwrap();
            config = config.reload().unwrap().into_parts().0;
            apply_subagent_config(&subagents, &config);
            let input = serde_json::json!({"role":"test","task":"test","model":"opus-5.5","thinking":"low","output_schema":{"type":"object"}});
            let output = runtime
                .execute_tool(
                    "spawn_agent",
                    ToolInput::Function(serde_json::value::to_raw_value(&input).unwrap()),
                    ToolContext::new(
                        Model::Codex(CodexModel::Sol).as_str(),
                        "root",
                        "spawn",
                        &[],
                        128,
                    ),
                )
                .await
                .unwrap();
            assert!(!output.success);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected_calls,
                "Claude enabled={enabled}"
            );
        }
    }
}
