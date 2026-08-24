//! Load generator (§1.2, container 1) and CLI (§11).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tailbench::clock::{Clock, RealClock};
use tailbench::config::Config;
use tailbench::downstream::{InProcessCluster, UdsClient};
use tailbench::harness::{self, RunOutcome};
use tailbench::record::{RequestRecord, RunManifest};
use tailbench::report::{self, Report, ReportInput};
use tailbench::target::{FanoutTarget, SyntheticTarget};
use tailbench::timeline::Timeline;

#[derive(Parser, Debug)]
#[command(name = "loadgen", about = "tailbench load generator")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run a scenario and write the per-request log.
    Run {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "out")]
        out: PathBuf,
        /// Repeat N times and report replay std. dev. of cvar_99 and p99 --
        /// the noise denominator §7 divides by.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Talk to the `mocks` binary over this socket. Without it the mock
        /// cluster runs in-process, which is faster to iterate on but shares a
        /// runtime with the target (§1.2) and is never authoritative.
        #[arg(long)]
        socket: Option<String>,
    },
    /// Aggregate an existing log.
    Report {
        #[arg(long)]
        log: PathBuf,
        #[arg(long)]
        config: PathBuf,
    },
    /// §10.1: check measured quantiles against the closed form.
    Validate {
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Run {
            config,
            out,
            repeat,
            socket,
        } => cmd_run(&config, &out, repeat, socket.as_deref()).await,
        Cmd::Report { log, config } => cmd_report(&log, &config),
        Cmd::Validate { config } => cmd_validate(&config).await,
    }
}

