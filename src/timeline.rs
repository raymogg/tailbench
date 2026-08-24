//! Precomputed arrival timeline.
//!
//! The whole schedule is generated before the run starts. Three reasons: it is
//! inspectable and testable without running anything; the dispatch loop does no
//! RNG work on the hot path; and the schedule is provably independent of
//! service behaviour, which is the open-loop property this whole design rests
//! on.

use rand::Rng;
use std::time::Duration;

use crate::config::{Arrival, Config};
use crate::rng::{arrival_rng, class_rng, payload_nonce};

#[derive(Clone, Debug)]
pub struct ScheduledRequest {
    pub request_id: u64,
    /// From run start. This is the *intended* dispatch time -- latency is
    /// measured from here, not from actual send.
    pub offset: Duration,
    pub class: String,
    pub required: Vec<String>,
    pub nonce: u64,
    /// instantaneous arrival rate at this offset, for v2's value
    /// function. Computed here because the timeline is the only place it is
    /// knowable, and it is unrecoverable afterwards.
    pub offered_load_rps: f64,
}

#[derive(Clone, Debug)]
pub struct Timeline {
    pub requests: Vec<ScheduledRequest>,
    pub duration: Duration,
}

/// Window for the sliding offered-load estimate. Wide enough to be stable at
/// a few hundred rps, narrow enough to resolve a burst.
const LOAD_WINDOW: Duration = Duration::from_millis(500);

impl Timeline {
    pub fn generate(cfg: &Config) -> Self {
        let duration = Duration::from_secs_f64(cfg.scenario.duration_s);
        let mut arng = arrival_rng(cfg.scenario.seed);
        let mut crng = class_rng(cfg.scenario.seed);

        let offsets = generate_offsets(cfg, &mut arng, duration);
        let loads = offered_load(&offsets, LOAD_WINDOW);

        // Cumulative weights for class sampling.
        let mut cum = Vec::with_capacity(cfg.request_classes.len());
        let mut acc = 0.0;
        for c in &cfg.request_classes {
            acc += c.weight;
            cum.push(acc);
        }

        let requests = offsets
            .into_iter()
            .enumerate()
            .map(|(i, offset)| {
                let request_id = i as u64;
                let u: f64 = crng.gen::<f64>();
                let idx = cum.iter().position(|c| u < *c).unwrap_or(cum.len() - 1);
                let class = &cfg.request_classes[idx];
                ScheduledRequest {
                    request_id,
                    offset,
                    class: class.name.clone(),
                    required: class.requires.clone(),
                    nonce: payload_nonce(cfg.scenario.seed, request_id),
                    offered_load_rps: loads[i],
                }
            })
            .collect();

        Timeline { requests, duration }
    }

    /// Strip required-call sets, for tests that drive the dispatch loop
    /// without a service or mocks behind it.
    pub fn without_requirements(mut self) -> Self {
        for r in &mut self.requests {
            r.required.clear();
        }
        self
    }

    pub fn len(&self) -> usize {
        self.requests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
}

fn generate_offsets(
    cfg: &Config,
    rng: &mut rand_chacha::ChaCha8Rng,
    duration: Duration,
) -> Vec<Duration> {
    let rate = cfg.load.rate_rps;
    let horizon = duration.as_secs_f64();
    let mut out = Vec::with_capacity((rate * horizon * 1.2) as usize + 16);

    match cfg.load.arrival {
        Arrival::Constant => {
            let gap = 1.0 / rate;
            let mut t = 0.0;
            while t < horizon {
                out.push(Duration::from_secs_f64(t));
                t += gap;
            }
        }
        Arrival::Poisson => {
            let mut t = 0.0;
            loop {
                // Exp(rate) via inverse CDF.
                let u: f64 = rng.gen::<f64>().max(f64::MIN_POSITIVE);
                t += -u.ln() / rate;
                if t >= horizon {
                    break;
                }
                out.push(Duration::from_secs_f64(t));
            }
        }
        Arrival::Bursty => {
            // Gamma inter-arrivals with shape solved for the requested CV:
            // shape = 1/cv^2, scale = 1/(rate*shape) so the mean gap is 1/rate.
            // Poisson is the cv = 1 special case, which is a useful
            // self-consistency check.
            let cv = cfg.load.burstiness_cv.expect("validated");
            let shape = 1.0 / (cv * cv);
            let scale = 1.0 / (rate * shape);
            let mut t = 0.0;
            loop {
                t += sample_gamma(rng, shape) * scale;
                if t >= horizon {
                    break;
                }
                out.push(Duration::from_secs_f64(t));
            }
        }
    }
    out
}

/// Marsaglia-Tsang for shape >= 1, with Johnk's boost for shape < 1.
/// Bursty needs shape < 1 (cv > 1), so the boost path is the common one here.
fn sample_gamma(rng: &mut rand_chacha::ChaCha8Rng, shape: f64) -> f64 {
    if shape < 1.0 {
        let u: f64 = rng.gen::<f64>().max(f64::MIN_POSITIVE);
        return sample_gamma(rng, shape + 1.0) * u.powf(1.0 / shape);
    }
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    loop {
        let x: f64 = {
            let u1: f64 = rng.gen::<f64>().max(f64::MIN_POSITIVE);
            let u2: f64 = rng.gen::<f64>();
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        };
        let v = (1.0 + c * x).powi(3);
        if v <= 0.0 {
            continue;
        }
        let u: f64 = rng.gen::<f64>().max(f64::MIN_POSITIVE);
        if u.ln() < 0.5 * x * x + d - d * v + d * v.ln() {
            return d * v;
        }
    }
}

/// Sliding-window arrival rate at each offset. Two-pointer over sorted offsets.
fn offered_load(offsets: &[Duration], window: Duration) -> Vec<f64> {
    let w = window.as_secs_f64();
    let half = w / 2.0;
    let mut out = Vec::with_capacity(offsets.len());
    let (mut lo, mut hi) = (0usize, 0usize);
    for &o in offsets {
        let t = o.as_secs_f64();
        while lo < offsets.len() && offsets[lo].as_secs_f64() < t - half {
            lo += 1;
        }
        while hi < offsets.len() && offsets[hi].as_secs_f64() <= t + half {
            hi += 1;
        }
        out.push((hi - lo) as f64 / w);
    }
    out
}
