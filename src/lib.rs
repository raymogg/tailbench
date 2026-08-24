//! tailbench -- an experimental environment for p99 optimization of async
//! services. See docs/phase1-step1-2-spec.md.

pub mod clock;
pub mod config;
pub mod dist;
pub mod record;
pub mod rng;
pub mod timeline;
pub mod downstream;
pub mod target;
pub mod harness;
pub mod oracle;
pub mod report;
