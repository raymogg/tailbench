//! The open-loop property and the success definition, driven end to end over
//! a real socket against a stub service.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tailbench::clock::{Clock, RealClock};
use tailbench::config::Config;
use tailbench::load_generator;
use tailbench::protocol::{ProgramReply, ProgramRequest};
use tailbench::record::{CallOutcome, CallSpan, Outcome};
use tailbench::report::{self, ReportInput};
use tailbench::rng::{call_digest, fold_digest};
use tailbench::program_client::ProgramClient;
use tailbench::timeline::Timeline;
use tailbench::wire::{read_msg, write_msg};

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

#[derive(Clone, Copy)]
enum Mode {
    /// Correct: required call recorded, right digest, on time.
    Correct,
    /// On time, right-looking digest, but no downstream call was made.
    SkipWork,
    /// Call recorded but the digest is fabricated.
    BadDigest,
    /// Correct work, always past the deadline.
    Late,
    /// Sleeps far longer than the inter-arrival gap.
    Slow(Duration),
}

/// Stand-in for the service process. Speaks the real protocol over a real
/// socket, so the tests exercise the same path production uses.
async fn spawn_stub(mode: Mode, dispatched: Arc<AtomicUsize>) -> (std::path::PathBuf, Arc<ProgramClient>) {
    // A monotonic counter, not a timestamp: tests share a process and run on
    // several threads, and two starting in the same instant read the same
    // nanosecond, collide on the directory, and fail `bind` with EEXIST.
    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tailbench-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("service.sock");

    let listener = tailbench::ready::bind(&sock).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let dispatched = dispatched.clone();
            tokio::spawn(async move {
                let (mut rd, mut wr) = tokio::io::split(stream);
                let (tx, mut rx) = tokio::sync::mpsc::channel::<ProgramReply>(4096);
                tokio::spawn(async move {
                    while let Some(r) = rx.recv().await {
                        if write_msg(&mut wr, &r).await.is_err() {
                            break;
                        }
                    }
                });
                while let Ok(req) = read_msg::<_, ProgramRequest>(&mut rd).await {
                    dispatched.fetch_add(1, Ordering::Relaxed);
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(reply_for(mode, req).await).await;
                    });
                }
            });
        }
    });

    let client = ProgramClient::connect(&sock).await.unwrap();
    (sock, client)
}

async fn reply_for(mode: Mode, req: ProgramRequest) -> ProgramReply {
    let clock = RealClock;
    let span = CallSpan {
        downstream_id: "svc_a".into(),
        attempt: 0,
        queue_wait_ns: 0,
        service_ns: 1_000_000,
        outcome: CallOutcome::Ok,
    };
    let mut ds = vec![call_digest(42, req.request_id, 0)];
    let good = fold_digest(req.nonce, &mut ds);

    let (digest, spans) = match mode {
        Mode::Correct => (Some(good), vec![span]),
        Mode::SkipWork => (Some(good), Vec::new()),
        Mode::BadDigest => (Some(good ^ 0xDEAD), vec![span]),
        Mode::Late => {
            clock.sleep_until(clock.now() + Duration::from_millis(80)).await;
            (Some(good), vec![span])
        }
        Mode::Slow(d) => {
            clock.sleep_until(clock.now() + d).await;
            (Some(good), vec![span])
        }
    };
    ProgramReply {
        tag: req.tag,
        digest,
        spans,
        error: None,
    }
}

async fn run_mode(mode: Mode) -> Vec<Outcome> {
    let cfg = Config::from_toml_str(CFG).unwrap();
    let (_sock, client) = spawn_stub(mode, Arc::new(AtomicUsize::new(0))).await;
    let out = load_generator::run(&cfg, Timeline::generate(&cfg), client, RealClock)
        .await
        .unwrap();
    out.records.iter().map(|r| r.outcome).collect()
}

/// The most valuable test here.
///
/// A service 100x slower than the inter-arrival gap must still receive every
/// scheduled request. A closed-loop generator fails this immediately, and that
/// choice determines whether any of these measurements mean anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generator_is_open_loop() {
    let cfg = Config::from_toml_str(CFG).unwrap();
    let timeline = Timeline::generate(&cfg);
    let n = timeline.len();

    let dispatched = Arc::new(AtomicUsize::new(0));
    let (_sock, client) =
        spawn_stub(Mode::Slow(Duration::from_millis(500)), dispatched.clone()).await;

    let out = load_generator::run(&cfg, timeline, client, RealClock)
        .await
        .unwrap();

    let sent = dispatched.load(Ordering::Relaxed);
    assert_eq!(sent, n, "service received {sent} of {n} scheduled requests");
    assert_eq!(out.records.len(), n, "every request must produce a record");
}

/// Latency is measured from *intended* dispatch, so generator lag lands in the
/// numbers instead of vanishing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latency_measured_from_intended_dispatch() {
    let cfg = Config::from_toml_str(CFG).unwrap();
    let (_sock, client) = spawn_stub(
        Mode::Slow(Duration::from_millis(200)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let out = load_generator::run(&cfg, Timeline::generate(&cfg), client, RealClock)
        .await
        .unwrap();

    for r in out.records.iter().filter(|r| r.completion_ns.is_some()) {
        let e2e = r.e2e_ns().unwrap();
        let from_actual = r.completion_ns.unwrap() - r.actual_dispatch_ns;
        assert!(
            e2e >= from_actual,
            "e2e from intended ({e2e}) must be >= from actual ({from_actual})"
        );
    }
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

/// Skipping a required downstream is caught even when the response is on time
/// with a plausible digest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skipped_work_is_incorrect() {
    assert!(run_mode(Mode::SkipWork)
        .await
        .iter()
        .all(|o| *o == Outcome::Incorrect));
}

/// The digest depends on values only obtainable by calling the downstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fabricated_digest_is_incorrect() {
    assert!(run_mode(Mode::BadDigest)
        .await
        .iter()
        .all(|o| *o == Outcome::Incorrect));
}

/// Past the deadline the response has no value, correct or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_but_correct_is_expired() {
    assert!(run_mode(Mode::Late)
        .await
        .iter()
        .all(|o| *o == Outcome::Expired));
}

/// The property the penalty table exists to guarantee: abandoning requests must
/// score worse than completing them slowly, or the environment rewards giving
/// up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiring_is_worse_than_being_slow() {
    let cfg = Config::from_toml_str(CFG).unwrap();
    let penalty = cfg.slo.penalty_ms();

    async fn score(mode: Mode, penalty: f64) -> f64 {
        let cfg = Config::from_toml_str(CFG).unwrap();
        let (_sock, client) = spawn_stub(mode, Arc::new(AtomicUsize::new(0))).await;
        let out = load_generator::run(&cfg, Timeline::generate(&cfg), client, RealClock)
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
    }

    let completing = score(Mode::Correct, penalty).await;
    let expiring = score(Mode::Late, penalty).await;
    assert!(
        expiring > completing,
        "expiring ({expiring:.1}) must score worse than completing ({completing:.1})"
    );
}
