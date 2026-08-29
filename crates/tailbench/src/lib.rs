//! tailbench -- an experimental environment for p99 optimization of async
//! services.
//!
//! Three processes: `loadgen` measures, `program` is the code under test, and
//! `downstreams` simulates its dependencies. Everything in this library is
//! harness apparatus; only `crates/program/src/main.rs` is open to
//! optimization, and it cannot reach this crate -- it depends on
//! `tailbench-abi` instead, so the scorer and the seeded draws are a compile
//! error away rather than a convention away.

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
pub use tailbench_abi::protocol;
pub mod ready;
pub use tailbench_abi::wire;
pub mod loadgen_client;
