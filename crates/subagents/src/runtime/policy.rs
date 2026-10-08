//! Model and effort admission for children spawned beneath a parent.

use super::state::AgentReservation;
use crate::error::SpawnError;
use nanocodex::{HarnessModel as Model, Model as CodexModel, Thinking};

pub(super) fn thinking_rank(thinking: Thinking) -> Result<u8, SpawnError> {
    Ok(match thinking {
        Thinking::None => return Err(SpawnError::ThinkingDisabled),
        Thinking::Low => 1,
        Thinking::Medium => 2,
        Thinking::High => 3,
        Thinking::Xhigh => 4,
        Thinking::Max => 5,
    })
}

impl AgentReservation {
    pub(crate) fn validate_child(
        &self,
        caller_model: &str,
        model: Model,
        thinking: Thinking,
    ) -> Result<(), SpawnError> {
        let parent_model = match self.parent_context {
            Some(parent) => {
                if thinking_rank(thinking)? > thinking_rank(parent.thinking)? {
                    return Err(SpawnError::ThinkingExceedsParent {
                        requested: thinking,
                        parent: parent.thinking,
                    });
                }
                parent.model
            }
            None => crate::parse_model(caller_model)?,
        };
        if matches!(
            (parent_model, model),
            (
                Model::Codex(CodexModel::Luna),
                Model::Codex(CodexModel::Sol | CodexModel::Astra)
            ) | (
                Model::Codex(CodexModel::Sol),
                Model::Codex(CodexModel::Astra)
            )
        ) {
            return Err(SpawnError::ModelExceedsParent {
                requested: model,
                parent: parent_model,
            });
        }
        Ok(())
    }
}
