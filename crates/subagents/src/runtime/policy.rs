//! Model and effort admission for children spawned beneath a parent.

use super::state::AgentReservation;
use crate::error::SpawnError;
use nanocodex::{HarnessModel as Model, Model as CodexModel, Thinking};

/// Orders reasoning efforts that a child may run at. Children must reason, so `none` is rejected.
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

/// Orders Codex models by capability. Claude models are unranked and may delegate to, or be
/// chosen by, any model.
const fn codex_tier(model: Model) -> Option<u8> {
    match model {
        Model::Codex(CodexModel::Luna) => Some(1),
        Model::Codex(CodexModel::Sol) => Some(2),
        Model::Codex(CodexModel::Astra) => Some(3),
        _ => None,
    }
}

impl AgentReservation {
    /// Admits a child whose effort does not exceed its registered parent's and whose Codex tier
    /// does not exceed its parent's. A root caller is identified by the model of its current
    /// turn and has no effort bound here; the runtime's configured cap applies at spawn time.
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
        if let (Some(parent_tier), Some(child_tier)) = (codex_tier(parent_model), codex_tier(model))
            && child_tier > parent_tier
        {
            return Err(SpawnError::ModelExceedsParent {
                requested: model,
                parent: parent_model,
            });
        }
        Ok(())
    }
}
