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

// `CallSpan`/`CallOutcome` moved to `tailbench-abi`: they cross the wire, so
// the program needs them too. Re-exported rather than re-declared -- two
// definitions of a bincode type is exactly how a wire format drifts.
pub use tailbench_abi::span::{CallOutcome, CallSpan};

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
    /// Path of the scenario file, as given on the command line. `scenario_id`
    /// is a label the file sets about itself; this is what was actually run.
    pub scenario_path: String,
    pub seed: u64,
    pub git_sha: Option<String>,
    /// SHA-256 of the program's source file. `git_sha` alone cannot tell two
    /// variants apart -- an agent iterating on one file leaves a dirty tree as
    /// the normal state, so every such run records the same commit. This
    /// pins the exact source that ran, and survives a rebase onto a newer
    /// harness. `unknown` if the file could not be read or hashed.
    ///
    /// Defaulted, like the two below: runs written before this field existed
    /// are still read by the notebooks.
    #[serde(default)]
    pub program_sha256: String,
    /// Whether the tree had uncommitted changes, i.e. whether `git_sha`
    /// describes what actually ran.
    #[serde(default)]
    pub git_dirty: bool,
    /// Human label for the variant -- the branch name. `None` on a detached
    /// HEAD or outside a checkout.
    #[serde(default)]
    pub program_variant: Option<String>,
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
