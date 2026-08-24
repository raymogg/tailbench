//! §10.5: distribution and RNG tests.

use tailbench::dist::Distribution;
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

/// §4.1: bimodal is the important one -- it models a cache miss and is where
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

/// §5's structural claim: a draw depends only on its key, never on call order.
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
    // §5.3: attempt is in the key so P5's retries do not collide.
    assert_ne!(draw(1, 0, 0), draw(1, 0, 1), "attempt ignored");
}

#[test]
fn pareto_rejects_infinite_mean() {
    assert!(Distribution::Pareto { scale_ms: 1.0, alpha: 0.9 }.validate().is_err());
    assert!(Distribution::Pareto { scale_ms: 1.0, alpha: 1.5 }.validate().is_ok());
}
