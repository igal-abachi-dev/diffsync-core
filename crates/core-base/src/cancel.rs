//! Cooperative cancellation and pausing (plan.md §3).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::error::{Error, Result};

/// A cancel flag shared between a caller and its workers. Cloning shares the flag.
///
/// Reads use `Relaxed` ordering: cancellation is a hint that workers poll, and nothing else
/// is published through the flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    parent: Option<Box<CancelToken>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// A token that is cancelled when this one is, or when it is cancelled itself. Cancelling
    /// the child leaves the parent alone (used to stop early without cancelling the caller).
    #[must_use]
    pub fn child(&self) -> CancelToken {
        CancelToken {
            flag: Arc::default(),
            parent: Some(Box::new(self.clone())),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed) || self.parent.as_ref().is_some_and(|p| p.is_cancelled())
    }

    /// `Err(Cancelled)` once the token has been cancelled. Use with `?` in loops.
    #[inline]
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::cancelled())
        } else {
            Ok(())
        }
    }

    /// Clears the flag so the token can be reused for a restart (`recompare`).
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

/// Pause/resume switch for sync jobs. Workers call [`PauseGate::wait`] at safe points.
#[derive(Debug, Clone, Default)]
pub struct PauseGate(Arc<(Mutex<bool>, Condvar)>);

impl PauseGate {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        // A poisoned lock only means another thread panicked while holding a plain bool.
        self.0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn pause(&self) {
        *self.lock() = true;
    }

    pub fn resume(&self) {
        *self.lock() = false;
        self.0.1.notify_all();
    }

    pub fn is_paused(&self) -> bool {
        *self.lock()
    }

    /// Blocks while the gate is paused.
    pub fn wait(&self) {
        let mut paused = self.lock();
        while *paused {
            paused = self
                .0
                .1
                .wait(paused)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Like [`wait`](Self::wait), but returns `Err(Cancelled)` when `cancel` fires while the
    /// gate is paused. A cancel does not notify the condvar, so this polls it every 10 ms.
    pub fn wait_or_cancel(&self, cancel: &CancelToken) -> Result<()> {
        let mut paused = self.lock();
        while *paused {
            cancel.check()?;
            let (g, _timeout) = self
                .0
                .1
                .wait_timeout(paused, Duration::from_millis(10))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            paused = g;
        }
        cancel.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn token_starts_clear_and_cancels_all_clones() {
        let t = CancelToken::new();
        let c = t.clone();
        assert!(!t.is_cancelled());
        assert!(t.check().is_ok());
        c.cancel();
        assert!(t.is_cancelled());
        let err = t.check().unwrap_err();
        assert!(err.is_cancelled());
        t.reset();
        assert!(!c.is_cancelled());
    }

    #[test]
    fn child_follows_the_parent_but_not_the_other_way() {
        let parent = CancelToken::new();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(
            !parent.is_cancelled(),
            "cancelling a child leaves the parent alone"
        );
        let child2 = parent.child();
        assert!(!child2.is_cancelled());
        parent.cancel();
        assert!(child2.is_cancelled());
        assert!(child2.child().check().is_err(), "grandchildren see it too");
    }

    #[test]
    fn cancel_from_another_thread_is_seen() {
        let t = CancelToken::new();
        let worker = {
            let t = t.clone();
            thread::spawn(move || {
                let start = Instant::now();
                while t.check().is_ok() {
                    assert!(
                        start.elapsed() < Duration::from_secs(10),
                        "cancel never arrived"
                    );
                    thread::yield_now();
                }
            })
        };
        thread::sleep(Duration::from_millis(20));
        t.cancel();
        worker.join().unwrap();
    }

    #[test]
    fn gate_blocks_until_resumed() {
        let gate = PauseGate::new();
        gate.pause();
        assert!(gate.is_paused());
        let progressed = Arc::new(AtomicUsize::new(0));
        let h = {
            let (gate, progressed) = (gate.clone(), progressed.clone());
            thread::spawn(move || {
                gate.wait();
                progressed.store(1, Ordering::SeqCst);
            })
        };
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            progressed.load(Ordering::SeqCst),
            0,
            "worker ran while paused"
        );
        gate.resume();
        h.join().unwrap();
        assert_eq!(progressed.load(Ordering::SeqCst), 1);
        assert!(!gate.is_paused());
    }

    #[test]
    fn gate_that_is_not_paused_does_not_block() {
        let gate = PauseGate::new();
        gate.wait();
        gate.pause();
        gate.resume();
        gate.wait();
    }

    #[test]
    fn many_threads_pause_resume_cycles() {
        let gate = PauseGate::new();
        let done = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let (gate, done) = (gate.clone(), done.clone());
                thread::spawn(move || {
                    for _ in 0..200 {
                        gate.wait();
                    }
                    done.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        for _ in 0..20 {
            gate.pause();
            thread::sleep(Duration::from_millis(1));
            gate.resume();
        }
        gate.resume();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(done.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn cancel_wakes_a_paused_waiter() {
        let gate = PauseGate::new();
        gate.pause();
        let cancel = CancelToken::new();
        let h = {
            let (gate, cancel) = (gate.clone(), cancel.clone());
            thread::spawn(move || gate.wait_or_cancel(&cancel))
        };
        thread::sleep(Duration::from_millis(30));
        cancel.cancel();
        let r = h.join().unwrap();
        assert!(r.unwrap_err().is_cancelled());
        assert!(gate.is_paused(), "cancel must not resume the gate");
    }
}
