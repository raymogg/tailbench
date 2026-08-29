//! distribution and RNG tests.

use tailbench::distributions::Distribution;
use tailbench::rng::call_rng;

fn sample_n(d: &Distribution, n: usize, seed: u64) -> Vec<f64> {
    let mut rng = call_rng(seed, 0, 0, 0);
    let mut v: Vec<f64> = (0..n).map(|_| d.sample_ms(&mut rng)).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    sorted[((q * sorted.len() as f64) as usize).min(sorted.len() - 1)]
}

#[test]
fn quantiles_converge_to_closed_form() {
    let cases = vec![
        Distribution::Constant { ms: 5.0 },
        Distribution::Uniform { min_ms: 1.0, max_ms: 9.0 },
        Distribution::LogNormal { median_ms: 8.0, sigma: 0.6 },
        Distribution::Pareto { scale_ms: 4.0, alpha: 3.0 },
    ];
    for d in cases {
        let s = sample_n(&d, 200_000, 7);
        for q in [0.5, 0.9, 0.99] {
            let measured = quantile(&s, q);
            let analytic = d.analytic_quantile(q).unwrap();
            let rel = (measured - analytic).abs() / analytic.max(1e-9);
            assert!(
                rel < 0.05,
                "{d:?} q={q}: measured {measured:.3} vs analytic {analytic:.3}"
            );
        }
    }
}

/// bimodal is the important one -- it models a cache miss and is where
/// mean and p99 diverge hardest.
#[test]
fn bimodal_slow_fraction_and_p99() {
    let d = Distribution::Bimodal { fast_ms: 3.0, slow_ms: 180.0, p_slow: 0.02 };
    let n = 200_000;
    let s = sample_n(&d, n, 11);
    let slow = s.iter().filter(|x| **x > 100.0).count() as f64 / n as f64;
    // Binomial CI at n=200k, p=0.02 is about +/- 0.0006; allow 3x.
    assert!((slow - 0.02).abs() < 0.002, "slow fraction {slow}");
    // p99 is exactly slow_ms whenever p_slow > 0.01.
    assert_eq!(quantile(&s, 0.99), 180.0);
    // Mean stays near 6.5ms -- a 28x gap to p99.
    let mean: f64 = s.iter().sum::<f64>() / n as f64;
    assert!((mean - d.analytic_mean().unwrap()).abs() < 0.5, "mean {mean}");
}

#[test]
fn same_seed_same_sequence() {
    let d = Distribution::LogNormal { median_ms: 8.0, sigma: 0.6 };
    assert_eq!(sample_n(&d, 1000, 3), sample_n(&d, 1000, 3));
    assert_ne!(sample_n(&d, 1000, 3), sample_n(&d, 1000, 4));
}

/// the structural claim: a draw depends only on its key, never on call order.
#[test]
fn draws_are_order_independent() {
    let d = Distribution::LogNormal { median_ms: 8.0, sigma: 0.6 };
    let draw = |rid: u64, ds: u16| {
        let mut r = call_rng(42, rid, ds, 0);
        d.sample_ms(&mut r)
    };
    let forward: Vec<f64> = (0..500).map(|i| draw(i, 1)).collect();
    let backward: Vec<f64> = (0..500).rev().map(|i| draw(i, 1)).collect();
    let mut b = backward;
    b.reverse();
    assert_eq!(forward, b, "draw order changed the values");
}

#[test]
fn distinct_keys_give_distinct_draws() {
    let d = Distribution::LogNormal { median_ms: 8.0, sigma: 0.6 };
    let draw = |rid: u64, ds: u16, att: u32| {
        let mut r = call_rng(42, rid, ds, att);
        d.sample_ms(&mut r)
    };
    assert_ne!(draw(1, 0, 0), draw(2, 0, 0), "request_id ignored");
    assert_ne!(draw(1, 0, 0), draw(1, 1, 0), "downstream_id ignored");
    // attempt is in the key so P5's retries do not collide.
    assert_ne!(draw(1, 0, 0), draw(1, 0, 1), "attempt ignored");
}

/// Swapping two field values must not reproduce a stream.
///
/// The previous derivation combined the fields as `splitmix64(request_id) ^
/// splitmix64(downstream_id << 32 | attempt)`, which is symmetric: with
/// `downstream_id == 0` the attempt aliased into the request_id's space, so
/// `(request 1, attempt 2)` and `(request 2, attempt 1)` drew the same latency,
/// and any `request_id == attempt` collapsed to a single shared stream.
/// `distinct_keys_give_distinct_draws` missed it by varying one field at a time.
#[test]
fn swapped_fields_do_not_collide() {
    let d = Distribution::LogNormal {
        median_ms: 8.0,
        sigma: 0.6,
    };
    let draw = |rid: u64, ds: u16, att: u32| {
        let mut r = call_rng(42, rid, ds, att);
        d.sample_ms(&mut r)
    };

    // downstream_id 0 is the first-declared downstream -- the common case.
    assert_ne!(draw(1, 0, 2), draw(2, 0, 1), "request_id/attempt swap collides");
    assert_ne!(draw(3, 0, 1), draw(1, 0, 3), "request_id/attempt swap collides");
    // The degenerate case: request_id == attempt used to cancel to a constant.
    assert_ne!(draw(0, 0, 0), draw(1, 0, 1), "request_id == attempt collapses");
    assert_ne!(draw(2, 0, 2), draw(3, 0, 3), "request_id == attempt collapses");
    // request_id/downstream_id swap.
    assert_ne!(draw(1, 2, 0), draw(2, 1, 0), "request_id/downstream swap collides");

    // Exhaustive over a small grid: every distinct tuple, a distinct draw.
    let mut seen = std::collections::HashMap::new();
    for rid in 0..40u64 {
        for ds in 0..4u16 {
            for att in 0..4u32 {
                let bits = draw(rid, ds, att).to_bits();
                if let Some(prev) = seen.insert(bits, (rid, ds, att)) {
                    panic!("{prev:?} and {:?} share a stream", (rid, ds, att));
                }
            }
        }
    }
}

#[test]
fn pareto_rejects_infinite_mean() {
    assert!(Distribution::Pareto { scale_ms: 1.0, alpha: 0.9 }.validate().is_err());
    assert!(Distribution::Pareto { scale_ms: 1.0, alpha: 1.5 }.validate().is_ok());
}
