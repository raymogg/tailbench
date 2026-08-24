# tailbench

An experimental environment for p99 optimization of async services. Phase 1,
steps 1–2: open-loop load generator, mock downstream cluster, metrics.

Full design: [`docs/phase1-step1-2-spec.md`](docs/phase1-step1-2-spec.md).
Section references below (§N) point there.

## Status

| Spec step | State |
|---|---|
| 1. Clock, config, distributions | done, tested (§10.5) |
| 2. Records, aggregation | done, tested (§9.3) |
| 3. Load generator + synthetic target | done, tested (§10.1, §10.2) |
| 4. Oracle — deadlines, outcomes, digest | done, tested (§10.6) |
| 5. UDS transport, `mocks` binary | done; transport cost (§10.7) not yet measured |
| 6. Docker, cpusets, env capture | compose written; **unvalidated — needs the Linux host** |
| 7. `--repeat`, calibration | done; penalty sweep (§10.6) outstanding |

Not started: fault primitives, scenario sampler, integrity gates, admission
filter, splits.

## Quick start

```bash
cargo test                                        # 18 tests
./scripts/check-clock.sh                          # §1.1 enforcement

# In-process mocks -- fast to iterate, never authoritative (§1.2).
cargo run --release --bin loadgen -- \
  run --config scenarios/fanout-bimodal.toml --out out

# Split processes over a Unix socket, as authoritative runs use.
cargo run --release --bin mocks -- \
  --config scenarios/fanout-bimodal.toml --socket /tmp/tb/mocks.sock &
cargo run --release --bin loadgen -- \
  run --config scenarios/fanout-bimodal.toml --socket /tmp/tb/mocks.sock --out out

# §10.1: measured quantiles vs the closed form.
cargo run --release --bin loadgen -- validate --config scenarios/validate-lognormal.toml
```

`--repeat N` reports replay std. dev. of `cvar_99` and `p99` — the noise
denominator §7's `signal/noise ≥ 5` gate divides by.

## Measured so far

All numbers below are from **unpinned arm64 macOS** and are therefore
non-authoritative (§1.2.1). They are recorded because they already answer two
design questions, and because the Linux numbers should be compared against them.

### Harness measures what it should (§10.1)

Synthetic target, lognormal(median 8ms, σ 0.6), 300rps, 20s:

| q | measured | analytic | delta |
|---|---|---|---|
| p50 | 10.24 ms | 8.00 ms | +2.24 |
| p90 | 19.25 ms | 17.26 ms | +1.99 |
| p99 | 33.20 ms | 32.31 ms | +0.90 |

The positive bias is tokio timer overshoot, as predicted. Worth noting it is
roughly *constant additive* rather than proportional — so it distorts fast
requests far more than slow ones, which matters for §13's open question about
`fast_ms = 3` sitting near timer resolution.

### CVaR is the lower-variance statistic (§6.5.1)

5 replays, `fanout-bimodal`, in-process:

| statistic | mean | replay sd | sd / mean |
|---|---|---|---|
| `cvar_99` | 346.85 ms | **0.082 ms** | 0.02% |
| `p99` | 38.10 ms | 0.220 ms | 0.58% |

§6.5.1 predicted CVaR would have lower replay variance and therefore be the
better denominator for §7's admission gate. It holds here by 2.7× in absolute
terms and ~25× as a coefficient of variation — on the *noisiest* environment
available, which is the conservative direction.

Not yet tested against Pareto `alpha ≤ 2`, which §6.5.1 flags as where CVaR
could plausibly lose. §10.4 must cover every distribution before this is
treated as settled.

### The macOS environment cannot meet the dispatch gate (§1.2.1, §10.3)

Late-dispatch rate against a near-zero-latency target:

| rate | over 1ms threshold |
|---|---|
| 50 rps | 3.40% |
| 150 rps | 6.51% |
| 300 rps | 10.34% |
| 600 rps | 11.29% |

3.4% late at 50 rps — where the dispatch loop has 20ms of slack per request —
is not a capacity ceiling. It is macOS timer granularity, and it is why §1.2.1
requires a Linux host. The `MAX_LATE_DISPATCH_FRAC = 0.001` gate correctly
fails these runs rather than certifying them.

**The genuine generator capacity ceiling is therefore still unmeasured.** §10.3
must be re-run on the Linux host; only there does the number mean anything.

## What is enforced, not just documented

- **Open-loop dispatch.** `generator_is_open_loop` drives a 500ms handler at
  200rps and asserts every scheduled request is still dispatched. A closed-loop
  implementation fails immediately.
- **Latency from intended dispatch.** Generator lag lands in the measurement
  instead of vanishing.
- **Unserved requests are logged.** A request that never completes still
  produces a `NeverServed` record, so shedding cannot hide in the log format.
- **Failures enter the percentile** at `PENALTY_MS`.
  `expiring_is_worse_than_being_slow` asserts the property directly.
- **Skipped work and fabricated digests are `Incorrect`**, even when on time.
- **Clock discipline.** `scripts/check-clock.sh` fails the build on any direct
  `Instant::now` or `sleep` outside `src/clock.rs`.
- **Non-authoritative runs say so**, from an `environment` field derived from
  whether a cpuset was actually applied — not from a documentation note.

## Layout

```
src/
├── bin/loadgen.rs   # container 1: generator + recorder + CLI
├── bin/mocks.rs      # container 2: downstream cluster over UDS
├── clock.rs          # Clock trait; the only place time is read
├── config.rs         # scenario TOML + §3.1 validation
├── dist.rs           # 6 distributions with closed-form quantiles
├── rng.rs            # per-call-site derivation (§5.3)
├── timeline.rs       # precomputed arrivals (§7.1)
├── downstream.rs     # Downstream trait, in-process + UDS impls
├── target.rs         # Target trait, fanout + synthetic targets
├── load_generator.rs # open-loop dispatch loop
├── oracle.rs         # deadlines, outcomes, expected digest (§6)
├── record.rs         # per-request record types
└── report.rs         # percentiles, CVaR, outcome rates
```

`docker/compose.yml` defines the cpusets. It has not been run — validating it
is the first task on the Linux host.

## Next

1. Resolve base-image digests and run compose on the Linux host.
2. Re-run §10.3 (generator capacity) and §10.4 (replay noise) there. Those are
   the numbers that decide whether the §4 determinism spike is optional.
3. §10.7: measure what the UDS boundary costs at p99. The in-process impl is
   retained as the differential fixture.
4. §10.6 penalty sweep on CVaR, then freeze the multiplier across the task set.
