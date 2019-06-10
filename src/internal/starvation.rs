//! `Starvation` is a private type used for blocking other threads in order to finish some work that
//! was unable to be performed speculatively in a finite amount of time.
//!
//! `Progress` contains the logic of when to signal that a thread is starving, and waits for other
//! threads that are starving.
//!
//! Everything in this file uses `Ordering::Relaxed` meaning that this is really just a backoff
//! algorithm, and synchronization should be provided by other types.
//!
//! In the presence of a fair scheduler and bounded critical sections, these types guarantee
//! progress of all threads. This gives blocking algorithms many of the properties of wait-free
//! algorithms.
//!
//! http://raiith.iith.ac.in/3530/1/1709.01033.pdf
//!
//! Based on RawMutex in parking_lot.
//!
//! https://github.com/Amanieu/parking_lot

use crate::{
    internal::epoch::{QuiesceEpoch, EPOCH_CLOCK, TICK_SIZE},
    stats,
};
use core::{
    cell::Cell,
    ptr::NonNull,
    sync::atomic::{self, AtomicU8, Ordering::Relaxed},
};
use parking_lot_core::{self, FilterOp, ParkResult, ParkToken, UnparkResult, UnparkToken};
use std::thread;

const NO_STARVERS: usize = 0;
const SPIN_LIMIT: u32 = 6;
const YIELD_LIMIT: u32 = 10;

const LOCKED_BIT: u8 = 1 << 0;
const PARKED_BIT: u8 = 1 << 1;

/// If a thread started a transaction this many epochs ago, the thread is considered to be starving.
///
/// Lower values result in more serialization under contention. Higher values result in more wasted
/// CPU cycles for large transactions.
// TODO: Use a value based on the concurrency of the machine. Should be larger than the concurrency
// to avoid collapsing into serialization.
const MAX_ELAPSED_EPOCHS: usize = 16 * TICK_SIZE;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Token(NonNull<Progress>);

impl Token {
    #[inline]
    fn new(raw: &Progress) -> Self {
        Token(NonNull::from(raw).cast())
    }

    #[inline]
    fn park_token(self) -> ParkToken {
        ParkToken(self.0.as_ptr() as usize)
    }

    #[inline]
    fn unpark_token(self) -> UnparkToken {
        UnparkToken(self.0.as_ptr() as usize)
    }

    #[inline]
    fn from_park_token(park_token: ParkToken) -> Self {
        debug_assert!(park_token.0 != 0);
        // park tokens are only ever created with valid Progress addresses.
        Token::new(unsafe { &*(park_token.0 as *mut Progress) })
    }

    #[inline]
    unsafe fn as_ref(self) -> &'static Progress {
        &*self.0.cast().as_ptr()
    }
}

static STARVATION: Starvation = Starvation {
    state: AtomicU8::new(0),
};

/// `Starvation` only uses `Relaxed` memory ` ordering.
#[repr(align(64))]
struct Starvation {
    state: AtomicU8,
}

impl Starvation {
    #[inline]
    fn starve_lock(&self, token: Token) {
        if self
            .state
            .compare_exchange_weak(0, LOCKED_BIT, Relaxed, Relaxed)
            .is_err()
        {
            self.starve_lock_slow(token);
        }
    }

    #[inline]
    fn starve_unlock<G: FnMut(Token) -> bool, U: FnOnce(Token)>(
        &self,
        should_upgrade: G,
        upgrade: U,
    ) {
        if self
            .state
            .compare_exchange(LOCKED_BIT, 0, Relaxed, Relaxed)
            .is_ok()
        {
            stats::blocked_by_starvation(0);
            return;
        }
        self.starve_unlock_slow(should_upgrade, upgrade);
    }

    #[inline]
    fn wait_for_starvers(&self, token: Token) {
        if unlikely!(self.state.load(Relaxed) != 0) {
            self.wait_for_starvers_slow(token)
        }
    }

