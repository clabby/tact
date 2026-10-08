use super::error::SubagentError;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone)]
pub(super) struct Capacity {
    state: Arc<Mutex<CapacityState>>,
    revision: watch::Sender<u64>,
}

struct CapacityState {
    active: usize,
    limit: usize,
}

pub(super) struct TurnCapacity {
    state: Arc<Mutex<CapacityState>>,
    revision: watch::Sender<u64>,
}

impl Capacity {
    pub(super) fn new(limit: usize) -> Self {
        let (revision, _) = watch::channel(0);
        Self {
            state: Arc::new(Mutex::new(CapacityState { active: 0, limit })),
            revision,
        }
    }

    pub(super) fn reserve(&self) -> Result<TurnCapacity, SubagentError> {
        let mut state = self
            .state
            .lock()
            .expect("subagent capacity lock should not be poisoned");
        if state.active >= state.limit {
            return Err(SubagentError::CapacityExhausted { limit: state.limit });
        }
        state.active += 1;
        drop(state);

        Ok(TurnCapacity {
            state: Arc::clone(&self.state),
            revision: self.revision.clone(),
        })
    }

    pub(super) fn set_limit(&self, limit: usize) {
        self.state
            .lock()
            .expect("subagent capacity lock should not be poisoned")
            .limit = limit;
        self.changed();
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    fn changed(&self) {
        self.revision.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

impl Drop for TurnCapacity {
    fn drop(&mut self) {
        self.state
            .lock()
            .expect("subagent capacity lock should not be poisoned")
            .active -= 1;
        self.revision.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::Capacity;
    use crate::error::SubagentError;

    #[test]
    fn capacity_is_released_for_later_delegation() {
        let capacity = Capacity::new(1);
        let reservation = capacity.reserve().unwrap();

        assert!(matches!(
            capacity.reserve(),
            Err(SubagentError::CapacityExhausted { limit: 1 })
        ));

        drop(reservation);
        assert!(capacity.reserve().is_ok());
    }

    #[test]
    fn limit_can_change_while_turns_are_active() {
        let capacity = Capacity::new(1);
        let _first = capacity.reserve().unwrap();

        capacity.set_limit(2);
        let _second = capacity.reserve().unwrap();
        capacity.set_limit(1);

        assert!(capacity.reserve().is_err());
    }
}
