//! Accounting for the turns and shell commands a pane has in flight.

use crate::core::protocol::Busy;

/// One of the two independent signals that end a turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TurnEnd {
    /// The worker's task returned. `terminal_expected` states whether the agent stream will also
    /// deliver a terminal record for this turn.
    Worker { terminal_expected: bool },
    /// The agent stream delivered the turn's `run.completed` or `run.failed` record.
    Terminal,
}

/// Counts the work a pane has started and not yet seen finish.
///
/// A turn that ends with a terminal record is reported twice, by two independent sources: the
/// worker, once its task returns, and the agent stream, once it delivers the `run.completed` or
/// `run.failed` record. The two signals race, so either may arrive first. The ledger pairs them
/// and treats the turn as finished only when both have arrived, so the next queued prompt cannot
/// start while the transcript is still receiving the previous turn. A worker turn that ends
/// without a terminal record has no partner signal and finishes immediately.
///
/// Every started turn finishes exactly once; counts never underflow even when a stray signal
/// arrives for a turn this ledger did not start.
#[derive(Debug, Default)]
pub(super) struct TurnLedger {
    turns: usize,
    shells: usize,
    /// Worker completions still waiting for their agent-stream partner.
    unmatched_worker: usize,
    /// Agent-stream completions still waiting for their worker partner.
    unmatched_agent: usize,
}

impl TurnLedger {
    pub(super) const fn start_turn(&mut self) {
        self.turns = self.turns.saturating_add(1);
    }

    pub(super) const fn start_shell(&mut self) {
        self.shells = self.shells.saturating_add(1);
    }

    pub(super) const fn finish_shell(&mut self) {
        self.shells = self.shells.saturating_sub(1);
    }

    /// Records one signal for the end of a turn and returns whether the turn has now finished.
    pub(super) const fn record_end(&mut self, end: TurnEnd) -> bool {
        match end {
            TurnEnd::Worker {
                terminal_expected: false,
            } => {}
            TurnEnd::Worker {
                terminal_expected: true,
            } => {
                if self.unmatched_agent == 0 {
                    self.unmatched_worker = self.unmatched_worker.saturating_add(1);
                    return false;
                }
                self.unmatched_agent -= 1;
            }
            TurnEnd::Terminal => {
                if self.unmatched_worker == 0 {
                    self.unmatched_agent = self.unmatched_agent.saturating_add(1);
                    return false;
                }
                self.unmatched_worker -= 1;
            }
        }
        self.finish_turn();
        true
    }

    const fn finish_turn(&mut self) {
        self.turns = self.turns.saturating_sub(1);
    }

    pub(super) const fn turn_running(&self) -> bool {
        self.turns > 0
    }

    /// Whether neither a turn nor a shell command is running.
    pub(super) const fn is_idle(&self) -> bool {
        self.turns == 0 && self.shells == 0
    }

    pub(super) const fn busy(&self) -> Busy {
        Busy {
            turns: self.turns,
            shells: self.shells,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TurnEnd, TurnLedger};

    const WORKER: TurnEnd = TurnEnd::Worker {
        terminal_expected: true,
    };

    fn running(turns: usize) -> TurnLedger {
        let mut ledger = TurnLedger::default();
        for _ in 0..turns {
            ledger.start_turn();
        }
        ledger
    }

    #[test]
    fn a_terminal_turn_finishes_once_both_signals_arrive_in_either_order() {
        let mut worker_first = running(1);
        assert!(!worker_first.record_end(WORKER));
        assert!(worker_first.turn_running());
        assert!(worker_first.record_end(TurnEnd::Terminal));
        assert!(worker_first.is_idle());

        let mut agent_first = running(1);
        assert!(!agent_first.record_end(TurnEnd::Terminal));
        assert!(agent_first.turn_running());
        assert!(agent_first.record_end(WORKER));
        assert!(agent_first.is_idle());
    }

    #[test]
    fn a_turn_without_a_terminal_record_finishes_on_the_worker_signal() {
        let mut ledger = running(1);

        assert!(ledger.record_end(TurnEnd::Worker {
            terminal_expected: false,
        }));
        assert!(ledger.is_idle());
    }

    #[test]
    fn signals_pair_across_consecutive_turns_without_finishing_a_turn_twice() {
        let mut ledger = running(2);

        assert!(!ledger.record_end(TurnEnd::Terminal));
        assert!(!ledger.record_end(TurnEnd::Terminal));
        assert!(ledger.record_end(WORKER));
        assert_eq!(ledger.busy().turns, 1);
        assert!(ledger.record_end(WORKER));
        assert_eq!(ledger.busy().turns, 0);
        assert!(!ledger.record_end(WORKER));
        assert_eq!(ledger.busy().turns, 0);
    }

    #[test]
    fn shells_count_separately_from_turns() {
        let mut ledger = TurnLedger::default();
        ledger.start_shell();

        assert!(!ledger.turn_running());
        assert!(!ledger.is_idle());
        ledger.finish_shell();
        ledger.finish_shell();
        assert!(ledger.is_idle());
        assert_eq!(ledger.busy().shells, 0);
    }
}