    #[cold]
    #[inline(never)]
    fn starve_lock_slow(&self, token: Token) {
        let mut state = self.state.load(Relaxed);
        loop {
            if state == 0 {
                match self
                    .state
                    .compare_exchange_weak(0, LOCKED_BIT, Relaxed, Relaxed)
                {
                    Ok(_) => return,
                    Err(x) => state = x,
                }
                continue;
            }

            // Set the parked bit
            if state & PARKED_BIT == 0 {
                if let Err(x) =
                    self.state
                        .compare_exchange_weak(state, state | PARKED_BIT, Relaxed, Relaxed)
                {
                    state = x;
                    continue;
                }
            }

            // Park our thread until we are woken up by an unlock
            let addr = self as *const _ as usize;
            let validate = || self.state.load(Relaxed) & PARKED_BIT != 0;
            let before_sleep = || {};
            let timed_out = |_, _| {};
            let park_token = token.park_token();
            match unsafe {
                parking_lot_core::park(addr, validate, before_sleep, timed_out, park_token, None)
            } {
                ParkResult::Unparked(wakeup_token) => {
                    debug_assert_eq!(
                        wakeup_token,
                        token.unpark_token(),
                        "unfairly unparking a starving thread"
                    );
                    debug_assert!(
                        self.state.load(Relaxed) & LOCKED_BIT != 0,
                        "improperly set the state before handing off starvation control"
                    );
                    return;
                }
                ParkResult::Invalid => {}
                ParkResult::TimedOut => debug_assert!(false),
            }
            state = self.state.load(Relaxed);
        }
    }

    #[cold]
    #[inline(never)]
    fn wait_for_starvers_slow(&self, token: Token) {
        let mut state = self.state.load(Relaxed);
        loop {
            if state == 0 {
                return;
            }

            // Set the parked bit
            if state & PARKED_BIT == 0 {
                if let Err(x) =
                    self.state
                        .compare_exchange_weak(state, state | PARKED_BIT, Relaxed, Relaxed)
                {
                    state = x;
                    continue;
                }
            }

            // Park our thread until we are woken up by an unlock
            let addr = self as *const _ as usize;
            let validate = || self.state.load(Relaxed) & PARKED_BIT != 0;
            let before_sleep = || {};
            let timed_out = |_, _| {};
            match unsafe {
                parking_lot_core::park(
                    addr,
                    validate,
                    before_sleep,
                    timed_out,
                    token.park_token(),
                    None,
                )
            } {
                ParkResult::Unparked(UnparkToken(NO_STARVERS)) => {
                    return;
                }
                ParkResult::Unparked(wakeup_token) => {
                    if wakeup_token == token.unpark_token() {
                        // this thread has been upgraded to a starver
                        debug_assert!(
                            self.state.load(Relaxed) & LOCKED_BIT != 0,
                            "improperly set the state before handing off starvation control"
                        );
                        return;
                    }
                    // unparked before it was known there was another starving thread.
                }
                ParkResult::Invalid => {}
                ParkResult::TimedOut => debug_assert!(false),
            }
            state = self.state.load(Relaxed);
        }
    }

    #[cold]
    #[inline(never)]
    fn starve_unlock_slow<G: FnMut(Token) -> bool, U: FnOnce(Token)>(
        &self,
        mut should_upgrade: G,
        upgrade: U,
    ) {
        let addr = self as *const _ as usize;
        let next_starved_token = Cell::new(None);
        let next_starved_token = &next_starved_token;

        // We don't know what thread we wish to unpark until we finish filtering. This means that
        // threads will sometimes be unparked without the possibility of making progress.
        let filter = move |token: ParkToken| {
            debug_assert!(token.0 != NO_STARVERS, "invalid ParkToken detected");
            let next_starved = next_starved_token.get();
            if let None = next_starved {
                let token = Token::from_park_token(token);
                if should_upgrade(token) {
                    next_starved_token.set(Some(token));
                }
                FilterOp::Unpark
            } else {
                // At this point, it's known we're handing off control to another starving thread.
                FilterOp::Stop
            }
        };
        let callback = |unpark_result: UnparkResult| {
            debug_assert!(self.state.load(Relaxed) & LOCKED_BIT != 0);
            debug_assert!(next_starved_token.get().is_none() || unpark_result.unparked_threads > 0);
            debug_assert!(
                unpark_result.unparked_threads == 0 || self.state.load(Relaxed) & PARKED_BIT != 0
            );
            debug_assert!(!unpark_result.have_more_threads || next_starved_token.get().is_some());

            let next_starved = next_starved_token.get();
            let next_state = if unpark_result.have_more_threads {
                LOCKED_BIT | PARKED_BIT
            } else if next_starved.is_some() {
                LOCKED_BIT
            } else {
                0
            };

            self.state.store(next_state, Relaxed);

            match next_starved {
                Some(next_starved) => {
                    upgrade(next_starved);
                    next_starved.unpark_token()
                }
                None => UnparkToken(0),
            }
        };

        let result = unsafe { parking_lot_core::unpark_filter(addr, filter, callback) };
        if next_starved_token.get().is_some() {
            stats::starvation_handoff();
        }
        stats::blocked_by_starvation(result.unparked_threads)
    }
}

