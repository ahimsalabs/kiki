//! Fault-injection infrastructure (BUGGIFY).
//!
//! Provides a `buggify!` macro for conditional fault injection at critical
//! code paths. Gated by `cfg(feature = "fault-injection")`:
//!
//! - **Feature disabled (production):** macros expand to `false` / empty.
//!   Zero overhead — the optimizer eliminates all injection sites.
//! - **Feature enabled (test):** a global atomic switch + per-thread
//!   xorshift64 RNG allows deterministic, reproducible fault injection.
//!
//! # Usage
//!
//! ```rust,ignore
//! use crate::fi::buggify;
//!
//! // Returns true with probability 1/10 when fault injection is active.
//! if buggify!(10) {
//!     return Err(anyhow!("[BUGGIFY] simulated store write failure"));
//! }
//!
//! // Execute a block with probability 1/5.
//! buggify!(5, {
//!     tokio::time::sleep(Duration::from_millis(200)).await;
//! });
//! ```
//!
//! # Reproducibility
//!
//! The test harness sets a seed via [`set_seed`]. Each thread's RNG is
//! derived from `global_seed XOR thread_id`, giving distinct but
//! reproducible streams. On failure, print the seed for exact replay.
//!
//! # Design rationale
//!
//! - `cfg(feature)` not `cfg(test)`: integration tests spawn the daemon
//!   as a subprocess where `cfg(test)` is NOT set. The feature flag ensures
//!   fault injection runs in the actual binary the integration tests invoke.
//! - Thread-local xorshift64: no lock, no allocation, single `Cell<u64>`
//!   read per injection site. Deterministic given a fixed seed.
//! - `1-in-N` integer probability: avoids float math and `rand` calls on
//!   the hot path.

// ── Feature DISABLED: no-op stubs ──────────────────────────────────────────

#[cfg(not(feature = "fault-injection"))]
macro_rules! buggify {
    ($n:expr) => {
        false
    };
    ($n:expr, $block:block) => {};
}

#[cfg(not(feature = "fault-injection"))]
pub(crate) use buggify;

// ── Feature ENABLED: real implementation ───────────────────────────────────

#[cfg(feature = "fault-injection")]
pub(crate) mod inner {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// Global kill-switch. Must be `true` for any injection to fire.
    static ENABLED: AtomicBool = AtomicBool::new(false);

    /// Global seed. Each thread's local RNG is initialized from this XOR'd
    /// with a per-thread counter to ensure distinct reproducible streams.
    static SEED: AtomicU64 = AtomicU64::new(0);

    /// Monotonic counter for per-thread uniqueness (avoids the unsafe
    /// ThreadId transmute while remaining zero-cost after first call).
    static THREAD_COUNTER: AtomicU64 = AtomicU64::new(1);

    /// Enable fault injection globally. Call from test harness setup.
    pub fn enable() {
        ENABLED.store(true, Ordering::Release);
    }

    /// Disable fault injection globally.
    pub fn disable() {
        ENABLED.store(false, Ordering::Release);
    }

    /// Returns whether fault injection is currently enabled.
    pub fn is_enabled() -> bool {
        ENABLED.load(Ordering::Acquire)
    }

    /// Set the global seed. Call BEFORE `enable()` so each thread picks up
    /// a consistent seed. Returns the previous seed.
    pub fn set_seed(seed: u64) -> u64 {
        SEED.swap(seed, Ordering::SeqCst)
    }

    /// Read the current global seed (for logging / reproduction).
    pub fn get_seed() -> u64 {
        SEED.load(Ordering::SeqCst)
    }

    // ── Thread-local xorshift64 RNG ────────────────────────────────────

    thread_local! {
        static RNG_STATE: Cell<u64> = const { Cell::new(0) };
        static THREAD_ID: Cell<u64> = const { Cell::new(0) };
    }

    /// Ensure the thread-local RNG is seeded. Idempotent per thread.
    #[inline]
    fn ensure_seeded() {
        THREAD_ID.with(|tid| {
            if tid.get() == 0 {
                let id = THREAD_COUNTER.fetch_add(1, Ordering::Relaxed);
                tid.set(id);
                let base = SEED.load(Ordering::Relaxed);
                let s = base ^ id;
                // xorshift64 state must be non-zero.
                RNG_STATE.with(|st| st.set(if s == 0 { 1 } else { s }));
            }
        });
    }

