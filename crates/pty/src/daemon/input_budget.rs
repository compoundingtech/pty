//! The bound on client input the child has not taken yet.
//!
//! A client that keeps writing to a child that never reads would otherwise
//! grow the daemon until the child read or exited (compoundingtech/pty#170).

use std::sync::{Arc, Condvar, Mutex};

/// How much client input the daemon holds for the child at once: accepted
/// from a socket, not yet taken by the kernel.
pub(crate) const INPUT_BUDGET: usize = 1 << 20;

/// Input accepted from clients but not yet written to the child, shared by
/// every client socket of one daemon.
///
/// A client's reader thread holds a DATA packet's bytes before it hands the
/// packet on, and waits while the budget is spent. It then stops reading
/// that socket, so the sender's own writes block. Nothing is dropped, every
/// other client is still served, and the actor never waits here. A packet
/// larger than the whole budget passes when nothing else is pending, or it
/// could never be written.
pub(crate) struct InputBudget {
    limit: usize,
    pending: Mutex<usize>,
    released: Condvar,
}

/// Bytes held against an [`InputBudget`] until this is dropped: once the
/// kernel has taken them, or wherever they are discarded instead (a
/// read-only client, an exited child, a daemon on its way out).
pub(crate) struct InputHold {
    budget: Arc<InputBudget>,
    bytes: usize,
}

impl InputBudget {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            pending: Mutex::new(0),
            released: Condvar::new(),
        })
    }

    /// Wait until `bytes` fit, then hold them.
    pub(crate) fn hold(self: &Arc<Self>, bytes: usize) -> InputHold {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        while *pending > 0 && *pending + bytes > self.limit {
            pending = self
                .released
                .wait(pending)
                .unwrap_or_else(|e| e.into_inner());
        }
        *pending += bytes;
        InputHold {
            budget: Arc::clone(self),
            bytes,
        }
    }
}

impl Drop for InputHold {
    fn drop(&mut self) {
        let mut pending = self
            .budget
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *pending -= self.bytes;
        drop(pending);
        self.budget.released.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Hold `bytes` on another thread; the receiver gets the hold once it is granted.
    fn hold_elsewhere(budget: &Arc<InputBudget>, bytes: usize) -> mpsc::Receiver<InputHold> {
        let (tx, rx) = mpsc::channel();
        let budget = Arc::clone(budget);
        std::thread::spawn(move || {
            let _ = tx.send(budget.hold(bytes));
        });
        rx
    }

    #[test]
    fn holds_within_the_budget_do_not_wait() {
        let budget = InputBudget::new(10);
        let _first = budget.hold(4);
        let _second = budget.hold(6);
    }

    #[test]
    fn a_hold_past_the_budget_waits_for_a_release() {
        let budget = InputBudget::new(10);
        let first = budget.hold(6);
        let second = hold_elsewhere(&budget, 6);
        assert!(
            second.recv_timeout(Duration::from_millis(200)).is_err(),
            "12 bytes were held against a budget of 10"
        );
        drop(first);
        assert!(
            second.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the release did not let the waiting hold through"
        );
    }

    #[test]
    fn a_packet_larger_than_the_budget_passes_alone() {
        let budget = InputBudget::new(10);
        let large = budget.hold(25);
        let next = hold_elsewhere(&budget, 1);
        assert!(
            next.recv_timeout(Duration::from_millis(200)).is_err(),
            "a hold went through while an oversize one was pending"
        );
        drop(large);
        let next = next.recv_timeout(Duration::from_secs(5)).expect("granted");
        drop(next);
        let _after = budget.hold(10);
    }
}