#[derive(Debug, Copy, Clone)]
enum ProgressImpl {
    NotStarving {
        first_failed_epoch: Option<QuiesceEpoch>,
        backoff:            u32,
    },
    Starving,
}

impl ProgressImpl {
    #[inline]
    fn new() -> Self {
        ProgressImpl::NotStarving {
            first_failed_epoch: None,
            backoff:            0,
        }
    }

    #[inline]
    fn should_starve(&self) -> bool {
        match self {
            ProgressImpl::NotStarving {
                first_failed_epoch: Some(epoch),
                backoff,
            } => {
                if *backoff >= YIELD_LIMIT {
                    return true;
                }
                let now = EPOCH_CLOCK.now().unwrap_or_else(|| abort!());
                now.get().get() - epoch.get().get() >= MAX_ELAPSED_EPOCHS
            }
            ProgressImpl::NotStarving {
                first_failed_epoch: None,
                ..
            } => false,
            ProgressImpl::Starving => {
                debug_assert!(false);
                false
            }
        }
    }
}

pub struct Progress {
    /// The `Cell` here is actually accessed from multiple threads, but only while the "owning"
    /// thread is parked, and parking lots bucket locks are held.
    inner: Cell<ProgressImpl>,
}

#[cfg(debug_assertions)]
impl Drop for Progress {
    fn drop(&mut self) {
        match self.inner.get() {
            ProgressImpl::NotStarving {
                first_failed_epoch: None,
                backoff,
            } if backoff == 1 => {}
            inner => panic!(
                "`Progress` dropped without having made progress: {:?}",
                inner
            ),
        }
    }
}

impl Progress {
    #[inline]
    pub fn new() -> Self {
        Progress {
            inner: Cell::new(ProgressImpl::new()),
        }
    }

    /// Called when a thread has failed either the optimistic phase of concurrency, or the
    /// pessimistic phase of concurrency.
    #[cold]
    pub fn failed_to_progress(&self, epoch: QuiesceEpoch) {
        match self.inner.get() {
            ProgressImpl::NotStarving {
                first_failed_epoch,
                backoff,
            } => {
                if backoff <= SPIN_LIMIT {
                    let first_failed_epoch = first_failed_epoch.unwrap_or(epoch);
                    if epoch.get().get() - first_failed_epoch.get().get() >= MAX_ELAPSED_EPOCHS {
                        // long transaction detected, `spin_loop_hint` is probably a bad backoff
                        // strategy.
                        self.inner.set(ProgressImpl::NotStarving {
                            first_failed_epoch: Some(epoch),
                            backoff:            SPIN_LIMIT + 1,
                        });
                        thread::yield_now();
                        return;
                    } else {
                        for _ in 0..1 << backoff {
                            atomic::spin_loop_hint();
                        }
                        self.inner.set(ProgressImpl::NotStarving {
                            first_failed_epoch: Some(epoch),
                            backoff:            backoff + 1,
                        });
                    }
                } else if backoff <= YIELD_LIMIT {
                    thread::yield_now();

                    self.inner.set(ProgressImpl::NotStarving {
                        first_failed_epoch: Some(epoch),
                        backoff:            backoff + 1,
                    });
                } else {
                    thread::yield_now();
                    STARVATION.starve_lock(Token::new(self));
                    self.inner.set(ProgressImpl::Starving)
                }
            }
            ProgressImpl::Starving => thread::yield_now(),
        };
    }

    /// Called when a thread has finished the optimistic phase of concurrency, and is about to enter
    /// a pessimistic phase where the threads progress will be published.
    #[inline]
    pub fn wait_for_starvers(&self) {
        match self.inner.get() {
            ProgressImpl::NotStarving { .. } => STARVATION.wait_for_starvers(Token::new(self)),
            ProgressImpl::Starving => {}
        };
    }

    /// Called after progress has been made.
    #[inline]
    pub fn progressed(&self) {
        match self.inner.get() {
            ProgressImpl::NotStarving {
                first_failed_epoch: None,
                ..
            } => return,
            _ => {}
        }
        self.progressed_slow()
    }

    #[inline(never)]
    #[cold]
    fn progressed_slow(&self) {
        match self.inner.get() {
            ProgressImpl::NotStarving { .. } => {}
            ProgressImpl::Starving => {
                STARVATION.starve_unlock(
                    |this| unsafe { this.as_ref() }.inner.get().should_starve(),
                    |this| unsafe { this.as_ref() }.inner.set(ProgressImpl::Starving),
                );
            }
        };
        self.inner.set(ProgressImpl::new());
    }
}
