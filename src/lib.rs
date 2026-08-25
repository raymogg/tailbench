//! tailbench -- an experimental environment for p99 optimization of async
//! services.
//!
//! Three processes: `loadgen` measures, `program` is the code under test, and
//! `downstreams` simulates its dependencies. Everything in this library is
//! harness apparatus; only `src/bin/program.rs` is open to optimization.

pub mod clock;
pub mod config;
pub mod distributions;
pub mod record;
pub mod rng;
pub mod timeline;
pub mod downstream;
pub mod load_generator;
pub mod oracle;
pub mod report;
pub mod protocol;
pub mod ready;
pub mod wire;
pub mod loadgen_client;
