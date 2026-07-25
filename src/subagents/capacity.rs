use std::sync::{Arc, Mutex};

pub(super) struct Capacity {
    state: Arc<Mutex<CapacityState>>,
}

struct CapacityState {
    active: usize,
    limit: usize,
}

pub(super) struct TurnCapacity {
    state: Arc<Mutex<CapacityState>>,
}

impl Capacity {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(CapacityState { active: 0, limit })),
        }
    }

    pub(super) fn reserve(&self) -> std::io::Result<TurnCapacity> {
        let mut state = self
            .state
            .lock()
            .expect("subagent capacity lock should not be poisoned");
        if state.active >= state.limit {
            return Err(std::io::Error::other(format!(
                "sub-agent concurrency limit of {} has been reached; try delegation again later",
                state.limit
            )));
        }
        state.active += 1;
        drop(state);

        Ok(TurnCapacity {
            state: Arc::clone(&self.state),
        })
    }

    pub(super) fn set_limit(&self, limit: usize) {
        self.state
            .lock()
            .expect("subagent capacity lock should not be poisoned")
            .limit = limit;
    }
}

impl Drop for TurnCapacity {
    fn drop(&mut self) {
        self.state
            .lock()
            .expect("subagent capacity lock should not be poisoned")
            .active -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::Capacity;

    #[test]
    fn capacity_is_released_for_later_delegation() {
        let capacity = Capacity::new(1);
        let reservation = capacity.reserve().unwrap();

        let error = capacity.reserve().err().unwrap();
        assert_eq!(
            error.to_string(),
            "sub-agent concurrency limit of 1 has been reached; try delegation again later"
        );

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
