//! Open-loop load generator.
//!
//! The single choice that determines whether this project measures anything
//! real: requests are dispatched on a fixed timeline, independent of service
//! state. A closed-loop generator issues request N+1 only after N completes, so
//! an overloaded service automatically receives less load and its tail looks
//! healthy.

use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::clock::{ns_since, Clock};
use crate::config::Config;
use crate::oracle::Oracle;
use crate::protocol::ProgramReply;
use crate::record::{Outcome, RequestRecord};
use crate::loadgen_client::LoadgenClient;
use crate::timeline::{ScheduledRequest, Timeline};

/// dispatch later than this counts as a coordinated-omission event.
/// 1ms is a starting point, not a measured number: calibrate it against the
/// generator's actual capacity and record the chosen value in the run manifest.
pub const LATE_DISPATCH_THRESHOLD: Duration = Duration::from_millis(1);

/// Fraction of post-warmup requests that may be dispatched late before the run
/// is marked failed rather than merely slow.
pub const MAX_LATE_DISPATCH_FRAC: f64 = 0.001;

/// Generous, because filling it is a harness bug rather than a service
/// property. An unbounded channel here would literally be fault primitive P2
/// living inside the harness.
const RECORD_CHANNEL_CAP: usize = 65_536;

pub struct RunOutcome {
    pub records: Vec<RequestRecord>,
    pub late_dispatches: usize,
    pub coordinated_omission_failure: bool,
}

pub async fn run<C: Clock + Clone>(
    cfg: &Config,
    timeline: Timeline,
    program: Arc<LoadgenClient>,
    clock: C,
) -> Result<RunOutcome> {
    let oracle = Arc::new(Oracle::new(cfg));
    let budget = Duration::from_secs_f64(cfg.slo.budget_ms / 1000.0);
    let warmup = Duration::from_secs_f64(cfg.scenario.warmup_s);

    let (tx, mut rx) = mpsc::channel::<RequestRecord>(RECORD_CHANNEL_CAP);
    let collector = tokio::spawn(async move {
        let mut out = Vec::new();
        while let Some(r) = rx.recv().await {
            out.push(r);
        }
        out
    });

    let start = clock.now();
    let mut late_dispatches = 0usize;
    let mut late_post_warmup = 0usize;
    let mut post_warmup = 0usize;

    for req in timeline.requests.iter() {
        let deadline = start + req.offset;
        let now = clock.now();

        let lateness = now.saturating_duration_since(deadline);
        let is_post_warmup = req.offset >= warmup;
        if is_post_warmup {
            post_warmup += 1;
        }
        if lateness > LATE_DISPATCH_THRESHOLD {
            late_dispatches += 1;
            if is_post_warmup {
                late_post_warmup += 1;
            }
        }

        if now < deadline {
            clock.sleep_until(deadline).await;
        }

        let actual = clock.now();
        let req = req.clone();
        let program = program.clone();
        let oracle = oracle.clone();
        let clock2 = clock.clone();
        let tx = tx.clone();

        // Spawn, never await inline. Awaiting the handler here would be a
        // closed-loop generator wearing an open-loop costume -- the exact
        // failure this design exists to avoid. Covered by
        // `generator_is_open_loop`, so it cannot regress silently.
        tokio::spawn(async move {
            let resp = program.call(&req).await;
            let done = clock2.now();
            let rec = build_record(
                &req, &oracle, resp, start, actual, done, budget, lateness,
            );
            let _ = tx.send(rec).await;
        });
    }

    drop(tx);

    // Requests still in flight at end of timeline get until their own deadline
    // plus a grace margin; anything still outstanding is NeverServed.
    let grace = budget + Duration::from_secs(1);
    let deadline_all = start + timeline.duration + grace;
    let now = clock.now();
    if now < deadline_all {
        clock.sleep_until(deadline_all).await;
    }

    let mut records = collector.await?;

    // a request that never completed must still produce a line. If
    // unserved requests simply produce no record, the top row of the hack
    // table becomes invisible in the log format itself.
    let seen: std::collections::HashSet<u64> =
        records.iter().map(|r| r.request_id).collect();
    for req in timeline.requests.iter() {
        if !seen.contains(&req.request_id) {
            records.push(never_served(req, start, budget));
        }
    }
    records.sort_by_key(|r| r.request_id);

    let frac = if post_warmup == 0 {
        0.0
    } else {
        late_post_warmup as f64 / post_warmup as f64
    };
    Ok(RunOutcome {
        records,
        late_dispatches,
        coordinated_omission_failure: frac > MAX_LATE_DISPATCH_FRAC,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_record(
    req: &ScheduledRequest,
    oracle: &Oracle,
    resp: Result<ProgramReply>,
    start: Instant,
    actual: Instant,
    done: Instant,
    budget: Duration,
    lateness: Duration,
) -> RequestRecord {
    let intended_ns = req.offset.as_nanos() as u64;
    // stamped from *intended* dispatch, so generator lag cannot gift the
    // service extra time.
    let deadline_ns = intended_ns + budget.as_nanos() as u64;
    let completion_ns = ns_since(start, done);

    let (outcome, digest_ok, calls_met) =
        oracle.classify(req, &resp, completion_ns, deadline_ns);
    let (digest, spans) = match &resp {
        Ok(r) => (r.digest, r.spans.clone()),
        Err(_) => (None, Vec::new()),
    };

    RequestRecord {
        request_id: req.request_id,
        class: req.class.clone(),
        intended_dispatch_ns: intended_ns,
        actual_dispatch_ns: ns_since(start, actual),
        deadline_ns,
        // No streaming response over this transport, so there is no meaningful
        // first byte distinct from completion.
        first_byte_ns: None,
        completion_ns: Some(completion_ns),
        outcome,
        expired: completion_ns > deadline_ns,
        offered_load_rps: req.offered_load_rps,
        response_digest: digest,
        digest_ok,
        required_calls_met: calls_met,
        spans,
        late_dispatch_ns: lateness.as_nanos() as u64,
    }
}

fn never_served(req: &ScheduledRequest, _start: Instant, budget: Duration) -> RequestRecord {
    let intended_ns = req.offset.as_nanos() as u64;
    RequestRecord {
        request_id: req.request_id,
        class: req.class.clone(),
        intended_dispatch_ns: intended_ns,
        actual_dispatch_ns: intended_ns,
        deadline_ns: intended_ns + budget.as_nanos() as u64,
        first_byte_ns: None,
        completion_ns: None,
        outcome: Outcome::NeverServed,
        expired: true,
        offered_load_rps: req.offered_load_rps,
        response_digest: None,
        digest_ok: None,
        required_calls_met: false,
        spans: Vec::new(),
        late_dispatch_ns: 0,
    }
}
