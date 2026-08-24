//! Per-request records. Log everything, aggregate offline.
//!
//! All times are ns since run start: smaller than timestamps, exactly
//! comparable, and free of monotonic-vs-wall ambiguity.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Completed by deadline, required work done, digest matched.
    Ok,
    /// Completed after deadline, or never completed..
    Expired,
    /// On time but digest mismatched or required work skipped.
    Incorrect,
    /// Service returned an error.
    Error,
    /// Service refused/shed the request. Gate violation in v1.
    Dropped,
    /// No record produced by end of run. Must be emitted, never omitted --
    /// otherwise the top row of the hack table is invisible in the log format.
    NeverServed,
}

impl Outcome {
    pub fn is_ok(self) -> bool {
        matches!(self, Outcome::Ok)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    Ok,
    Timeout,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallSpan {
    pub downstream_id: String,
    pub attempt: u32,
    /// Time waiting for a capacity permit. Part of the call's latency, and the
    /// mechanism by which a downstream saturates.
    pub queue_wait_ns: u64,
    pub service_ns: u64,
    pub outcome: CallOutcome,
}

impl CallSpan {
    pub fn total_ns(&self) -> u64 {
        self.queue_wait_ns + self.service_ns
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    pub request_id: u64,
    pub class: String,
    pub intended_dispatch_ns: u64,
    pub actual_dispatch_ns: u64,
    /// intended_dispatch + budget_ms. Stamped from *intended* so
    /// generator lag cannot gift the service extra time.
    pub deadline_ns: u64,
    pub first_byte_ns: Option<u64>,
    pub completion_ns: Option<u64>,
    pub outcome: Outcome,
    pub expired: bool,
    /// arrival rate at intended dispatch, from the precomputed timeline.
    /// Unrecoverable after the fact; recorded so v2's value function is a
    /// scorer change rather than a re-run.
    pub offered_load_rps: f64,
    pub response_digest: Option<u64>,
    pub digest_ok: Option<bool>,
    pub required_calls_met: bool,
    pub spans: Vec<CallSpan>,
    /// 0 if dispatched on time..
    pub late_dispatch_ns: u64,
}

impl RequestRecord {
    /// Latency as measured, from *intended* dispatch. `None` if the
    /// request never completed.
    pub fn e2e_ns(&self) -> Option<u64> {
        self.completion_ns
            .map(|c| c.saturating_sub(self.intended_dispatch_ns))
    }

    /// the value this request contributes to the latency population.
    /// Every scheduled request contributes something -- a percentile over only
    /// successes is trivially gamed by failing the slow ones.
    pub fn scored_latency_ms(&self, penalty_ms: f64) -> f64 {
        match self.outcome {
            Outcome::Ok => self.e2e_ns().map(|ns| ns as f64 / 1e6).unwrap_or(penalty_ms),
            _ => penalty_ms,
        }
    }
}

/// Written once per run alongside the JSONL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunManifest {
    pub scenario_id: String,
    pub seed: u64,
    pub git_sha: Option<String>,
    /// `linux-pinned` or `unpinned`. Anything not `linux-pinned` is
    /// stamped non-authoritative in every report it appears in.
    pub environment: String,
    pub cpuset: Option<String>,
    pub worker_threads: Option<usize>,
    pub kernel: Option<String>,
    pub budget_ms: f64,
    pub penalty_ms: f64,
    pub warmup_s: f64,
    pub duration_s: f64,
    pub scheduled_requests: usize,
    pub late_dispatches: usize,
    pub coordinated_omission_failure: bool,
}

impl RunManifest {
    pub fn is_authoritative(&self) -> bool {
        self.environment == "linux-pinned"
    }
}
