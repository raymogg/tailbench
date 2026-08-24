//! Per-call-site RNG derivation (§5.3).
//!
//! A request's latency draw must not depend on what else the service is doing.
//! Deriving a fresh RNG from `(seed, request_id, downstream_id, attempt)` makes
//! that structural: request 400's latency at svc_b is the same number whatever
//! else is in flight, and a service that retries cannot shift the stream for
//! every subsequent call (§5.2 case 3).

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// splitmix64 finalizer. Explicit and documented rather than `DefaultHasher`,
/// whose output is not stable across Rust releases -- cross-version
/// reproducibility is the point.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// RNG for one downstream call. Order-independent by construction.
pub fn call_rng(seed: u64, request_id: u64, downstream_id: u16, attempt: u32) -> ChaCha8Rng {
    let mixed = splitmix64(
        seed ^ splitmix64(request_id)
            ^ splitmix64((downstream_id as u64) << 32 | attempt as u64),
    );
    ChaCha8Rng::seed_from_u64(mixed)
}

/// RNG for the arrival timeline (§5.5). Independent of every call stream, and
/// consumed entirely before the run starts.
pub fn arrival_rng(seed: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(splitmix64(seed ^ 0xA771_7A17_A771_7A17))
}

/// RNG for assigning request classes to timeline slots. Separate stream so
/// changing the class mix does not perturb arrival times.
pub fn class_rng(seed: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(splitmix64(seed ^ 0xC1A5_5C1A_55C1_A55C))
}

/// The value a downstream returns, folded into the response digest (§6.4).
/// Derived the same way as the latency draw so the oracle can compute the
/// expected digest offline without the service's cooperation.
pub fn call_digest(seed: u64, request_id: u64, downstream_id: u16) -> u64 {
    splitmix64(
        seed ^ splitmix64(request_id ^ 0xD16E_5700_D16E_5700)
            ^ splitmix64((downstream_id as u64) << 16),
    )
}

/// Fold per-call digests into a response digest. Order-independent: §6.3 does
/// not constrain call order, because doing so would forbid P4's fix (making a
/// serialized fan-out parallel).
pub fn fold_digest(nonce: u64, call_digests: &mut Vec<u64>) -> u64 {
    call_digests.sort_unstable();
    let mut acc = splitmix64(nonce);
    for d in call_digests.iter() {
        acc = splitmix64(acc ^ d);
    }
    acc
}

/// Per-request payload nonce. Unique per request_id, which is what makes
/// cross-request caching fail the oracle (§6.4).
pub fn payload_nonce(seed: u64, request_id: u64) -> u64 {
    splitmix64(seed ^ splitmix64(request_id ^ 0x9051_0AD9_0510_AD90))
}
