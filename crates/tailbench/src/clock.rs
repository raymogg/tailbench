//! Time helpers.
//!
//! Previously a `Clock` trait with a single implementation, kept so a
//! determinism spike could swap in a virtual clock. That spike is not planned,
//! and one trait with one impl bought nothing but generic parameters threaded
//! through every caller -- so the trait is gone and time access is direct.

use std::time::{Duration, Instant};

/// Nanoseconds elapsed from `origin`, saturating at zero.
///
/// The log records every time as ns-since-run-start, so this is the one
/// conversion used everywhere.
pub fn ns_since(origin: Instant, t: Instant) -> u64 {
    t.saturating_duration_since(origin).as_nanos() as u64
}

pub fn ms_to_duration(ms: f64) -> Duration {
    Duration::from_nanos((ms * 1e6) as u64)
}
