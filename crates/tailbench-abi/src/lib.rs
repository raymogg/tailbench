//! The contract between the harness and the program under test.
//!
//! This crate is everything a program legitimately needs: the wire protocol,
//! the framing, the downstream client, the readiness handshake, and the digest
//! fold. It is deliberately small -- roughly 150 lines of surface -- because it
//! is also the whole of what an agent editing the program has to understand.
//!
//! What is *not* here is the point. The scorer (`oracle`), the scenario
//! (`config`), the latency model (`distributions`, `timeline`), and the seeded
//! draws (`call_digest`, `call_rng`, `payload_nonce`) all stay in the harness
//! crate. `crates/program` depends on this crate and not on `tailbench`, so
//! reaching them is a compile error rather than a matter of discipline.

pub mod call;
pub mod digest;
pub mod protocol;
pub mod ready;
pub mod span;
pub mod wire;
