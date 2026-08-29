//! Latency distributions.
//!
//! Every kind except `Empirical` has a closed-form quantile, which is what lets
//! check a measured p99 against a known answer rather than against
//! another measurement.

use anyhow::{bail, Result};
use rand::Rng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::clock::ms_to_duration;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Distribution {
    Constant {
        ms: f64,
    },
    Uniform {
        min_ms: f64,
        max_ms: f64,
    },
    #[serde(rename = "lognormal")]
    LogNormal {
        median_ms: f64,
        sigma: f64,
    },
    Bimodal {
        fast_ms: f64,
        slow_ms: f64,
        p_slow: f64,
    },
    Pareto {
        scale_ms: f64,
        alpha: f64,
    },
    Empirical {
        samples_ms: Vec<f64>,
    },
}

impl Distribution {
    /// reject out-of-domain parameters at config load, not at first draw.
    pub fn validate(&self) -> Result<()> {
        match self {
            Distribution::Constant { ms } => {
                if !ms.is_finite() || *ms < 0.0 {
                    bail!("constant: ms must be finite and >= 0, got {ms}");
                }
            }
            Distribution::Uniform { min_ms, max_ms } => {
                if !min_ms.is_finite() || !max_ms.is_finite() || *min_ms < 0.0 {
                    bail!("uniform: bounds must be finite and >= 0");
                }
                if min_ms > max_ms {
                    bail!("uniform: min_ms ({min_ms}) > max_ms ({max_ms})");
                }
            }
            Distribution::LogNormal { median_ms, sigma } => {
                if !median_ms.is_finite() || *median_ms <= 0.0 {
                    bail!("lognormal: median_ms must be finite and > 0, got {median_ms}");
                }
                if !sigma.is_finite() || *sigma <= 0.0 {
                    bail!("lognormal: sigma must be finite and > 0, got {sigma}");
                }
            }
            Distribution::Bimodal {
                fast_ms,
                slow_ms,
                p_slow,
            } => {
                if !fast_ms.is_finite() || !slow_ms.is_finite() || *fast_ms < 0.0 {
                    bail!("bimodal: fast_ms/slow_ms must be finite and >= 0");
                }
                if fast_ms > slow_ms {
                    bail!("bimodal: fast_ms ({fast_ms}) > slow_ms ({slow_ms})");
                }
                if !(0.0..=1.0).contains(p_slow) {
                    bail!("bimodal: p_slow must be in [0,1], got {p_slow}");
                }
            }
            Distribution::Pareto { scale_ms, alpha } => {
                if !scale_ms.is_finite() || *scale_ms <= 0.0 {
                    bail!("pareto: scale_ms must be finite and > 0, got {scale_ms}");
                }
                // alpha <= 1 has infinite mean, so any measured mean is
                // meaningless. alpha <= 2 has infinite variance -- legitimate,
                // but destabilises the replay std. dev. that the admission
                // filter divides by.
                if !alpha.is_finite() || *alpha <= 1.0 {
                    bail!("pareto: alpha must be > 1 (alpha <= 1 has infinite mean), got {alpha}");
                }
            }
            Distribution::Empirical { samples_ms } => {
                if samples_ms.is_empty() {
                    bail!("empirical: samples_ms is empty");
                }
                if samples_ms.iter().any(|s| !s.is_finite() || *s < 0.0) {
                    bail!("empirical: all samples must be finite and >= 0");
                }
            }
        }
        Ok(())
    }

    /// One draw. Callers supply a per-call-site RNG, never a shared one.
    pub fn sample(&self, rng: &mut ChaCha8Rng) -> Duration {
        ms_to_duration(self.sample_ms(rng))
    }

