//! Wire-visible span types, shared by the harness and the program.
//!
//! Lifted verbatim from `record.rs`: bincode is positional, so the field order
//! here is load-bearing. `Outcome` and `RequestRecord` stay harness-side -- the
//! program reports what it did, never what that was worth.

use serde::{Deserialize, Serialize};

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
