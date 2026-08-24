# tailbench

An experimental environment for measuring and optimizing p99 latency of async
services under open-loop load.

## Quick start

```bash
cargo test
scripts/run.sh scenarios/smoke.toml          # 5 requests, checks the wiring
scripts/run.sh scenarios/fanout-bimodal.toml
```

The script starts all three processes, runs one scenario, and shuts down.
Results land in `results/`: `requests.jsonl` (one record per request),
`report.json`, and `run.json` (config, seed, git SHA, environment).

To run the processes by hand — useful for `--verbose` on the service, which
logs each request as it routes:

```bash
cargo run --release --bin mocks -- \
  --config scenarios/fanout-bimodal.toml --socket /tmp/tb/mocks.sock &

cargo run --release --bin service -- \
  --listen /tmp/tb/service.sock --mocks /tmp/tb/mocks.sock --verbose &

cargo run --release --bin loadgen -- run \
  --config scenarios/fanout-bimodal.toml --socket /tmp/tb/service.sock
```

Other subcommands:

```bash
loadgen run --repeat 5    # N runs; reports replay std. dev. of cvar_99 and p99
loadgen report --log results/requests.jsonl --config <scenario>
loadgen validate --config <scenario>   # measured quantiles vs the closed form
```

## Architecture

```
 loadgen (2 cores)      service (4 cores)      mocks (4 cores)
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│ timeline        │    │ fan out to      │    │ capacity+queue  │
│ dispatch loop   │─▶──│ required        │─▶──│ seeded latency  │
│ oracle, records │─◀──│ downstreams     │─◀──│ digest          │
└─────────────────┘    └─────────────────┘    └─────────────────┘
        │                       ▲
        ▼               the code under test
  results/*.jsonl       (the only editable part)
```

- **Separate processes, pinned cores.** Sharing a runtime with the code under
  test would let changes to it move the measurement.

- **Open-loop load.** Arrivals are scheduled up front and dispatched regardless
  of service state; a closed-loop generator would slow down under overload and
  hide the tail. Latency is measured from *intended* dispatch.

- **Deterministic downstreams.** Each latency draw comes from
  `(seed, request_id, downstream_id, attempt)`, so it does not depend on what
  else is in flight.

- **Harness-side scoring.** `Ok` means on time, required calls made, digest
  correct. Everything else enters the latency population at `penalty_ms`, so
  failing cannot improve the tail.

- **`cvar_99` is the primary metric**, p99 reported alongside. p99 is flat in
  `penalty_ms` below 1% failures and equal to it above; CVaR responds
  throughout. Outcome rates always accompany both.

## Scenario Config

```toml
[scenario]
id         = "fanout-bimodal-001"  # label, appears in run.json
seed       = 42                    # seeds arrivals, class mix, latency draws
duration_s = 20.0                  # length of the arrival timeline
warmup_s   = 2.0                   # leading window discarded before scoring

[load]
arrival       = "poisson"   # constant | poisson | bursty
rate_rps      = 300.0       # mean arrival rate
burstiness_cv = 1.4         # bursty only; coefficient of variation, must be > 1

[slo]
budget_ms  = 120.0          # per-request deadline
penalty_ms = 1200.0         # latency a failed request contributes; default 10x budget

[[request_class]]
name     = "cheap"
weight   = 0.9              # fraction of requests; all weights must sum to 1.0
requires = ["svc_a"]        # downstreams this class must call

[[request_class]]
name     = "expensive"
weight   = 0.1
requires = ["svc_a", "svc_b"]

[[downstream]]
id           = "svc_a"
distribution = { kind = "lognormal", median_ms = 8.0, sigma = 0.6 }
capacity     = 64           # concurrent calls before others queue
timeout_ms   = 250.0        # applies to queue + service time

[[downstream]]
id           = "svc_b"
distribution = { kind = "bimodal", fast_ms = 3.0, slow_ms = 180.0, p_slow = 0.02 }
capacity     = 16
timeout_ms   = 250.0
```

### The parameters worth understanding

**`budget_ms`** is a per-request deadline. A response arriving after
`intended_dispatch + budget_ms` results in `Expired`, exactly
as if it never arrived.

**`penalty_ms`** is the latency value a failed request contributes to the
percentile, defaulting to `10 × budget_ms`.
It must exceed `budget_ms`, or quitting would beat completing slowly.

**`weight` and `requires`** are what make requests heterogeneous. `requires` is a
*minimum* — a service may call a downstream more than once (retries, hedging)
without penalty, and call order is unconstrained. Both matter: fixing either
would forbid legitimate optimizations.

**`capacity`** is how many calls a downstream serves concurrently.

**`warmup_s`** requests are discarded on *intended* dispatch time, so the
boundary does not move with service behaviour.

### Latency distributions

| `kind` | Parameters | Notes |
|---|---|---|
| `constant` | `ms` | Useful for validation — the answer is known exactly |
| `uniform` | `min_ms`, `max_ms` | |
| `lognormal` | `median_ms`, `sigma` | The ordinary well-behaved dependency |
| `bimodal` | `fast_ms`, `slow_ms`, `p_slow` | Cache hit/miss; the important one |
| `pareto` | `scale_ms`, `alpha` | Heavy-tailed; `alpha > 1` required |
| `empirical` | `samples_ms` | Resampled from a trace |

`bimodal` is where mean and p99 diverge hardest. With `fast_ms = 3`,
`slow_ms = 180`, `p_slow = 0.02`, the mean is ~6.5ms and p99 is exactly 180ms —
a 28× gap. A service tuned on mean latency will barely notice it.

Invalid configs are rejected at load with the offending value named — unknown
fields, weights that do not sum to 1, `requires` naming an undeclared
downstream, out-of-domain distribution parameters.

## Measurement environment

Authoritative runs need a host that supports real CPU pinning: Linux with
cpusets, ideally `isolcpus` on the boot line and no hypervisor in the way.
over the 1ms threshold against a gate of 0.1%. That is the gate working.

Under Docker with cpusets applied:

```bash
cd docker && SCENARIO=fanout-bimodal.toml docker compose up
```
