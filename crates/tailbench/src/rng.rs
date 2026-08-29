//! Per-call-site RNG derivation.
//!
//! A request's latency draw must not depend on what else the service is doing.
//! Deriving a fresh RNG from `(seed, request_id, downstream_id, attempt)` makes
//! that structural: request 400's latency at svc_b is the same number whatever
//! else is in flight, and a service that retries cannot shift the stream for
//! every subsequent call.

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

// splitmix64 and fold_digest live in `tailbench-abi`: the program needs the
// fold, so a single definition is the only way the two sides cannot drift.
// Everything below stays here -- these are the seeded draws, and a program
// holding `call_digest` could forge every response without doing the work.
use tailbench_abi::digest::splitmix64;

pub use tailbench_abi::digest::fold_digest;

/// Pack a call site into one integer, each field in its own bit range.
///
/// Packing rather than combining the fields with XOR. `splitmix64(a) ^
/// splitmix64(b)` is symmetric in `a` and `b`, so tuples that swap two field
/// values produce an identical stream -- `(request 1, attempt 2)` collided with
/// `(request 2, attempt 1)`. Disjoint bit ranges make every distinct tuple a
/// distinct integer, so no two call sites can alias.
///
/// Widths: attempt 8 bits, downstream_id 16, request_id 40. 255 retries on one
/// call is already pathological, and 40 bits of request_id is ~58,000 years at
/// 600 rps.
const ATTEMPT_BITS: u32 = 8;
const DOWNSTREAM_BITS: u32 = 16;

fn call_key(request_id: u64, downstream_id: u16, attempt: u32) -> u64 {
    debug_assert!(
        attempt < (1 << ATTEMPT_BITS),
        "attempt {attempt} exceeds {ATTEMPT_BITS} bits; draws would alias"
    );
    debug_assert!(
        request_id < (1 << (64 - ATTEMPT_BITS - DOWNSTREAM_BITS)),
        "request_id {request_id} exceeds 40 bits; draws would alias"
    );
    (request_id << (ATTEMPT_BITS + DOWNSTREAM_BITS))
        | ((downstream_id as u64) << ATTEMPT_BITS)
        | (attempt as u64 & ((1 << ATTEMPT_BITS) - 1))
}

/// RNG for one downstream call. Order-independent by construction.
pub fn call_rng(seed: u64, request_id: u64, downstream_id: u16, attempt: u32) -> ChaCha8Rng {
    let key = call_key(request_id, downstream_id, attempt);
    ChaCha8Rng::seed_from_u64(splitmix64(seed ^ splitmix64(key)))
}

/// RNG for the arrival timeline. Independent of every call stream, and
/// consumed entirely before the run starts.
pub fn arrival_rng(seed: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(splitmix64(seed ^ 0xA771_7A17_A771_7A17))
}

/// RNG for assigning request classes to timeline slots. Separate stream so
/// changing the class mix does not perturb arrival times.
pub fn class_rng(seed: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(splitmix64(seed ^ 0xC1A5_5C1A_55C1_A55C))
}

/// The value a downstream returns, folded into the response digest.
/// Derived the same way as the latency draw so the oracle can compute the
/// expected digest offline without the service's cooperation.
/// Packed the same way as the latency draw, with a domain constant keeping it a
/// separate stream -- otherwise the digest would be derivable from the latency.
/// No attempt field: the value a downstream returns is the same on every
/// attempt, which is what makes a retry a retry.
pub fn call_digest(seed: u64, request_id: u64, downstream_id: u16) -> u64 {
    let key = call_key(request_id, downstream_id, 0);
    splitmix64(seed ^ 0xD16E_5700_D16E_5700 ^ splitmix64(key))
}

/// Per-request payload nonce. Unique per request_id, which is what makes
/// cross-request caching fail the oracle.
pub fn payload_nonce(seed: u64, request_id: u64) -> u64 {
    splitmix64(seed ^ splitmix64(request_id ^ 0x9051_0AD9_0510_AD90))
}
