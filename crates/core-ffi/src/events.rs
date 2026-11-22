//! The event bridge and the log sink (plan.md §10.1/§10.2; backlog E9-S1.3, E9-S2.1).
//!
//! Contract (docs/abi.md, decision D51):
//!
//! * **Worker threads only.** A callback never runs on the thread of an API call: an event that
//!   is produced while an API call is on the stack is delivered from a short-lived core thread.
//! * **Serialised and ordered.** One delivery loop per bridge: callbacks of one handle never run
//!   concurrently and arrive in the order the events were produced.
//! * **Coalesced** to at most one event per kind per 16 ms; state changes and questions are
//!   forced. Events held back (or produced before the handle was known) go out with the next
//!   event; the last state change of a session or job is always forced, so nothing final is held.
//! * **Detached on close.** `ds_close` detaches the bridge: no callback starts afterwards, and
//!   the call waits for one that is running (unless it is the caller's own callback). After
//!   `ds_close` returns, `user` is no longer used.
//! * A callback must not throw or panic (a panic escaping an `extern "C"` function aborts the
//!   process); a callback that takes more than 500 ms is reported to the log sink.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::types::{DsEventFn, DsHandle, DsLogFn};

const INTERVAL: Duration = Duration::from_millis(16);
const SLOW_CALLBACK: Duration = Duration::from_millis(500);

thread_local! {
    /// True while an API call (`ffi_entry!`) is on this thread's stack.
    pub static IN_API: Cell<bool> = const { Cell::new(false) };
}

// ---------------------------------------------------------------- log sink

struct LogSink {
    f: unsafe extern "C" fn(*mut c_void, u32, *const u8, usize),
    user: usize,
}

static SINK: Mutex<Option<LogSink>> = Mutex::new(None);

pub fn set_log_sink(f: DsLogFn, user: *mut c_void) {
    *SINK.lock() = f.map(|f| LogSink {
        f,
        user: user as usize,
    });
}

/// Level 0 error, 1 warning, 2 info. The sink may be called on the thread of the API call that
/// found the problem (a warning about that call's own arguments).
pub fn log(level: u32, message: &str) {
    let guard = SINK.lock();
    if let Some(s) = guard.as_ref() {
        let (f, user) = (s.f, s.user);
        drop(guard); // never call out with the lock held
        // SAFETY: `f` is the host's callback with the contract of `DsLogFn`; the message bytes
        // are valid for the call.
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            f(user as *mut c_void, level, message.as_ptr(), message.len());
        }));
    }
}

// ---------------------------------------------------------------- event bridge

type Event = (u32, u64, u64);

#[derive(Default)]
struct BridgeState {
    src: u64,
    closed: bool,
    last: [Option<Instant>; 8],
    /// The newest event of each kind that was not due (or came before the handle was known).
    held: Vec<Event>,
    queue: VecDeque<Event>,
    delivering: bool,
    /// The thread whose callback is running right now.
    in_callback: Option<ThreadId>,
}

/// Delivers events of one session or job to the host callback.
pub struct Bridge {
    cb: DsEventFn,
    user: usize,
    state: Mutex<BridgeState>,
    idle: Condvar,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bridge")
    }
}

impl Bridge {
    pub fn new(cb: DsEventFn, user: *mut c_void) -> Arc<Bridge> {
        Arc::new(Bridge {
            cb,
            user: user as usize,
            state: Mutex::new(BridgeState::default()),
            idle: Condvar::new(),
        })
    }

    /// Tells the bridge which handle its events belong to; the events held until now go out from
    /// a core thread.
    pub fn arm(self: &Arc<Self>, src: DsHandle) {
        if self.cb.is_none() {
            return;
        }
        let mut st = self.state.lock();
        st.src = src;
        let held = std::mem::take(&mut st.held);
        let now = Instant::now();
        for e in held {
            st.last[(e.0 as usize).min(7)] = Some(now);
            st.queue.push_back(e);
        }
        if !st.queue.is_empty() && !st.delivering {
            st.delivering = true;
            drop(st);
            self.drain_on_thread();
        }
    }

    /// Stops all delivery: no callback starts after this returns, and one that is running is
    /// waited for (unless it is the caller's own callback). Used by `ds_close`.
    pub fn detach(&self) {
        let mut st = self.state.lock();
        st.closed = true;
        st.held.clear();
        st.queue.clear();
        let me = std::thread::current().id();
        while st.delivering && st.in_callback != Some(me) {
            if st.in_callback.is_none() && !st.delivering {
                break;
            }
            self.idle.wait(&mut st);
        }
    }

    /// Emits an event, at most once per kind per 16 ms unless `force`.
    pub fn emit(self: &Arc<Self>, kind: u32, a: u64, b: u64, force: bool) {
        if self.cb.is_none() {
            return;
        }
        let mut st = self.state.lock();
        if st.closed {
            return;
        }
        let idx = (kind as usize).min(7);
        let due = force || st.last[idx].is_none_or(|t| t.elapsed() >= INTERVAL);
        if !due || st.src == 0 {
            st.held.retain(|e| e.0 != kind);
            st.held.push((kind, a, b));
            return;
        }
        let now = Instant::now();
        st.last[idx] = Some(now);
        // older held events of other kinds go first, so order is kept
        let held = std::mem::take(&mut st.held);
        for e in held.into_iter().filter(|e| e.0 != kind) {
            st.last[(e.0 as usize).min(7)] = Some(now);
            st.queue.push_back(e);
        }
        st.queue.push_back((kind, a, b));
        if st.delivering {
            return; // the running delivery loop takes it
        }
        st.delivering = true;
        drop(st);
        if IN_API.with(Cell::get) {
            // never inside an API call: a host that holds a lock around the call and takes it
            // in the callback must not deadlock
            self.drain_on_thread();
        } else {
            self.drain();
        }
    }

    fn drain_on_thread(self: &Arc<Self>) {
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("ds-events".into())
            .spawn(move || me.drain());
        if spawned.is_err() {
            self.drain(); // no thread to be had: deliver here rather than lose the events
        }
    }

    fn drain(&self) {
        let Some(cb) = self.cb else { return };
        loop {
            let (event, src) = {
                let mut st = self.state.lock();
                match st.queue.pop_front() {
                    Some(e) if !st.closed => {
                        st.in_callback = Some(std::thread::current().id());
                        (e, st.src)
                    }
                    _ => {
                        st.queue.clear();
                        st.delivering = false;
                        st.in_callback = None;
                        self.idle.notify_all();
                        return;
                    }
                }
            };
            let started = Instant::now();
            // SAFETY: `cb` is the host's callback with the contract of `DsEventFn`; no lock of
            // the core is held. A host callback must not panic (see the module docs).
            let r = catch_unwind(AssertUnwindSafe(|| unsafe {
                cb(self.user as *mut c_void, src, event.0, event.1, event.2);
            }));
            if r.is_err() {
                log(0, "an event callback panicked; the panic was contained");
            }
            if started.elapsed() > SLOW_CALLBACK {
                log(
                    1,
                    &format!(
                        "an event callback took {} ms; callbacks must return quickly",
                        started.elapsed().as_millis()
                    ),
                );
            }
            self.state.lock().in_callback = None;
        }
    }
}
