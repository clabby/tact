//! Effort and reasoning-mode admission for children spawned beneath a parent.

use super::state::AgentReservation;
use crate::error::SpawnError;
use nanocode::{ReasoningMode, Thinking};

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

/// Whether `requested` needs Pro authority that `ceiling` does not grant.
pub(super) const fn reasoning_mode_exceeds(
    requested: ReasoningMode,
    ceiling: ReasoningMode,
) -> bool {
    matches!(
        (requested, ceiling),
        (ReasoningMode::Pro, ReasoningMode::Standard)
    )
}

impl AgentReservation {
    /// Admits a child whose effort and reasoning mode do not exceed its registered parent's.
    /// A registered parent's authority is its actual mode, so a parent resolved to standard
    /// cannot request Pro. Root callers have no effort or mode bound here; the runtime's
    /// configured caps apply at spawn time.
    pub(crate) fn validate_child(
        &self,
        thinking: Thinking,
        reasoning_mode: ReasoningMode,
    ) -> Result<(), SpawnError> {
        if let Some(parent) = self.parent_context {
            if thinking_rank(thinking)? > thinking_rank(parent.thinking)? {
                return Err(SpawnError::ThinkingExceedsParent {
                    requested: thinking,
                    parent: parent.thinking,
                });
            }
            if reasoning_mode_exceeds(reasoning_mode, parent.reasoning_mode) {
                return Err(SpawnError::ReasoningModeExceedsParent {
                    requested: reasoning_mode,
                    parent: parent.reasoning_mode,
                });
            }
        }
        Ok(())
    }
}
