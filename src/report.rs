//! Offline aggregation (§9.3).
//!
//! `cvar_99` is the primary metric (§6.5.1); p99 is reported alongside for
//! interpretability. Percentiles come from the sorted exact sample, not
//! histogram buckets: at these run sizes sorting is free and exact, and
//! relative-precision buckets would quantize the very statistic whose replay
//! std. dev. §7 divides by.

use serde::{Deserialize, Serialize};

use crate::record::{Outcome, RequestRecord};

/// `ceil`, tolerant of binary-floating-point error.
///
/// `(1.0 - 0.99) * 100.0` is 1.0000000000000009, and a bare `ceil()` turns that
/// into 2 -- so CVaR@99 of 100 samples would average the worst *two* values
/// instead of the worst one. Snap to an integer first when within tolerance.
fn ceil_robust(x: f64) -> usize {
    let r = x.round();
    if (x - r).abs() < 1e-9 {
        r as usize
    } else {
        x.ceil() as usize
    }
}

/// Nearest-rank, on the sorted sample. Stated explicitly because different
/// conventions give different p99s on identical data and §0's reproducibility
/// claim needs one written down.
pub fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = ceil_robust(q * sorted.len() as f64).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

/// Mean of the worst `ceil(n * (1-q))` values (§6.5.2).
///
/// Smoother than a single order statistic: every failure contributes its full
/// penalty proportionally, where p99 is flat below 1% failures and equals the
/// penalty above.
pub fn cvar(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let k = ceil_robust((1.0 - q) * sorted.len() as f64)
        .max(1)
        .min(sorted.len());
    let tail = &sorted[sorted.len() - k..];
    tail.iter().sum::<f64>() / k as f64
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub n: usize,
    pub n_post_warmup: usize,

    /// Primary optimization target (§6.5.1).
    pub cvar_99: f64,
    pub cvar_999: f64,
    /// Reported for interpretability.
    pub p99: f64,

    pub p50: f64,
    pub p90: f64,
    pub p999: f64,
    pub max: f64,
    pub mean: f64,
    pub throughput_rps: f64,

    /// §6.5.3: never optional. A tail statistic without these is
    /// uninterpretable -- a run can post an excellent p99 by failing 0.9%.
    pub ok_rate: f64,
    pub expiry_rate: f64,
    pub incorrect_rate: f64,
    pub error_rate: f64,
    pub dropped_count: usize,
    pub never_served_count: usize,

    /// Diagnostic only, never headline (§9.3). The gap against the penalised
    /// figures shows whether a service won on latency or on attrition.
    pub p99_ok_only: f64,
    pub cvar_99_ok_only: f64,

    pub downstream_calls: usize,
    pub downstream_timeouts: usize,

    pub penalty_ms: f64,
    /// True when a gate failed. A failed run still reports its metrics -- the
    /// Phase 1 spec's §1.5 wants failure distinguished from slowness, which
    /// requires the numbers to stay visible for debugging.
    pub failed: bool,
    pub failure_reason: Option<String>,
}

pub struct ReportInput<'a> {
    pub records: &'a [RequestRecord],
    pub warmup_s: f64,
    pub duration_s: f64,
    pub penalty_ms: f64,
    pub coordinated_omission_failure: bool,
}

pub fn build(input: ReportInput<'_>) -> Report {
    let warmup_ns = (input.warmup_s * 1e9) as u64;
    // Discard on *intended* dispatch, so the warmup boundary is independent of
    // service behaviour.
    let post: Vec<&RequestRecord> = input
        .records
        .iter()
        .filter(|r| r.intended_dispatch_ns >= warmup_ns)
        .collect();

    let n = post.len();
    let mut scored: Vec<f64> = post
        .iter()
        .map(|r| r.scored_latency_ms(input.penalty_ms))
        .collect();
    scored.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let mut ok_only: Vec<f64> = post
        .iter()
        .filter(|r| r.outcome.is_ok())
        .filter_map(|r| r.e2e_ns().map(|ns| ns as f64 / 1e6))
        .collect();
    ok_only.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let count = |o: Outcome| post.iter().filter(|r| r.outcome == o).count();
    let n_f = n.max(1) as f64;
    let measured_s = (input.duration_s - input.warmup_s).max(f64::EPSILON);

    let dropped = count(Outcome::Dropped);
    let never = count(Outcome::NeverServed);
    let mut failed = input.coordinated_omission_failure;
    let mut reason = if input.coordinated_omission_failure {
        Some("coordinated omission: late dispatches over threshold".to_string())
    } else {
        None
    };
    // §6.2: shedding is a gate violation in v1.
    if dropped > 0 {
        failed = true;
        reason = Some(format!("{dropped} requests dropped by the service"));
    }

    Report {
        n: input.records.len(),
        n_post_warmup: n,
        cvar_99: cvar(&scored, 0.99),
        cvar_999: cvar(&scored, 0.999),
        p99: percentile(&scored, 0.99),
        p50: percentile(&scored, 0.50),
        p90: percentile(&scored, 0.90),
        p999: percentile(&scored, 0.999),
        max: scored.last().copied().unwrap_or(f64::NAN),
        mean: scored.iter().sum::<f64>() / n_f,
        throughput_rps: count(Outcome::Ok) as f64 / measured_s,
        ok_rate: count(Outcome::Ok) as f64 / n_f,
        expiry_rate: count(Outcome::Expired) as f64 / n_f,
        incorrect_rate: count(Outcome::Incorrect) as f64 / n_f,
        error_rate: count(Outcome::Error) as f64 / n_f,
        dropped_count: dropped,
        never_served_count: never,
        p99_ok_only: percentile(&ok_only, 0.99),
        cvar_99_ok_only: cvar(&ok_only, 0.99),
        downstream_calls: post.iter().map(|r| r.spans.len()).sum(),
        downstream_timeouts: post
            .iter()
            .flat_map(|r| r.spans.iter())
            .filter(|s| s.outcome == crate::record::CallOutcome::Timeout)
            .count(),
        penalty_ms: input.penalty_ms,
        failed,
        failure_reason: reason,
    }
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if self.failed {
            s.push_str(&format!(
                "RUN FAILED: {}\n\n",
                self.failure_reason.as_deref().unwrap_or("gate violation")
            ));
        }
        s.push_str(&format!(
            "requests   {} ({} post-warmup)\n\
             cvar_99    {:.2} ms   <- primary\n\
             p99        {:.2} ms\n\
             p50/p90    {:.2} / {:.2} ms\n\
             p99.9/max  {:.2} / {:.2} ms\n\
             mean       {:.2} ms\n\
             throughput {:.1} rps\n\
             \n\
             ok         {:.3}%\n\
             expired    {:.3}%\n\
             incorrect  {:.3}%\n\
             error      {:.3}%\n\
             dropped    {}\n\
             unserved   {}\n\
             \n\
             diagnostic: p99(ok only) {:.2} ms, cvar_99(ok only) {:.2} ms\n\
             downstream: {} calls, {} timeouts\n",
            self.n,
            self.n_post_warmup,
            self.cvar_99,
            self.p99,
            self.p50,
            self.p90,
            self.p999,
            self.max,
            self.mean,
            self.throughput_rps,
            self.ok_rate * 100.0,
            self.expiry_rate * 100.0,
            self.incorrect_rate * 100.0,
            self.error_rate * 100.0,
            self.dropped_count,
            self.never_served_count,
            self.p99_ok_only,
            self.cvar_99_ok_only,
            self.downstream_calls,
            self.downstream_timeouts,
        ));
        s
    }
}
