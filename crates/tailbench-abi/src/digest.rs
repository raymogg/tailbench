//! Response digest folding.
//!
//! `fold_digest` and its `splitmix64` primitive are the *only* part of the
//! harness's RNG that crosses to the program, and the split is deliberate.
//! Folding is something a program must do: it combines values it obtained by
//! actually calling downstreams.
//!
//! `call_digest` -- which *produces* those values from the seed -- stays
//! harness-side. A program holding it could compute every expected digest
//! offline and skip the work entirely, and under `scripts/run.sh` the seed is
//! readable from `scenarios/*.toml`. `payload_nonce` stays behind for the same
//! reason: `nonce` travels over the wire, and a program holding both could work
//! back toward the seed. With neither, the nonce is an opaque u64.

/// splitmix64 finalizer. Public because the harness derives its seeded draws
/// from it too; it is a hash primitive, not a secret. What must not cross is
/// `call_digest`, which turns the seed into a forgeable answer. Explicit and documented rather than `DefaultHasher`,
/// whose output is not stable across Rust releases -- cross-version
/// reproducibility is the point.
pub fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fold per-call digests into a response digest. Order-independent: call order
/// is deliberately unconstrained, because constraining it would forbid P4's fix
/// (making a serialized fan-out parallel).
pub fn fold_digest(nonce: u64, call_digests: &mut [u64]) -> u64 {
    call_digests.sort_unstable();
    let mut acc = splitmix64(nonce);
    for d in call_digests.iter() {
        acc = splitmix64(acc ^ d);
    }
    acc
}
