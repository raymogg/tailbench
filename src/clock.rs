//! Time access, behind a trait so §4's determinism spike can swap the impl.
//!
//! Nothing outside this module may call `Instant::now` or `tokio::time::sleep`
//! directly; `xtask/check-clock.sh` enforces it.

use std::future::Future;
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn sleep_until(&self, deadline: Instant) -> impl Future<Output = ()> + Send;
}

#[derive(Clone, Copy, Default, Debug)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&self, deadline: Instant) -> impl Future<Output = ()> + Send {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline))
    }
}

/// Nanoseconds elapsed from `origin`, saturating at zero.
///
/// The log records every time as ns-since-run-start (§9.1), so this is the one
/// conversion used everywhere.
pub fn ns_since(origin: Instant, t: Instant) -> u64 {
    t.saturating_duration_since(origin).as_nanos() as u64
}

pub fn ms_to_duration(ms: f64) -> Duration {
    Duration::from_nanos((ms * 1e6) as u64)
}
