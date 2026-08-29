//! Load generator (container 1) and CLI.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};

use tailbench::clock::RealClock;
use tailbench::config::Config;
use tailbench::ready;
use tailbench::load_generator::{self, RunOutcome};
use tailbench::record::{RequestRecord, RunManifest};
use tailbench::report::{self, Report, ReportInput};
use tailbench::loadgen_client::LoadgenClient;
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
        /// Parent directory for run directories; each run creates its own
        /// timestamped subdirectory beneath it.
        #[arg(long, default_value = "results")]
        out: PathBuf,
        /// Repeat N times and report replay std. dev. of cvar_99 and p99 --
        /// the noise denominator the admission filter divides by.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Unix socket the `program` process is listening on.
        #[arg(long, default_value = "/run/tailbench/program.sock")]
        socket: PathBuf,
    },
    /// Aggregate an existing log.
    Report {
        #[arg(long)]
        log: PathBuf,
        #[arg(long)]
        config: PathBuf,
    },
    /// Check measured quantiles against the closed form.
    Validate {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "/run/tailbench/program.sock")]
        socket: PathBuf,
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
        } => cmd_run(&config, &out, repeat, &socket).await,
        Cmd::Report { log, config } => cmd_report(&log, &config),
        Cmd::Validate { config, socket } => cmd_validate(&config, &socket).await,
    }
}

async fn cmd_run(config: &Path, out: &Path, repeat: usize, socket: &Path) -> Result<()> {
    let cfg = Config::load(config)?;
    let run_dir = new_run_dir(out, &cfg.scenario.id)?;
    println!("run directory: {}", run_dir.display());

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
        write_log(
            &run_dir.join(format!("requests{suffix}.jsonl")),
            &outcome.records,
        )?;
        write_manifest(
            &run_dir.join(format!("run{suffix}.json")),
            &cfg,
            config,
            &outcome,
        )?;
        std::fs::write(
            run_dir.join(format!("report{suffix}.json")),
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
        // this is the number that goes in the README, and the
        // denominator the signal/noise gate divides by.
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
        // automatic, not a documentation note -- otherwise a laptop
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

async fn execute(cfg: &Config, socket: &Path) -> Result<RunOutcome> {
    ready::wait_for(socket).await?;
    let program = LoadgenClient::connect(socket)
        .await
        .with_context(|| format!("connecting to program at {}", socket.display()))?;
    load_generator::run(cfg, Timeline::generate(cfg), program, RealClock).await
}

/// Compare measured quantiles against the closed form.
///
/// The only check that measures against a *known answer* rather than against
/// another measurement, so it is what catches a silently broken pipeline. Uses
/// the same run path as everything else; needs a single-downstream scenario
/// with plenty of capacity, so queueing does not distort the distribution.
async fn cmd_validate(config: &Path, socket: &Path) -> Result<()> {
    let cfg = Config::load(config)?;
    let dist = cfg.downstreams[0].distribution.clone();
    let outcome = execute(&cfg, socket).await?;
    let rep = report::build(ReportInput {
        records: &outcome.records,
        warmup_s: cfg.scenario.warmup_s,
        duration_s: cfg.scenario.duration_s,
        penalty_ms: cfg.slo.penalty_ms(),
        coordinated_omission_failure: outcome.coordinated_omission_failure,
    });

    println!("{}", rep.summary());
    println!("measured vs analytic:");
    println!("  {:<6} {:>10} {:>10} {:>10}", "q", "measured", "analytic", "delta");
    for (label, q, measured) in [
        ("p50", 0.50, rep.p50),
        ("p90", 0.90, rep.p90),
        ("p99", 0.99, rep.p99),
    ] {
        if let Some(a) = dist.analytic_quantile(q) {
            println!("  {label:<6} {measured:>10.3} {a:>10.3} {:>+10.3}", measured - a);
        }
    }
    println!(
        "\nExpect a small positive bias: timer granularity is ~1ms, sleeps\n\
         overshoot, and the two socket hops add their own latency."
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

fn write_manifest(
    path: &Path,
    cfg: &Config,
    scenario_path: &Path,
    outcome: &RunOutcome,
) -> Result<()> {
    let m = RunManifest {
        scenario_id: cfg.scenario.id.clone(),
        scenario_path: scenario_path.display().to_string(),
        seed: cfg.scenario.seed,
        git_sha: git_sha(),
        program_sha256: program_sha256(),
        git_dirty: git_dirty(),
        program_variant: program_variant(),
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

/// `linux-pinned` only when a cpuset was actually applied -- a
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

/// The program's source, relative to the repo root. `run.sh` cds there before
/// launching, so a relative path is what both entry points see. Named once
/// because the path moves if the program becomes its own crate.
const PROGRAM_SRC: &str = "crates/program/src/main.rs";

/// SHA-256 of the program source, or `unknown`.
///
/// Shelled out rather than hand-rolled: the crate has no hash dependency and
/// this file already treats `git` and `uname` the same way. Never fails the
/// run -- a missing hash degrades provenance, it does not invalidate the
/// measurement.
fn program_sha256() -> String {
    // `shasum -a 256` on macOS, `sha256sum` on Linux; both print
    // `<hex>  <path>`.
    for (bin, args) in [
        ("shasum", &["-a", "256", PROGRAM_SRC][..]),
        ("sha256sum", &[PROGRAM_SRC][..]),
    ] {
        let out = std::process::Command::new(bin).args(args).output();
        if let Ok(o) = out {
            if o.status.success() {
                let text = String::from_utf8_lossy(&o.stdout);
                if let Some(hex) = text.split_whitespace().next() {
                    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                        return hex.to_ascii_lowercase();
                    }
                }
            }
        }
    }
    "unknown".into()
}

/// Whether the working tree has uncommitted changes. `false` if git is
/// unavailable -- the same answer a clean tree gives, but `git_sha` is `None`
/// there too, so the pair is never misleading.
fn git_dirty() -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Current branch name. `None` on a detached HEAD, where `rev-parse` reports
/// the literal `HEAD` and there is no label to record.
fn program_variant() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD")
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

/// Create a fresh directory for one run: `<out>/<UTC timestamp>-<scenario id>`.
///
/// Timestamp first so lexical order is chronological order. Each invocation
/// gets its own directory, so no run ever overwrites another and a series can
/// be compared after the fact. `--repeat` replays stay *inside* one directory:
/// they are one experiment measuring replay noise, not N separate runs.
fn new_run_dir(out: &Path, scenario_id: &str) -> Result<PathBuf> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = utc_stamp(secs);
    let slug: String = scenario_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Two runs can start within the same second; suffix rather than clobber.
    for attempt in 0..100 {
        let name = match attempt {
            0 => format!("{stamp}-{slug}"),
            n => format!("{stamp}-{slug}-{n}"),
        };
        let dir = out.join(name);
        match std::fs::create_dir_all(&dir) {
            // create_dir_all succeeds on an existing directory, so check
            // emptiness rather than trusting the return.
            Ok(()) if dir.read_dir()?.next().is_none() => return Ok(dir),
            Ok(()) => continue,
            Err(e) => return Err(e).context(format!("creating {}", dir.display())),
        }
    }
    anyhow::bail!(
        "could not find a free run directory under {}",
        out.display()
    )
}

/// `YYYYMMDD-HHMMSS` in UTC from a Unix timestamp.
///
/// Days-since-epoch to civil date, via Howard Hinnant's algorithm. UTC only,
/// which is why this is a dozen lines instead of a calendar dependency.
fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);

    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}
