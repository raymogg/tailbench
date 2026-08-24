//! The loadgen <-> service protocol.
//!
//! The service receives the work it must do and returns what it did. It never
//! sees deadlines or scoring: classification is the harness\'s job, so a
//! service cannot influence its own verdict.

use serde::{Deserialize, Serialize};

use crate::record::CallSpan;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceRequest {
    pub tag: u64,
    pub request_id: u64,
    /// Downstreams this request must call. Order is not significant.
    pub required: Vec<String>,
    /// Unique per request, so cross-request caching fails the digest check.
    pub nonce: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceReply {
    pub tag: u64,
    pub digest: Option<u64>,
    pub spans: Vec<CallSpan>,
    pub error: Option<String>,
}