    pub fn sample_ms(&self, rng: &mut ChaCha8Rng) -> f64 {
        match self {
            Distribution::Constant { ms } => *ms,
            Distribution::Uniform { min_ms, max_ms } => rng.gen_range(*min_ms..=*max_ms),
            Distribution::LogNormal { median_ms, sigma } => {
                // mu = ln(median), so the distribution is parameterised by the
                // median directly -- easier to reason about than by mu.
                let z: f64 = sample_standard_normal(rng);
                median_ms * (sigma * z).exp()
            }
            Distribution::Bimodal {
                fast_ms,
                slow_ms,
                p_slow,
            } => {
                if rng.gen::<f64>() < *p_slow {
                    *slow_ms
                } else {
                    *fast_ms
                }
            }
            Distribution::Pareto { scale_ms, alpha } => {
                // Inverse-CDF: scale * (1-u)^(-1/alpha).
                let u: f64 = rng.gen::<f64>();
                scale_ms * (1.0 - u).powf(-1.0 / alpha)
            }
            Distribution::Empirical { samples_ms } => {
                samples_ms[rng.gen_range(0..samples_ms.len())]
            }
        }
    }

    /// Closed-form quantile, or `None` for `Empirical` (use the order statistic).
    ///
    /// This is the reference that measured quantiles are validated against.
    pub fn analytic_quantile(&self, q: f64) -> Option<f64> {
        assert!((0.0..1.0).contains(&q), "quantile must be in [0,1)");
        match self {
            Distribution::Constant { ms } => Some(*ms),
            Distribution::Uniform { min_ms, max_ms } => Some(min_ms + q * (max_ms - min_ms)),
            Distribution::LogNormal { median_ms, sigma } => {
                Some(median_ms * (sigma * inverse_standard_normal_cdf(q)).exp())
            }
            Distribution::Bimodal {
                fast_ms,
                slow_ms,
                p_slow,
            } => Some(if q < 1.0 - p_slow { *fast_ms } else { *slow_ms }),
            Distribution::Pareto { scale_ms, alpha } => {
                Some(scale_ms * (1.0 - q).powf(-1.0 / alpha))
            }
            Distribution::Empirical { .. } => None,
        }
    }

    /// Analytic mean where one exists. Used by tests, not by the harness.
    pub fn analytic_mean(&self) -> Option<f64> {
        match self {
            Distribution::Constant { ms } => Some(*ms),
            Distribution::Uniform { min_ms, max_ms } => Some((min_ms + max_ms) / 2.0),
            Distribution::LogNormal { median_ms, sigma } => {
                Some(median_ms * (sigma * sigma / 2.0).exp())
            }
            Distribution::Bimodal {
                fast_ms,
                slow_ms,
                p_slow,
            } => Some(fast_ms * (1.0 - p_slow) + slow_ms * p_slow),
            // Finite only for alpha > 1, which validate() already requires.
            Distribution::Pareto { scale_ms, alpha } => Some(alpha * scale_ms / (alpha - 1.0)),
            Distribution::Empirical { samples_ms } => {
                Some(samples_ms.iter().sum::<f64>() / samples_ms.len() as f64)
            }
        }
    }
}

/// Box-Muller. `rand_distr` has this, but rolling it explicitly keeps the draw
/// count per sample fixed and known, which matters for the reproducibility.
fn sample_standard_normal(rng: &mut ChaCha8Rng) -> f64 {
    let u1: f64 = rng.gen::<f64>().max(f64::MIN_POSITIVE);
    let u2: f64 = rng.gen::<f64>();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Acklam's inverse normal CDF. Accurate to ~1.15e-9 relative, which is far
/// tighter than the sampling error at the run sizes.
// Constants are Acklam's as published. Kept digit-for-digit rather than
// trimmed to f64 precision: they are the reference the analytic quantiles are
// checked against, and matching the source exactly is worth more than the lint.
#[allow(clippy::excessive_precision)]
pub fn inverse_standard_normal_cdf(p: f64) -> f64 {
    assert!((0.0..1.0).contains(&p));
    const A: [f64; 6] = [
        -3.969683028665376e+01,
        2.209460984245205e+02,
        -2.759285104469687e+02,
        1.383577518672690e+02,
        -3.066479806614716e+01,
        2.506628277459239e+00,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e+01,
        1.615858368580409e+02,
        -1.556989798598866e+02,
        6.680131188771972e+01,
        -1.328068155288572e+01,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e+00,
        -2.549732539343734e+00,
        4.374664141464968e+00,
        2.938163982698783e+00,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e+00,
        3.754408661907416e+00,
    ];
    const P_LOW: f64 = 0.02425;
    const P_HIGH: f64 = 1.0 - P_LOW;

    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= P_HIGH {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}