async fn cmd_run(
    config: &Path,
    out: &Path,
    repeat: usize,
    socket: Option<&str>,
) -> Result<()> {
    let cfg = Config::load(config)?;
    std::fs::create_dir_all(out)?;

    let mut cvars = Vec::new();
    let mut p99s = Vec::new();
    let mut any_failed = false;

    for i in 0..repeat {
        let outcome = execute(&cfg, socket).await?;
        let rep = report::build(ReportInput {
            records: &outcome.records,
            warmup_s: cfg.scenario.warmup_s,
            duration_s: cfg.scenario.duration_s,
            penalty_ms: cfg.slo.penalty_ms(),
            coordinated_omission_failure: outcome.coordinated_omission_failure,
        });

        let suffix = if repeat > 1 {
            format!(".{i}")
        } else {
            String::new()
        };
        write_log(&out.join(format!("requests{suffix}.jsonl")), &outcome.records)?;
        write_manifest(&out.join(format!("run{suffix}.json")), &cfg, &outcome)?;
        std::fs::write(
            out.join(format!("report{suffix}.json")),
            serde_json::to_string_pretty(&rep)?,
        )?;

        if repeat > 1 {
            println!("--- replay {i} ---");
        }
        print!("{}", rep.summary());
        cvars.push(rep.cvar_99);
        p99s.push(rep.p99);
        any_failed |= rep.failed;
    }

    if repeat > 1 {
        // §10.4: this is the number that goes in the README, and the
        // denominator §7's signal/noise gate divides by.
        println!(
            "\nreplay over {repeat} runs:\n  cvar_99  mean {:.2} ms  sd {:.3} ms\n  \
             p99      mean {:.2} ms  sd {:.3} ms",
            mean(&cvars),
            stddev(&cvars),
            mean(&p99s),
            stddev(&p99s),
        );
    }

    let env = environment();
    if env != "linux-pinned" {
        // §1.2.1: automatic, not a documentation note -- otherwise a laptop
        // number ends up quoted as a result.
        println!(
            "\nNON-AUTHORITATIVE: environment={env}. Authoritative runs require a \
             Linux host with cpusets applied."
        );
    }

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

async fn execute(cfg: &Config, socket: Option<&str>) -> Result<RunOutcome> {
    let timeline = Timeline::generate(cfg);
    match socket {
        Some(path) => {
            wait_for_ready(path).await?;
            let client = UdsClient::connect(path)
                .await
                .with_context(|| format!("connecting to mocks at {path}"))?;
            let target = Arc::new(FanoutTarget {
                downstreams: client,
                seed: cfg.scenario.seed,
            });
            harness::run(cfg, timeline, target, RealClock).await
        }
        None => {
            let cluster = Arc::new(InProcessCluster::new(cfg, RealClock));
            let target = Arc::new(FanoutTarget {
                downstreams: cluster,
                seed: cfg.scenario.seed,
            });
            harness::run(cfg, timeline, target, RealClock).await
        }
    }
}

/// §1.4: `depends_on` waits for container start, not readiness. Without this
/// the first requests hit a cold or absent peer and poison the warmup.
async fn wait_for_ready(socket: &str) -> Result<()> {
    let ready = PathBuf::from(socket).with_extension("ready");
    for _ in 0..300 {
        if ready.exists() {
            return Ok(());
        }
        let c = RealClock;
        c.sleep_until(c.now() + std::time::Duration::from_millis(100))
            .await;
    }
    anyhow::bail!("timed out waiting for mocks readiness at {}", ready.display())
}

async fn cmd_validate(config: &Path) -> Result<()> {
    let cfg = Config::load(config)?;
    let dist = cfg.downstreams[0].distribution.clone();
    let timeline = Timeline::generate(&cfg).without_requirements();
    let target = Arc::new(SyntheticTarget {
        dist: dist.clone(),
        clock: RealClock,
        seed: cfg.scenario.seed,
    });
    let outcome = harness::run(&cfg, timeline, target, RealClock).await?;
    let rep = report::build(ReportInput {
        records: &outcome.records,
        warmup_s: cfg.scenario.warmup_s,
        duration_s: cfg.scenario.duration_s,
        penalty_ms: cfg.slo.penalty_ms(),
        coordinated_omission_failure: outcome.coordinated_omission_failure,
    });

    println!("{}", rep.summary());
    println!("measured vs analytic (§10.1):");
    println!("  {:<8} {:>10} {:>10} {:>10}", "q", "measured", "analytic", "delta");
    for (label, q, measured) in [
        ("p50", 0.50, rep.p50),
        ("p90", 0.90, rep.p90),
        ("p99", 0.99, rep.p99),
    ] {
        match dist.analytic_quantile(q) {
            Some(a) => println!(
                "  {label:<8} {measured:>10.3} {a:>10.3} {:>+10.3}",
                measured - a
            ),
            None => println!("  {label:<8} {measured:>10.3} {:>10} {:>10}", "n/a", "-"),
        }
    }
    println!(
        "\nExpect a small positive bias in every quantile: tokio timer\n\
         granularity is ~1ms and sleeps overshoot."
    );
    Ok(())
}

fn cmd_report(log: &Path, config: &Path) -> Result<()> {
    let cfg = Config::load(config)?;
    let text = std::fs::read_to_string(log)?;
    let records: Vec<RequestRecord> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let rep: Report = report::build(ReportInput {
        records: &records,
        warmup_s: cfg.scenario.warmup_s,
        duration_s: cfg.scenario.duration_s,
        penalty_ms: cfg.slo.penalty_ms(),
        coordinated_omission_failure: false,
    });
    print!("{}", rep.summary());
    Ok(())
}

fn write_log(path: &Path, records: &[RequestRecord]) -> Result<()> {
    let f = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::new(f);
    for r in records {
        serde_json::to_writer(&mut w, r)?;
        w.write_all(b"\n")?;
    }
    w.flush()?;
    Ok(())
}

fn write_manifest(path: &Path, cfg: &Config, outcome: &RunOutcome) -> Result<()> {
    let m = RunManifest {
        scenario_id: cfg.scenario.id.clone(),
        seed: cfg.scenario.seed,
        git_sha: git_sha(),
        environment: environment(),
        cpuset: std::env::var("TAILBENCH_CPUSET").ok(),
        worker_threads: std::env::var("TOKIO_WORKER_THREADS")
            .ok()
            .and_then(|v| v.parse().ok()),
        kernel: kernel(),
        budget_ms: cfg.slo.budget_ms,
        penalty_ms: cfg.slo.penalty_ms(),
        warmup_s: cfg.scenario.warmup_s,
        duration_s: cfg.scenario.duration_s,
        scheduled_requests: outcome.records.len(),
        late_dispatches: outcome.late_dispatches,
        coordinated_omission_failure: outcome.coordinated_omission_failure,
    };
    std::fs::write(path, serde_json::to_string_pretty(&m)?)?;
    Ok(())
}

/// §1.2.1. `linux-pinned` only when a cpuset was actually applied -- a
/// container without one looks pinned and is not.
fn environment() -> String {
    if cfg!(target_os = "linux") && std::env::var("TAILBENCH_CPUSET").is_ok() {
        "linux-pinned".into()
    } else {
        "unpinned".into()
    }
}

fn git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn kernel() -> Option<String> {
    std::process::Command::new("uname")
        .arg("-a")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn stddev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}
