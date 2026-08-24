//! and: the open-loop property and the success definition.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tailbench::clock::{Clock, RealClock};
use tailbench::config::Config;
use tailbench::load_generator;
use tailbench::record::Outcome;
use tailbench::report::{self, ReportInput};
use tailbench::target::{Response, Target};
use tailbench::timeline::{ScheduledRequest, Timeline};

const CFG: &str = r#"
[scenario]
id = "t"
seed = 42
duration_s = 2.0
warmup_s = 0.0

[load]
arrival = "constant"
rate_rps = 200.0

[slo]
budget_ms = 50.0

[[request_class]]
name = "c"
weight = 1.0
requires = ["svc_a"]

[[downstream]]
id = "svc_a"
distribution = { kind = "constant", ms = 1.0 }
capacity = 1024
timeout_ms = 250.0
"#;

/// Sleeps far longer than the inter-arrival gap.
struct SlowTarget {
    delay: Duration,
    dispatched: Arc<AtomicUsize>,
}

impl Target for SlowTarget {
    async fn handle(&self, _req: &ScheduledRequest) -> anyhow::Result<Response> {
        self.dispatched.fetch_add(1, Ordering::Relaxed);
        let c = RealClock;
        c.sleep_until(c.now() + self.delay).await;
        Ok(Response { digest: None, spans: Vec::new(), failure: None })
    }
}

/// the most valuable test here.
///
/// A target 100x slower than the inter-arrival gap must still receive requests
/// at the configured rate. A closed-loop generator fails this immediately --
/// and since calls that choice determinative for the whole project, it
/// must not be able to regress silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generator_is_open_loop() {
    let cfg = Config::from_str(CFG).unwrap();
    let timeline = Timeline::generate(&cfg).without_requirements();
    let n = timeline.len();
    let dispatched = Arc::new(AtomicUsize::new(0));

    // 500ms handler at 200rps: a closed-loop generator would issue ~4 requests.
    let target = Arc::new(SlowTarget {
        delay: Duration::from_millis(500),
        dispatched: dispatched.clone(),
    });

    let out = load_generator::run(&cfg, timeline, target, RealClock).await.unwrap();
    let sent = dispatched.load(Ordering::Relaxed);
    assert_eq!(sent, n, "dispatched {sent} of {n} scheduled requests");

    // And every request has a record -- late ones included.
    assert_eq!(out.records.len(), n);
}

/// latency is measured from *intended* dispatch, so generator lag shows
/// up in the numbers instead of vanishing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latency_measured_from_intended_dispatch() {
    let cfg = Config::from_str(CFG).unwrap();
    let timeline = Timeline::generate(&cfg).without_requirements();
    let target = Arc::new(SlowTarget {
        delay: Duration::from_millis(200),
        dispatched: Arc::new(AtomicUsize::new(0)),
    });
    let out = load_generator::run(&cfg, timeline, target, RealClock).await.unwrap();

    for r in out.records.iter().filter(|r| r.completion_ns.is_some()) {
        let e2e = r.e2e_ns().unwrap();
        let from_actual = r.completion_ns.unwrap() - r.actual_dispatch_ns;
        assert!(
            e2e >= from_actual,
            "e2e from intended ({e2e}) must be >= from actual ({from_actual})"
        );
    }
}

// ---------------------------------------------------------------------------
// boundary semantics
// ---------------------------------------------------------------------------

struct ScriptedTarget {
    mode: Mode,
}

#[derive(Clone, Copy)]
enum Mode {
    /// Correct: calls svc_a, returns the right digest, on time.
    Correct,
    /// Returns on time but never calls the downstream.
    SkipWork,
    /// Calls the downstream but returns a fabricated digest.
    BadDigest,
    /// Correct work, but always late.
    Late,
}

impl Target for ScriptedTarget {
    async fn handle(&self, req: &ScheduledRequest) -> anyhow::Result<Response> {
        use tailbench::record::{CallOutcome, CallSpan};
        use tailbench::rng::{call_digest, fold_digest};

        let c = RealClock;
        let span = CallSpan {
            downstream_id: "svc_a".into(),
            attempt: 0,
            queue_wait_ns: 0,
            service_ns: 1_000_000,
            outcome: CallOutcome::Ok,
        };
        let mut ds = vec![call_digest(42, req.request_id, 0)];
        let good = fold_digest(req.nonce, &mut ds);

        match self.mode {
            Mode::Correct => Ok(Response {
                digest: Some(good),
                spans: vec![span],
                failure: None,
            }),
            Mode::SkipWork => Ok(Response {
                digest: Some(good),
                spans: Vec::new(),
                failure: None,
            }),
            Mode::BadDigest => Ok(Response {
                digest: Some(good ^ 0xDEAD),
                spans: vec![span],
                failure: None,
            }),
            Mode::Late => {
                c.sleep_until(c.now() + Duration::from_millis(80)).await;
                Ok(Response {
                    digest: Some(good),
                    spans: vec![span],
                    failure: None,
                })
            }
        }
    }
}

async fn run_mode(mode: Mode) -> Vec<Outcome> {
    let cfg = Config::from_str(CFG).unwrap();
    let timeline = Timeline::generate(&cfg);
    let out = load_generator::run(&cfg, timeline, Arc::new(ScriptedTarget { mode }), RealClock)
        .await
        .unwrap();
    out.records.iter().map(|r| r.outcome).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn correct_service_scores_ok() {
    let outcomes = run_mode(Mode::Correct).await;
    let ok = outcomes.iter().filter(|o| **o == Outcome::Ok).count();
    assert!(
        ok as f64 / outcomes.len() as f64 > 0.95,
        "only {ok}/{} ok",
        outcomes.len()
    );
}

/// skipping the required downstream is caught even when the response is
/// on time and the digest looks right.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skipped_work_is_incorrect() {
    let outcomes = run_mode(Mode::SkipWork).await;
    assert!(outcomes.iter().all(|o| *o == Outcome::Incorrect));
}

/// the digest depends on values only obtainable by calling downstreams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fabricated_digest_is_incorrect() {
    let outcomes = run_mode(Mode::BadDigest).await;
    assert!(outcomes.iter().all(|o| *o == Outcome::Incorrect));
}

/// past the deadline the response has no value, correct or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_but_correct_is_expired() {
    let outcomes = run_mode(Mode::Late).await;
    assert!(
        outcomes.iter().all(|o| *o == Outcome::Expired),
        "late responses must expire, got {:?}",
        &outcomes[..5.min(outcomes.len())]
    );
}

/// the property the penalty table exists to guarantee. Abandoning
/// requests must score *worse* than completing them slowly, or the environment
/// rewards giving up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiring_is_worse_than_being_slow() {
    let cfg = Config::from_str(CFG).unwrap();
    let penalty = cfg.slo.penalty_ms();

    let score = |mode: Mode| async move {
        let cfg = Config::from_str(CFG).unwrap();
        let timeline = Timeline::generate(&cfg);
        let out = load_generator::run(&cfg, timeline, Arc::new(ScriptedTarget { mode }), RealClock)
            .await
            .unwrap();
        report::build(ReportInput {
            records: &out.records,
            warmup_s: cfg.scenario.warmup_s,
            duration_s: cfg.scenario.duration_s,
            penalty_ms: penalty,
            coordinated_omission_failure: false,
        })
        .cvar_99
    };

    let slow_but_ok = score(Mode::Correct).await;
    let expired = score(Mode::Late).await;
    assert!(
        expired > slow_but_ok,
        "expiring ({expired:.1}) must score worse than completing ({slow_but_ok:.1})"
    );
}
