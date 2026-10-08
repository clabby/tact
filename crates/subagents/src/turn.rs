//! Turn tokens and result submission for one child session.
//!
//! Each turn and each urgent steer draws a fresh token from a per-child counter. `submit_result`
//! must quote the token of the running turn, so a result produced for an earlier turn, or for a
//! prompt that an urgent message has just replaced, cannot complete the current one. A running
//! turn accepts at most one schema-valid result.

use super::error::SubagentError;
use jsonschema::Validator;
use serde_json::Value;

/// Number of schema violations reported back to the model for one rejected submission.
const REPORTED_VIOLATIONS: usize = 4;

#[derive(Default)]
pub(super) struct TurnSlot {
    issued: u64,
    turn: Turn,
}

#[derive(Default)]
enum Turn {
    #[default]
    Idle,
    Running {
        token: u64,
        submitted: Option<Value>,
    },
    /// An urgent message is replacing the running prompt under `token`. If steering fails, the
    /// turn continues under `previous`.
    Steering { token: u64, previous: u64 },
}

impl TurnSlot {
    pub(super) const fn is_active(&self) -> bool {
        !matches!(self.turn, Turn::Idle)
    }

    /// Opens a turn and returns its token, or `None` if a turn is already running.
    pub(super) fn start(&mut self) -> Option<u64> {
        if self.is_active() {
            return None;
        }
        let token = self.issue()?;
        self.turn = Turn::Running {
            token,
            submitted: None,
        };
        Some(token)
    }

    pub(super) fn submit(
        &mut self,
        token: u64,
        output: Value,
        validator: &Validator,
    ) -> Result<(), SubagentError> {
        let submitted = match &mut self.turn {
            Turn::Idle => return Err(SubagentError::NoActiveTurn),
            Turn::Steering { .. } => return Err(SubagentError::TurnSteering),
            Turn::Running { token: current, .. } if *current != token => {
                return Err(SubagentError::StaleTurnToken);
            }
            Turn::Running {
                submitted: Some(_), ..
            } => return Err(SubagentError::AlreadySubmitted),
            Turn::Running { submitted, .. } => submitted,
        };
        let violations = validator
            .iter_errors(&output)
            .take(REPORTED_VIOLATIONS)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        if !violations.is_empty() {
            return Err(SubagentError::OutputMismatch { violations });
        }
        *submitted = Some(output);
        Ok(())
    }

    /// Issues a replacement token for a running turn that has not yet produced a result.
    pub(super) fn begin_steer(&mut self) -> Option<u64> {
        let Turn::Running {
            token: previous,
            submitted: None,
        } = self.turn
        else {
            return None;
        };
        let token = self.issue()?;
        self.turn = Turn::Steering { token, previous };
        Some(token)
    }

    /// Resolves a steer. A turn that ended or was replaced in the meantime is left untouched.
    pub(super) fn finish_steer(&mut self, steer: u64, committed: bool) {
        let Turn::Steering { token, previous } = self.turn else {
            return;
        };
        if token != steer {
            return;
        }
        self.turn = Turn::Running {
            token: if committed { token } else { previous },
            submitted: None,
        };
    }

    /// Ends the turn and returns its accepted result, or `None` if no turn was running.
    pub(super) fn finish(&mut self) -> Option<Option<Value>> {
        match std::mem::take(&mut self.turn) {
            Turn::Idle => None,
            Turn::Running { submitted, .. } => Some(submitted),
            Turn::Steering { .. } => Some(None),
        }
    }

    fn issue(&mut self) -> Option<u64> {
        self.issued = self.issued.checked_add(1)?;
        Some(self.issued)
    }
}

#[cfg(test)]
mod tests {
    use super::TurnSlot;
    use crate::error::SubagentError;
    use serde_json::json;

    fn any_value() -> jsonschema::Validator {
        jsonschema::validator_for(&json!({})).unwrap()
    }

    #[test]
    fn tokens_from_earlier_turns_are_stale() {
        let validator = any_value();
        let mut slot = TurnSlot::default();
        let first = slot.start().unwrap();
        assert_eq!(slot.finish(), Some(None));
        let second = slot.start().unwrap();

        assert!(matches!(
            slot.submit(first, json!(1), &validator),
            Err(SubagentError::StaleTurnToken)
        ));
        slot.submit(second, json!(2), &validator).unwrap();
        assert!(matches!(
            slot.submit(second, json!(3), &validator),
            Err(SubagentError::AlreadySubmitted)
        ));
        assert_eq!(slot.finish(), Some(Some(json!(2))));
        assert_eq!(slot.finish(), None);
    }

    #[test]
    fn steering_rotates_the_token_or_restores_it_on_failure() {
        let validator = any_value();
        let mut slot = TurnSlot::default();
        let original = slot.start().unwrap();

        let failed = slot.begin_steer().unwrap();
        assert!(matches!(
            slot.submit(original, json!(1), &validator),
            Err(SubagentError::TurnSteering)
        ));
        slot.finish_steer(failed, false);
        let committed = slot.begin_steer().unwrap();
        slot.finish_steer(committed, true);

        assert!(matches!(
            slot.submit(original, json!(1), &validator),
            Err(SubagentError::StaleTurnToken)
        ));
        slot.submit(committed, json!(2), &validator).unwrap();
        assert!(slot.begin_steer().is_none());
    }

    #[test]
    fn submissions_require_a_running_turn_and_a_schema_valid_value() {
        let validator = jsonschema::validator_for(&json!({ "type": "integer" })).unwrap();
        let mut slot = TurnSlot::default();
        assert!(matches!(
            slot.submit(1, json!(1), &validator),
            Err(SubagentError::NoActiveTurn)
        ));

        let token = slot.start().unwrap();
        assert!(matches!(
            slot.submit(token, json!("one"), &validator),
            Err(SubagentError::OutputMismatch { violations }) if violations.len() == 1
        ));
        slot.submit(token, json!(1), &validator).unwrap();
    }
}