    /// Advance xorshift64 and return the next value.
    #[inline]
    fn next_u64() -> u64 {
        ensure_seeded();
        RNG_STATE.with(|st| {
            let mut s = st.get();
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            st.set(s);
            s
        })
    }

    /// Returns `true` with probability 1/n. Returns `false` if fault
    /// injection is globally disabled or `n == 0`.
    #[inline]
    pub fn should_fire(n: u64) -> bool {
        if n == 0 || !ENABLED.load(Ordering::Relaxed) {
            return false;
        }
        next_u64() % n == 0
    }

    /// Reset thread-local RNG state. Call between proptest cases to ensure
    /// each case starts with a fresh RNG derived from the global seed.
    pub fn reset_thread_local() {
        THREAD_ID.with(|tid| tid.set(0));
    }
}

// Public API re-exports when feature is enabled.
#[cfg(feature = "fault-injection")]
pub use inner::{disable, enable, get_seed, is_enabled, reset_thread_local, set_seed};

#[cfg(feature = "fault-injection")]
macro_rules! buggify {
    ($n:expr) => {
        $crate::fi::inner::should_fire($n as u64)
    };
    ($n:expr, $block:block) => {
        if $crate::fi::inner::should_fire($n as u64) $block
    };
}

#[cfg(feature = "fault-injection")]
pub(crate) use buggify;

// ── Init from environment ──────────────────────────────────────────────────

/// Initialize fault injection from the `KIKI_FI_SEED` environment variable.
/// Called early in daemon startup. No-op when the feature is disabled.
#[cfg(feature = "fault-injection")]
pub fn init_from_env() {
    if let Ok(seed_str) = std::env::var("KIKI_FI_SEED") {
        if let Ok(seed) = seed_str.parse::<u64>() {
            set_seed(seed);
            enable();
            tracing::info!(seed, "fault injection enabled via KIKI_FI_SEED");
        }
    }
}

#[cfg(not(feature = "fault-injection"))]
pub fn init_from_env() {}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "fault-injection"))]
mod tests {
    use super::inner;

    // Note: these tests use global state (ENABLED, SEED) that may be
    // modified by other tests running in parallel. Each test runs on a
    // dedicated thread to isolate thread-local RNG state and controls
    // enable/disable locally.

    /// Run a closure on a dedicated thread to isolate FI state.
    fn isolated(f: impl FnOnce() + Send + 'static) {
        std::thread::spawn(f).join().expect("fi test panicked");
    }

    #[test]
    fn fires_when_enabled_with_prob_1() {
        isolated(|| {
            inner::set_seed(42);
            inner::reset_thread_local();
            inner::enable();
            assert!(inner::should_fire(1)); // 1-in-1 = always
            inner::disable();
        });
    }

    #[test]
    fn does_not_fire_when_disabled() {
        isolated(|| {
            inner::set_seed(42);
            inner::reset_thread_local();
            inner::disable();
            assert!(!inner::should_fire(1)); // disabled → never fires
        });
    }

    #[test]
    fn deterministic_given_seed() {
        isolated(|| {
            inner::set_seed(99999);
            inner::reset_thread_local();
            inner::enable();

            let results_a: Vec<bool> = (0..100).map(|_| inner::should_fire(3)).collect();

            // Reset and replay with same seed.
            inner::set_seed(99999);
            inner::reset_thread_local();
            let results_b: Vec<bool> = (0..100).map(|_| inner::should_fire(3)).collect();

            assert_eq!(results_a, results_b);
            inner::disable();
        });
    }

    #[test]
    fn macro_expands_correctly() {
        isolated(|| {
            inner::set_seed(1);
            inner::reset_thread_local();
            inner::enable();

            let fired = buggify!(1); // 1-in-1 = always
            assert!(fired);

            let mut executed = false;
            buggify!(1, { executed = true; });
            assert!(executed);

            inner::disable();
        });
    }
}
