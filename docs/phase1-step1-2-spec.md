# tailbench — Codebase Spec: Load Generator, Mock Downstreams, Metrics

Covers build-order steps 1 and 2 from the Phase 1 spec. Out of scope: fault
primitives, scenario sampling, integrity gates, admission filter, splits.

## 0. Scope and deliverable

Two things must be true when this is done:

1. An open-loop load generator drives a service at a specified arrival process
   and records a per-request log, validated against a synthetic service whose
   latency distribution is known analytically.
2. A mock downstream cluster produces latencies from six seeded distributions,
   reproducible from `(seed, request_id, downstream_id)`.

Both require a third thing the Phase 1 spec leaves implicit: **a definition of
what counts as a successful request** — and, following from it, which statistic
is the optimization target (§6.5.1: CVaR@99, with p99 reported alongside). §6 supplies it — deadlines, outcome
taxonomy, and the correctness oracle — because p99 is meaningless without a rule
for which requests are in the population and at what value.

Everything here is a library plus one binary. No task directories, no scenario
sampler, no scoring beyond what's needed to validate the above.

## 1. Deliberate deviations from the Phase 1 spec

Each with the reasoning, because they're the decisions most worth disagreeing
with. §1.2 in particular reverses an earlier draft of this spec, which put the
mocks in-process; the reasoning for the reversal is stated there.

### 1.1 Real time, not simulated time

§4 leaves simulated time open pending a one-week spike, and that spike is step 3
— after this work. Building steps 1–2 against `turmoil` or `madsim` would commit
to an unvalidated bet before it's evaluated.

So: real tokio, real `Instant`. But every time read and every sleep goes through
a `Clock` trait (§5.6). Nothing else in the codebase calls `Instant::now` or
`tokio::time::sleep` directly. If the spike succeeds, it swaps one impl; if it
fails, the trait costs one indirection.

This is not a prediction that the spike fails. It's arranging so the answer
doesn't require a rewrite either way.

### 1.2 Process isolation — three components, three containers

**The requirement.** The load generator, the mock downstream cluster, and the
service under test each run in their own process, on disjoint sets of pinned
cores.

The reason is a confound, not merely noise. If the generator's timer tasks and
the mocks' sleeps share a tokio runtime with the service under test, then
changing the service changes the scheduling environment the *measurement itself*
runs in. A service that spawns fewer tasks leaves more scheduler headroom for
the generator, which dispatches more punctually, which lowers measured latency —
an improvement in the number that is not an improvement in the service. That is
not variance to be averaged away over K replays; it is bias correlated with
exactly the variable under study. Nothing downstream can correct for it.

So the service under test must be the only meaningful user of its runtime.

**Isolation comes from pinning, not from containers.** Worth stating plainly
because the two get conflated: a container does not by itself grant dedicated
cores. By default containers share the host CPU pool and the kernel schedules
their threads anywhere. Dedicated cores come from `--cpuset-cpus` (and, for the
real guarantee, `isolcpus` on the host). Docker's contribution is packaging and
environment reproducibility — pinned base image, pinned toolchain, identical
kernel-visible config across machines — which §7 needs when it asks for
admission runs across ≥ 2 machine configurations. Both matter. They are not the
same thing, and containerising without pinning yields none of the isolation.

**Layout**, for a 12-core host:

| Component | Cores | tokio worker threads |
|---|---|---|
| `loadgen` | 2 | 2 |
| `service` (under test) | 4 | 4 |
| `mocks` | 4 | 4 |
| host / OS | 2 | — |

Worker threads must equal the cpuset width. A tokio runtime that defaults to
`num_cpus` sees the *host* count, not the cpuset, and oversubscribes — 12 worker
threads fighting over 4 cores reintroduces the scheduling jitter the split
exists to remove. Set `worker_threads` explicitly from the cpuset in every
container.

**The cost, stated honestly.** This reverses the in-process design and the
reason for that design does not disappear: mock calls now cross a process
boundary, and that transport contributes its own latency *and its own tail* to
every measurement. Loopback TCP round-trip is roughly 20–50µs, which against a
3ms `fast_ms` is ~1% of the median — but the relevant quantity is the transport's
p99, not its median, and it lands directly in the tail being measured.

This is a real trade, not a free win: the split removes a bias correlated with
the optimization target and adds unbiased variance in its place. That is the
right direction — bias cannot be averaged out and variance can — but the
variance must be quantified, not assumed small. §10.7 measures it and states the
transport's contribution to measured p99 and CVaR. If it turns out large
relative to the naive→expert gap, revisit before building four more primitives.

Use **Unix domain sockets** over a shared volume rather than TCP: lower latency,
tighter tail, no TCP stack, no Nagle, no port exhaustion.

**Only one run mode ships.** An earlier draft kept an in-process `Downstream`
impl alongside the UDS one, as a development convenience and as the differential
fixture for §10.7. That is now removed: two modes produce two sets of numbers
that are not comparable, and the cheaper one is the one people reach for by
habit. The cost is that §10.7 can no longer difference in-process against UDS —
measure the transport with a null-latency probe (`transport_probe`) instead, and
state that figure rather than a difference.

**A benefit worth naming:** this makes the Phase 1 spec's §6 integrity boundary
real. "The model may only change the service crate" stops being a convention
enforced by a file hash and becomes a process boundary — the service cannot
reach the harness's clock, the mocks' RNG, or the metrics log, because they are
not in its address space.

### 1.2.1 Authoritative runs are on a Linux bare-metal host

macOS provides no CPU affinity mechanism on Apple Silicon: `thread_policy_set`
with `THREAD_AFFINITY_POLICY` returns `KERN_NOT_SUPPORTED`, there is no
`sched_setaffinity`, and Docker's `--cpuset-cpus` pins only to VM vCPUs that the
host scheduler still migrates freely. Add heterogeneous P/E cores and thermal
throttling and local timing is not controllable in the way §1.2 requires.

So the project runs authoritative experiments on a **Linux bare-metal box** with
real cpusets and `isolcpus`. Everything that produces a number anyone relies on
— §7 admission, §10.4 replay tolerance, §10.7 transport cost, the baseline eval
— runs there.

Local macOS runs are for development: correctness, wiring, and iteration speed.
The same compose file runs; the numbers are not trustworthy.

The runner must **detect this and mark it**, not leave it to documentation.
`run.json` carries an `environment` field (`linux-pinned` | `unpinned`), set
from whether cpusets were actually applied, and any run not `linux-pinned` is
stamped non-authoritative in every report it appears in. Otherwise a laptop
number ends up quoted as a result.

Steps 1–4 are correctness work that unpinned runs can validate. The Linux host
is needed before step 5's transport measurement and step 4's admission runs.

### 1.3 One crate, three binaries

§1.2 forces separate processes, so the "one crate" simplification survives only
in a modified form: one crate, one shared library, **three binary targets**.

```
src/
├── lib.rs
├── bin/
│   ├── loadgen.rs    # container 1: open-loop generator + recorder
│   ├── mocks.rs      # container 2: downstream cluster server
│   └── service.rs    # container 3: service under test (step 4+)
├── clock.rs          # Clock trait + RealClock
├── config.rs         # scenario TOML -> types
├── dist.rs           # latency distributions
├── downstream.rs     # Downstream trait + UDS client/server
├── harness.rs        # timeline, dispatch loop
├── record.rs         # per-request records, log writer
├── target.rs         # Target trait + synthetic validation target
├── oracle.rs         # deadlines, outcomes, expected digest
└── report.rs         # percentiles, CVaR, validation summary
```

Still one crate: the three binaries share `config`, `dist`, and the wire types,
and splitting into a workspace now means cross-crate refactoring while those
interfaces are still moving.

**The one boundary that must be real immediately.** `oracle.rs` and `record.rs`
must not be reachable from `service.rs`. In a single crate that is a convention,
not a guarantee — so when `service/` appears at step 4 it becomes its own crate
depending only on a narrow `tailbench-api` crate (wire types and the
`Downstream` trait). Everything else stays put. This is the Phase 1 spec's §6
"model may only change the service crate" rule made structural.

`report.rs` runs offline over the JSONL and needs no container.

### 1.4 Deployment

`docker/` holds a compose file and one Dockerfile per component. Compose defines
the cpusets from §1.2, the shared volume for the UDS and the log, and
`worker_threads` per container.

```yaml
services:
  mocks:
    cpuset: "6-9"
    environment: { TOKIO_WORKER_THREADS: 4 }
    volumes: [ "sock:/run/tailbench", "./out:/out" ]
  service:
    cpuset: "2-5"
    environment: { TOKIO_WORKER_THREADS: 4 }
    volumes: [ "sock:/run/tailbench" ]
    depends_on: [ mocks ]
  loadgen:
    cpuset: "0-1"
    environment: { TOKIO_WORKER_THREADS: 2 }
    volumes: [ "sock:/run/tailbench", "./out:/out" ]
    depends_on: [ service ]
```

Requirements on the runner:

- **Pin the base image by digest, not tag.** A moving `:bookworm` changes libc
  and kernel-visible behaviour between admission runs and the eval that cites
  them, invalidating §7's numbers silently.
- **Startup barrier.** `depends_on` waits for container start, not readiness.
  The generator must wait for a readiness handshake from `service`, and
  `service` from `mocks`, or the first requests hit a cold or absent peer and
  poison the warmup.
- **Record the environment.** `run.json` captures image digests, cpuset
  assignment, `worker_threads`, kernel version, and the §1.2.1 `environment`
  field. Without these an admission number cannot be reproduced or trusted.
- **Fail closed on cpuset.** If a container's cpuset is absent or overlaps
  another's, abort rather than run — an unpinned run that looks pinned is the
  failure mode this whole section exists to prevent.

## 2. Dependencies

```toml
tokio      = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }
serde      = { version = "1", features = ["derive"] }
toml       = "0.8"
rand       = "0.8"
rand_chacha = "0.3"   # reproducible, portable, explicitly stable stream
rand_distr = "0.4"    # LogNormal, Pareto, Exp
clap       = { version = "4", features = ["derive"] }
serde_json = "1"       # per-request log as JSONL
anyhow     = "1"
bincode    = "1"       # UDS wire format; compact and fast to decode
hdrhistogram = "7"     # optional, see §9.3
```

`rand_chacha` rather than `StdRng`: `StdRng`'s algorithm is explicitly allowed to
change between `rand` releases. Reproducibility across time is the whole point,
so the generator must be pinned by name.

## 3. Configuration

Subset of the §2 schema — only the fields steps 1–2 consume. Unknown fields are
rejected (`#[serde(deny_unknown_fields)]`) so that a config written against the
full schema fails loudly rather than silently ignoring `[topology]` or
`[faults]`.

```toml
[scenario]
id         = "validate-lognormal-001"
seed       = 42
duration_s = 30
warmup_s   = 5

[load]
arrival   = "poisson"        # constant | poisson | bursty
rate_rps  = 300
burstiness_cv = 1.4          # bursty only

[slo]
budget_ms  = 120             # §6.1: per-request deadline
penalty_ms = 1200            # §6.5: default 10 x budget_ms

# §6.3: replaces the old inline `request_mix`, since a class now carries
# required work as well as a weight.
[[request_class]]
name     = "cheap"
weight   = 0.9
requires = ["svc_a"]

[[request_class]]
name     = "expensive"
weight   = 0.1
requires = ["svc_a", "svc_b"]

[[downstream]]
id           = "svc_a"
distribution = { kind = "lognormal", median_ms = 8, sigma = 0.6 }
capacity     = 64
timeout_ms   = 250
```

`[slo]` is no longer deferred: §6 makes the deadline part of the success
definition, so the budget must be known at generation time to stamp
`deadline_ns` on every request.

Still deferred, with a stated reason:

- `target_utilization` — needs a measured capacity number from an expert
  solution, which doesn't exist until step 4. `rate_rps` is required for now;
  make `target_utilization` an error, not a silent ignore.
- `[topology]`, `[faults]` — steps 4–6. Parse-and-reject.

### 3.1 Validation at load

Reject at parse time, with the offending value in the message:

- `warmup_s >= duration_s`
- `rate_rps <= 0`, or non-finite
- `request_class` weights not summing to 1.0 within `1e-9`, or any weight `<= 0`
- duplicate downstream `id`, or duplicate `request_class` name
- `capacity == 0`
- a `requires` entry naming a downstream that is not declared (§6.3)
- empty `requires` — a class that must do no work has no correctness oracle
- `budget_ms <= 0`, or `penalty_ms <= budget_ms` (§6.5: the penalty must be
  worse than the worst legitimate success, or failing beats being slow)
- distribution parameters outside their domain (`sigma <= 0`, `p_slow` outside
  `[0,1]`, `fast_ms > slow_ms`, Pareto `alpha <= 1` — see §4.2)
- `arrival = "bursty"` without `burstiness_cv`, or `burstiness_cv <= 1.0`
  (CV ≤ 1 is not bursty; Poisson is exactly CV = 1)

## 4. Latency distributions (`dist.rs`)

```rust
pub enum Distribution {
    Constant { ms: f64 },
    Uniform { min_ms: f64, max_ms: f64 },
    LogNormal { median_ms: f64, sigma: f64 },
    Bimodal { fast_ms: f64, slow_ms: f64, p_slow: f64 },
    Pareto { scale_ms: f64, alpha: f64 },
    Empirical { samples_ms: Vec<f64> },   // sorted at construction
}

impl Distribution {
    pub fn sample(&self, rng: &mut ChaCha8Rng) -> Duration;
    pub fn analytic_quantile(&self, q: f64) -> Option<f64>;
}
```

`analytic_quantile` is what makes step 1's validation possible: for Constant,
Uniform, LogNormal, Bimodal, and Pareto the true p99 is a closed form, so a
measured p99 can be checked against a known answer rather than against another
measurement. Returns `None` for `Empirical` (use the order statistic).

Closed forms, to be asserted in tests:

- Constant: `q -> ms`
- Uniform: `min + q*(max-min)`
- LogNormal: `median * exp(sigma * Φ⁻¹(q))` — parameterized by median so
  `mu = ln(median_ms)`
- Bimodal: `q < 1 - p_slow ? fast : slow`
- Pareto: `scale * (1-q)^(-1/alpha)`

### 4.1 Bimodal is the one that matters

§1.3 flags it as the important distribution because it models a cache miss or a
slow shard, and it's the one where mean and p99 diverge hardest. `p_slow = 0.02`
with `fast=3ms, slow=180ms` gives mean ≈ 6.5ms and p99 = 180ms — a 28× gap. That
gap is the headline effect the whole project is hunting, so bimodal gets the most
test attention: sampled fraction of slow draws within binomial CI of `p_slow`,
and p99 exactly `slow_ms` whenever `p_slow > 0.01`.

### 4.2 Pareto needs `alpha > 1`

For `alpha <= 1` the mean is infinite and any measured mean latency is
meaningless — it doesn't converge and grows with sample size. Reject at config
load rather than producing numbers that look fine and aren't. Note that `alpha <=
2` gives infinite variance, which is legitimate but makes replay std. dev.
(the denominator in §7's `signal/noise`) unstable; document it, don't reject it.

## 5. Determinism model

**Rule: a request's downstream latency draw must not depend on what else the
service is doing.**

### 5.1 The two options

**Per-downstream stream.** Each downstream owns one `Mutex<ChaCha8Rng>` and draws
on arrival. Simple to describe. Draw *k* from the stream goes to whichever
request acquires the lock first, so the *assignment* of draws to requests is
scheduler-dependent.

**Per-call-site derivation.** Each call constructs a fresh RNG from
`(seed, request_id, downstream_id, attempt)` and draws once. Stateless, no lock.

```rust
fn call_rng(seed: u64, request_id: u64, downstream_id: u16, attempt: u32) -> ChaCha8Rng {
    let mut z = seed
        ^ request_id.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (downstream_id as u64) << 32
        ^ (attempt as u64) << 48;
    // splitmix64 finalizer
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ChaCha8Rng::seed_from_u64(z ^ (z >> 31))
}
```

The mixer must be explicit and documented, not `DefaultHasher` —
`DefaultHasher`'s output is not stable across Rust releases, and cross-version
reproducibility is the point.

### 5.2 What the difference actually costs

Under per-downstream streams the *multiset* of draws is identical across
replays — the same fraction of calls are slow, just attached to different
requests. **For a homogeneous workload with one call per request and no
saturation, p99 is largely insensitive to that permutation.** Do not claim
otherwise without measuring; the initial version of this spec did, and it was
unsupported.

The permutation stops being harmless in three cases, all of which tailbench
hits:

1. **Heterogeneous request classes.** With `request_mix` at 0.9 cheap / 0.1
   expensive and differing fan-out per class, which class absorbs the slow draws
   changes the latency distribution, not just its labelling.
2. **Saturation.** At capacity, a slow draw holds a permit and delays everything
   queued behind it. Which request receives it changes what is in flight, and
   the effect propagates. This is P3, one of the two step-4 primitives.
3. **Retries and hedging (P5).** Draw consumption becomes service-dependent: a
   service that retries pulls extra draws and shifts the stream for every later
   call. The service under test would be perturbing its own latency oracle, and
   naive-vs-expert would compare against different draw sequences.

Case 3 is a correctness problem rather than a noise problem, and it is the one
that cannot be papered over with more replays.

### 5.3 Decision

Use per-call-site derivation. The justification is **order-independence as a
structural guarantee**, not a measured noise reduction — a request's latency
oracle cannot be perturbed by concurrent activity, which keeps §7's
naive-vs-expert comparison meaningful once P3 and P5 exist.

Note that it is also the simpler implementation: eight stateless lines against a
per-downstream `Mutex` contended on every call, on the hot path of the thing
being measured. The cost is one ChaCha seeding per call (~100ns) instead of one
draw from a live stream; benchmark it in §10.5 rather than assuming it is free.

`attempt` is in the key from the start so P5's retries do not collide with the
original call. Adding it later means rewriting every recorded draw.

### 5.4 Falsifying this

§10.5 includes an A/B: implement both, run the same scenario N times under each,
and compare replay std. dev. of `cvar_99`. Expected result is little difference on a
homogeneous unsaturated scenario and a growing gap under the §5.2 cases. If the
gap never materializes even under saturation, the structural argument still
holds for case 3 but the noise argument should be struck from this spec rather
than left as folklore.

### 5.5 Arrival stream

The arrival timeline gets its own independent stream seeded from
`(seed, "arrival")`, generated fully in advance (§6.1), so it is unaffected by
anything the service does.

### 5.6 The Clock trait

```rust
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn sleep_until(&self, deadline: Instant) -> impl Future<Output = ()> + Send;
}
```

`RealClock` wraps `tokio::time`. Grep for `Instant::now` in CI and fail if it
appears outside `clock.rs` — the trait is worthless if call sites bypass it.

## 6. Request boundaries — what counts as success

The environment is modelled on a trading system: **a response that arrives after
its deadline has no value.** Not "less value" — none. This is the one property
borrowed from that domain, and it is borrowed deliberately; see §6.6 for what is
*not* being borrowed and why.

### 6.1 The contract

Every request carries a deadline stamped by the generator:

```
deadline = intended_dispatch + slo.budget_ms
```

Stamped from *intended* dispatch, not actual, for the same reason latency is
measured from intended dispatch (§7.2): otherwise generator lag silently grants
the service extra time.

A request is `Ok` if and only if all three hold:

1. It completed at or before its deadline.
2. It performed the work its class requires (§6.3).
3. Its response digest matches the oracle (§6.4).

Everything else is a failure. There is no partial credit in v1.

### 6.2 Outcome taxonomy

```rust
pub enum Outcome {
    Ok,             // completed <= deadline, work done, digest matched
    Expired,        // completed > deadline, or never completed by deadline
    Incorrect,      // completed <= deadline, digest mismatched or work skipped
    Error,          // service returned an error
    Dropped,        // service refused/shed the request  -- GATE VIOLATION in v1
    NeverServed,    // no record produced by end of run
}
```

Note what changed from the earlier draft: `Timeout` is gone as a request-level
outcome. A downstream timeout is a property of a `CallSpan`, not of the request —
the request either still made its deadline (`Ok`) or it did not (`Expired`).
Having both `Timeout` and `Expired` at request level invites double-counting the
same event.

**`Expired` is a failure that stays in the percentile.** It is recorded with its
true completion time when one exists, and enters latency aggregation at a
penalty value (§6.5). A service cannot improve p99 by letting requests expire.

**`Dropped` is a gate violation in v1.** Shedding is deferred to v2 (§6.7); until
then a service that refuses a request fails the run outright, preserving the
anti-shed rule from the Phase 1 spec's §6 gate table.

### 6.3 Required work

Each request class declares the downstream calls it must make:

```toml
[[request_class]]
name     = "expensive"
weight   = 0.1
requires = ["svc_a", "svc_b", "svc_c"]

[[request_class]]
name     = "cheap"
weight   = 0.9
requires = ["svc_a"]
```

A request whose spans do not include at least one successful call to each
required downstream is `Incorrect`, even if it returned on time with a plausible
digest. This is what blocks "skip the slow downstream" as a strategy.

Deliberately a *minimum*, not an exact set. The service may call a downstream
more than once (retries, hedging — P5's fix family) without penalty. Constraining
the exact call multiset would forbid the correct fix for one of the six
primitives.

Ordering between calls is **not** constrained. Requiring a specific order would
forbid P4's fix (parallelising a serialized fan-out), which is the entire point
of that primitive.

### 6.4 Correctness oracle

The digest is a pure function of the request payload and the values returned by
the required downstream calls:

```
digest = mix(payload.nonce, [call_digest(svc) for svc in requires])
```

`call_digest` is deterministic from `(seed, request_id, downstream_id)` — the
same derivation as §5 — so the harness can compute the expected digest offline
without the service's cooperation.

This gives the properties the Phase 1 spec's hack table asks for:

- **Cross-request caching** fails, because `payload.nonce` is unique per
  `request_id` (§7.4).
- **Fabricating a response** fails, because the digest depends on values only
  obtainable by actually calling the downstream.
- **Returning early with a partial answer** fails, because a missing call's
  digest is missing from the mix.

The open question from the Phase 1 spec's §12 — "is a digest enough, or is
semantic checking needed?" — resolves to *digest is enough*, conditional on the
response being fully determined by payload plus downstream results. Hold that
condition when authoring P4 and P5.

### 6.5 How failures enter the metric

A percentile over only-successful requests is trivially gamed: fail the slow
ones and the tail disappears. So every scheduled request contributes a latency
value:

| Outcome | Latency contributed |
|---|---|
| `Ok` | `completion − intended_dispatch` |
| `Expired` | `PENALTY_MS` |
| `Incorrect` | `PENALTY_MS` |
| `Error` | `PENALTY_MS` |
| `Dropped` | `PENALTY_MS` (and the run fails a gate) |
| `NeverServed` | `PENALTY_MS` |

`PENALTY_MS` defaults to `10 × budget_ms` and is recorded in the run manifest.

The multiplier sets an **exchange rate**: how many milliseconds of added latency
one failure is worth. It has a real failure mode in both directions. Too small
and failing beats being slow — a service facing a 400ms request abandons it and
scores the penalty instead, so the environment rewards giving up. (§3.1 rejects
`penalty_ms <= budget_ms` for this reason: below that, quitting strictly
dominates any late success.) Too large and the metric stops measuring latency at
all and reports only whether the failure rate crossed a threshold.

10× is a starting point to be checked in §10.6, not a result.

### 6.5.1 Why the primary metric is CVaR, not p99

The exchange rate above only means something if the metric responds to it
proportionally, and **p99 does not**. p99 is a single order statistic: with
10,000 requests it is the 100th-worst value. Below 1% failures, every failure
sorts above position 100 and p99 is completely unaffected by the penalty value.
Above 1%, position 100 *is* a failure and p99 equals `PENALTY_MS` exactly. The
multiplier is invisible in one regime and total in the other — a step function
with no informative gradient anywhere.

CVaR@99 — the mean of the worst 1% — does not have this shape. Every failure
contributes its full penalty to that average, proportionally: ten failures raise
CVaR by a measurable amount, a hundred raise it by ten times as much. The
exchange rate applies smoothly across the whole range.

So, following the Phase 1 spec's §5: **optimize CVaR@99, report p99 for
interpretability, emit both from day one.** Everywhere this spec gates or tunes
on a tail statistic — the §10.6 penalty sweep, the §10.4 replay tolerance, and
later the §7 `signal/noise ≥ 5` admission bar — the statistic is CVaR.

The admission filter is the strongest reason. `signal/noise` divides by replay
standard deviation, and CVaR averages ~90 values where p99 reads one, so its
replay variance should be materially lower. Lower noise admits more candidate
tasks, which matters directly given the Phase 1 spec expects most generated
tasks to be discarded.

**Verify rather than assume.** CVaR is the mean of the extreme tail, so it is
more sensitive to the far upper tail than p99 is. Under Pareto with
`alpha <= 2` — infinite variance, which §4.2 permits — CVaR's replay variance
may be *worse*. §10.4 reports replay std. dev. for both statistics; if CVaR
loses on some distribution, that is a finding to record, not to hide.

### 6.5.2 Two conventions that must be fixed

Both change the number, so both are written down rather than left to the
implementation:

1. **Tail size at the boundary.** With 10,000 samples the worst 1% is exactly
   100 values; with 9,437 it is 94.37. Use `k = ceil(n * 0.01)` and take the
   mean of the `k` largest. Same requirement as the percentile interpolation
   method in §9.3 — different conventions give different answers on identical
   data, and §0's reproducibility claim needs one stated.
2. **CVaR is computed over the penalised population**, exactly like p99 —
   failures enter at `PENALTY_MS` per the table above. Note the consequence:
   CVaR@99 *is* the worst 1%, so once failures exceed 1% of requests, CVaR
   saturates at `PENALTY_MS` just as p99 does. CVaR degrades more gracefully
   *below* that threshold, not above it. Above 1% failures, neither statistic
   carries latency information and the outcome rates are the only signal.

**The multiplier must be frozen across the task set.** It is a scenario
parameter, so scenarios using different values produce incomparable CVaR and
p99 numbers — and incomparable `signal/noise` ratios in §7. Pick one value from
the §10.6 sweep and freeze it, the same treatment §7 gives its threshold.

### 6.5.3 Outcome rates are not optional

Report `expiry_rate`, `incorrect_rate`, and `error_rate` **separately and
always**. Any tail statistic is uninterpretable without them, since a run can
post an excellent p99 by failing 0.9% of requests — and per §6.5.2, once
failures exceed 1% the tail statistics carry no latency information at all.

### 6.6 What is not borrowed from the trading domain

The deadline is taken. These are not:

- **Microsecond budgets.** Real trading paths run single-digit microseconds with
  kernel bypass and busy-polling. Tokio's scheduler jitter alone exceeds that
  budget, so the six fix primitives — `spawn_blocking`, `join_all`, semaphores —
  would be meaningless. Budgets here stay in the tens-to-hundreds of ms.
- **Jitter as the primary metric.** Trading systems optimise variance around a
  mean. This project optimises a tail statistic. Related, not the same.

The framing is "deadline-driven service under bursty load", which is real and
motivates the design. It is not a claim to model an HFT system, and the writeup
should not make one — see the Phase 1 spec's §11 on overclaiming.

### 6.7 Deferred to v2: shedding and value-weighted reward

The intended end state is richer than v1: dropping a doomed request is
*sometimes* correct, and requests arriving during a load spike are worth more.
Both are deferred, with reasons.

**Why not now.** The §7 admission filter needs `signal/noise ≥ 5`, and the noise
floor is currently unmeasured — §10.4 produces that number for the first time. A
load-dependent value function makes the metric strictly noisier, because jitter
in burst timing feeds directly into the score. Adding variance to a metric whose
variance is uncharacterised means being unable to tell a bad reward function from
a bad measurement.

There is also a design risk that is cheaper to find with a simple metric: a
load-dependent reward is a game, and games have degenerate strategies. Detecting
burst onset and shedding hard may score well without being good engineering.
Diagnosing that requires a plain-p99 baseline to compare rank ordering against.

**The conflict that must be resolved before v2 ships.** The Phase 1 spec's §6
makes dropping a failure *specifically* so p99 cannot be flattened by discarding
the tail. v2 wants dropping to be sometimes correct. Both cannot be
unconditionally true. Proposed resolution, to be tested rather than assumed:

> A dropped request scores **identically to an expired one**. Never free — so
> shedding never directly improves the score — but it returns capacity that a
> doomed request would have consumed. Shedding becomes rational exactly when
> capacity is genuinely short, and never as a way to hide latency.

**Observability constraint.** Whatever the v2 value function depends on must be
derivable by the service from state it can observe at decision time. If value
depends on information available only to the harness, the task tests whether the
model guessed the scenario, not whether it engineered anything.

### 6.8 Forward compatibility — the part that is expensive to retrofit

v2 must be a **scorer change, not a re-run**. Every admission result and every
recorded log stays valid only if v1 logs already contain what a value-weighted
score needs. Three fields, recorded from day one:

```rust
pub deadline_ns: u64,          // intended_dispatch + budget_ms
pub expired: bool,             // completion > deadline (or never completed)
pub offered_load_rps: f64,     // instantaneous arrival rate at intended dispatch
```

`offered_load_rps` is computed from the precomputed timeline (§7.1) over a
sliding window, so it costs nothing at runtime and is identical across replays.
It is the input a burst-aware value function needs, and it is unrecoverable
after the fact if the timeline was not retained.

With these, v2's value function can be developed and tuned **offline against v1
logs**, compared against p99 on the same runs, and required to clear the same
`signal/noise ≥ 5` bar before it is allowed to replace p99. That is the cheap
version of the experiment.

## 7. Load generator (`harness.rs`)

### 7.1 Timeline precomputed before the run starts

Generate the entire arrival schedule up front, as a `Vec<ScheduledRequest>`:

```rust
pub struct ScheduledRequest {
    pub request_id: u64,
    pub offset: Duration,     // from run start; the intended dispatch time
    pub class: RequestClass,
    pub payload: Payload,     // unique by construction (§6.4)
}
```

Precomputing rather than sampling inter-arrival gaps as the run proceeds has
three benefits: the timeline is trivially inspectable and testable without
running anything; the generator does zero RNG work on the hot path; and the
schedule is provably independent of service behaviour, which is exactly the
open-loop property §1.1 makes non-negotiable.

Memory is a non-issue: 30s × 300rps ≈ 9,000 entries.

Arrival processes:

- `constant`: `offset[i] = i / rate`
- `poisson`: gaps ~ `Exp(rate)`, cumulative sum
- `bursty`: gaps ~ Gamma with the shape solved to hit `burstiness_cv`
  (`shape = 1/cv²`, `scale = 1/(rate*shape)`), giving mean rate `rate_rps` with
  the requested CV. Poisson is the `cv = 1` special case, which is a useful
  self-consistency test.

### 7.2 Dispatch loop

```
start = clock.now()
for req in timeline:
    deadline = start + req.offset
    if clock.now() > deadline + LATE_DISPATCH_THRESHOLD:
        record CoordinatedOmission { request_id, lateness }
    clock.sleep_until(deadline).await
    actual = clock.now()
    spawn(async move { target.handle(req).await })   // never awaited inline
```

Two things this must get right:

**Spawn, never await inline.** Awaiting the handler inside the loop is a
closed-loop generator wearing an open-loop costume — the exact failure §1.1
names. The spawn is the property; it belongs in a test (§10.2), not just a
comment.

**Latency measured from `deadline`, not `actual`.** `e2e = completion -
intended_dispatch`. This is what makes coordinated omission visible instead of
invisible: if the generator stalls, the requests it dispatches late carry that
lateness in their measured latency.

### 7.3 Coordinated omission is a run failure

§1.1: late dispatch "must record coordinated-omission events and fail the run
rather than silently pace itself."

- `LATE_DISPATCH_THRESHOLD`: 1ms default, configurable.
- Every late dispatch is recorded with its lateness.
- After the run, if late dispatches exceed `max_late_dispatch_frac` (default
  0.001) **of post-warmup requests**, the run is marked `Failed`, not `Slow`.

That default is a guess, not a measured number. Calibrate it during step 1
validation: run the synthetic target at several rates, find the rate at which
the generator itself starts falling behind, and set the threshold with real
headroom below it. Record the calibration in the validation report — a threshold
nobody measured is a threshold that silently passes bad runs.

The generator's own capacity is a real ceiling. At high rates a
sleep-then-spawn loop on one task will not keep up, and the failure mode is
silently attributing generator lag to the service. Measure it explicitly in §10.3
and state the maximum trustworthy rate in the README.

### 7.4 Payload uniqueness

§6 of the Phase 1 spec blocks cross-request caching by making payloads unique by
construction. Cheapest form that actually works: embed `request_id` in the
payload and make the correctness oracle's expected digest a function of it. A
service that caches across requests then returns a digest for the wrong
`request_id` and fails the oracle.

Full oracle is step 6. What's required now is only that payloads are unique and
digestible — so the field exists and is populated, and step 6 doesn't need to
change the record format.

## 8. Mock downstream cluster (`mocks.rs`)

```rust
pub trait Downstream: Send + Sync {
    async fn call(&self, ctx: CallCtx) -> Result<CallOutcome, CallError>;
}

pub struct CallCtx { pub request_id: u64, pub attempt: u32 }

pub enum CallOutcome { Ok { digest: u64 }, Timeout, Error }
```

Per-call sequence:

1. Acquire a semaphore permit sized to `capacity`. **Time spent waiting here is
   queueing delay and is part of the measured call span** — it's how a downstream
   saturates, which §1.3 requires.
2. Draw service time from the distribution via `call_rng(...)` (§5).
3. `clock.sleep_until(now + service_time)`.
4. Apply `timeout_ms` against total elapsed (queue + service), yielding `Timeout`.
5. Release permit, emit a `CallSpan`.

Note step 4 applies the timeout to queue + service, not service alone — a
request that spent 240ms queued and 20ms being served has waited 260ms and must
time out. Timing out only on service time would make a saturated downstream look
healthy, which is the same class of error as closed-loop load generation.

Correlated slow modes (§1.3's "sick downstream") are **deferred**. They are
config-visible in the full schema but not needed to validate steps 1–2, and they
add a wall-clock-dependent axis that wants care. Reject the config key for now.

## 9. Metrics and logging

### 9.1 Per-request records, aggregate offline

The Phase 1 spec's §1.4 is explicit: log everything, aggregate offline. JSONL, one object per
request.

```rust
pub struct RequestRecord {
    pub request_id: u64,
    pub class: RequestClass,
    pub intended_dispatch_ns: u64,   // all times: ns since run start
    pub actual_dispatch_ns: u64,
    pub deadline_ns: u64,            // §6.1: intended_dispatch + budget_ms
    pub first_byte_ns: Option<u64>,
    pub completion_ns: Option<u64>,
    pub outcome: Outcome,            // §6.2
    pub expired: bool,               // §6.8: completion > deadline, or never
    pub offered_load_rps: f64,       // §6.8: arrival rate at intended dispatch
    pub response_digest: Option<u64>,
    pub digest_ok: Option<bool>,     // vs oracle; None if never completed
    pub required_calls_met: bool,    // §6.3
    pub spans: Vec<CallSpan>,
    pub late_dispatch_ns: u64,       // 0 if on time
}

pub struct CallSpan {
    pub downstream_id: String,
    pub attempt: u32,
    pub queue_wait_ns: u64,
    pub service_ns: u64,
    pub outcome: CallOutcome,
}
```

Times as `u64` ns from run start rather than timestamps: smaller, exactly
comparable, and no timezone or monotonic-vs-wall ambiguity in the log.

**`NeverServed` must be written.** A request that was scheduled and never
completed produces a record with `completion_ns: None` — because §6's gate table
requires unserved requests to count as failures and never be excluded from the
percentile. If unserved requests simply produce no line, the most important hack
in the table becomes invisible in the log format itself. At end of run, walk the
timeline and emit a `NeverServed` record for every `request_id` without one.

### 9.2 Writing without perturbing the measurement

The recorder must not add latency to the thing it measures. Handlers send
records over a bounded `mpsc` to a single writer task that buffers and writes.
Bounded, not unbounded — an unbounded channel here is literally fault primitive
P2 living in the harness. Size it generously (e.g. 64k); if it ever fills, that's
a harness bug and the run fails rather than blocking a handler.

Serialize on the writer task, not the handler.

### 9.3 Aggregation (`report.rs`)

Offline pass over the JSONL. Discard records whose `intended_dispatch_ns <
warmup_s`. Discarding on *intended* rather than actual dispatch keeps the warmup
boundary independent of service behaviour.

Every scheduled post-warmup request contributes a latency value, with failures
entering at `PENALTY_MS` per the §6.5 table. A percentile computed over only
successful requests would be trivially gamed and must never be emitted.

**`cvar_99` is the primary metric** (§6.5.1); p99 is reported alongside for
interpretability. Compute CVaR as the mean of the `ceil(n * 0.01)` largest
values in the penalised population (§6.5.2).

Emit, per §5 of the Phase 1 spec: p50, p90, p99, p99.9, max, mean, throughput,
downstream call counts. Also emit `cvar_999` — cheap, and it tells you whether
CVaR@99 is being driven by a handful of extreme values.

Emit the §6.2 outcome breakdown alongside, always, never optionally:
`ok_rate`, `expiry_rate`, `incorrect_rate`, `error_rate`, `dropped_count`,
`never_served_count`. A p99 without these is uninterpretable — a run can post an
excellent p99 by failing 0.9% of requests.

Also emit **`p99_ok_only`** and **`cvar_99_ok_only`**, over successful requests
alone, clearly labelled as diagnostic and never as headline metrics. The gap
between these and their penalised counterparts is the fastest way to see whether
a service is winning on latency or on attrition.

Percentiles from the sorted exact sample, not HDR histogram buckets. At ~9k
requests per run, sorting is free and exact, and HDR's relative-precision buckets
introduce quantization on the exact statistics whose replay std. dev. §7 needs
to measure. Keep `hdrhistogram` out unless run sizes grow enough to need it.

State the interpolation method for percentiles explicitly (nearest-rank, or
linear) — different conventions give different p99s on the same data, and §0's
reproducibility claim needs one written down. Same for the CVaR tail size, per
§6.5.2.

`Failed` runs report their metrics but are marked failed. The distinction
matters: the Phase 1 spec's §1.5 requires a gate failure to score as failure, not as a slow run, and
that requires the metrics to still be visible for debugging.

## 10. Validation — how step 1 is proven correct

Step 1's deliverable is "validated against a synthetic service with known
analytic latency." Concretely:

### 10.1 Synthetic target with known answer

`target.rs` provides a target that sleeps for a draw from a configured
`Distribution` and returns — no downstreams, no concurrency limits, unbounded
parallelism.

Under this target with unbounded concurrency, queueing is zero, so measured e2e
latency should converge to the distribution itself. Test: measured p50/p90/p99
within tolerance of `analytic_quantile`. This validates the timeline, the
dispatch loop, the measurement points, and the aggregation as one unit — if any
of them is wrong, the numbers won't match a closed form.

Tolerance must account for both sampling error at ~9k samples and real-clock
sleep overshoot (tokio timer granularity is ~1ms, which is a large fraction of a
3ms `fast_ms`). Expect a small positive bias in every measured quantile; state
it rather than tuning the tolerance until it passes.

### 10.2 The open-loop property, as a test

The most valuable test in this spec. A target whose latency far exceeds the
inter-arrival gap (e.g. 5s handler, 300rps) must still receive requests at
300rps. Assert `actual_dispatch` tracks `intended_dispatch` throughout, and that
the number dispatched matches the timeline.

A closed-loop implementation fails this immediately. Given that §1.1 calls this
choice determinative for the whole project, it should be impossible to regress
silently.

### 10.3 Generator capacity calibration

Sweep rate against a near-zero-latency target and find where late dispatches
appear. Output the max trustworthy rate. This is a report, not a pass/fail test —
but it's the number that tells you whether a future measurement is real.

### 10.4 Reproducibility

Same seed, same config, two runs → per-request latencies identical *for the
mock draws*, and aggregate metrics within a stated tolerance. Note the honest
limit: under real time and a real scheduler, e2e latencies will not be
bit-identical. What's exactly reproducible is the *downstream latency draws*
(§5); what's approximately reproducible is the measured tail statistics. §0's
criterion 2 asks for a stated tolerance, so measure it here and state it — this
test produces the number that goes in the README.

**Report replay std. dev. for both `cvar_99` and `p99`, across every
distribution in §4.** §6.5.1 expects CVaR to be the lower-variance statistic and
therefore the better denominator for §7's `signal/noise`, but that is a
prediction. The Pareto `alpha <= 2` case is where it is most likely to fail,
since CVaR averages the extreme tail that heavy tails make unstable. If CVaR
loses anywhere, record it — the primary-metric choice in §6.5.1 is supposed to
be made on this data, not asserted ahead of it.

### 10.5 Distribution and RNG tests

Per distribution: sampled quantiles converge to `analytic_quantile` over a large
sample; same seed → identical sequence; different `request_id` → different draws.

Order-invariance (§5's structural claim): drawing the same set of
`(request_id, downstream_id)` pairs concurrently and sequentially produces
identical results.

Two measurements rather than assertions:

- **Cost of per-call seeding.** Benchmark `call_rng` + one draw against a draw
  from a live stream. §5.3 assumes ~100ns and negligible at these rates; confirm.
- **The §5.4 A/B.** Both RNG strategies, same scenario, N replays each, compare
  replay std. dev. of `cvar_99` — on a homogeneous unsaturated scenario and on a
  saturated one. Record both numbers in the validation report. If the saturated
  case shows no gap, strike the noise argument from §5.2 and keep only the
  case-3 correctness argument.

### 10.6 Boundary semantics and penalty calibration

Tests for §6, since the success definition is now load-bearing:

- **Deadline is stamped from intended dispatch.** Delay the generator
  artificially; assert `deadline_ns` is unchanged. A service must not gain time
  from harness lag.
- **Expiry is not a free exit.** A target that abandons requests just past the
  deadline must score *worse* than one that completes them slowly. This is the
  property §6.5 exists to guarantee; assert it directly rather than trusting the
  table.
- **Skipped work is caught.** A target that omits a required downstream call but
  returns on time is `Incorrect`.
- **Fabricated digest is caught.** A target returning a plausible digest without
  calling downstreams is `Incorrect`.
- **Cross-request cache is caught.** A target that memoizes by class rather than
  by payload produces digest mismatches.
- **Extra calls are permitted.** A target that calls a required downstream twice
  is still `Ok` — §6.3 specifies a minimum, and P5's fix depends on it.
- **Parallel and sequential both pass.** Same required calls in either order are
  both `Ok`, since constraining order would forbid P4's fix.

**Penalty calibration.** `PENALTY_MS = 10 × budget_ms` is a starting guess
(§6.5). Sweep the multiplier over runs at several known failure rates — below,
at, and above 1% — and plot both `cvar_99` and `p99` against it.

The expected shape, per §6.5.1: p99 is flat in the multiplier below 1% failures
and equals it above; CVaR responds proportionally throughout the sub-1% range.
Confirm that before trusting the choice — if CVaR also comes out flat, something
is wrong in the aggregation, not in the multiplier.

**Choose on CVaR**, since that is the optimization target. The question the
sweep answers is the exchange rate from §6.5: how many milliseconds of added
tail latency one failure should be worth. Record the chosen value, the sweep,
and the reasoning; then freeze it across the task set (§6.5.2).

### 10.7 Transport cost

§1.2 trades in-process calls for a process boundary and must quantify what that
costs, since the trade is only sound if the added variance is small relative to
the signal.

- **Baseline the transport alone.** Round-trip a null request over the UDS with
  no distribution sampling. Report p50, p99, p99.9 of the transport itself.
- **Differential run.** Same scenario, same seed, through the in-process
  `Downstream` impl and through the UDS impl. The delta in measured p99 and
  `cvar_99` is the transport's contribution. Keep the in-process impl alive as a
  test fixture for exactly this reason.
- **State it in the README.** A measured "UDS adds X ms at p99" is what makes
  later `signal/noise` figures interpretable.

If the transport's p99 contribution is large relative to plausible naive→expert
gaps, that is a step-4 kill-criteria input under the Phase 1 spec's §10 — raise
it then, not at step 8.

### 10.8 Isolation is doing its job

The §1.2 confound is the reason for the split, so test that it is actually gone:

- **Cross-talk test.** Run a fixed scenario against a synthetic service, then
  re-run with the service artificially spawning a large number of idle tasks.
  Measured latency should be unchanged. Under a shared runtime it will not be —
  which makes this test the direct evidence that §1.2's reasoning was correct
  and that the fix works.
- **Cpuset enforcement.** Assert each process's actual CPU affinity matches the
  compose file. A silently-unpinned run is the failure mode being guarded.
- **Generator punctuality under service load.** Late-dispatch count (§7.3) must
  not rise when the service is saturated. If it does, the generator is still
  coupled to the service and the isolation is incomplete.

## 11. CLI

```
tailbench run     --config <path> [--out <dir>] [--repeat N]
tailbench report  --log <path.jsonl>
tailbench validate --config <path>      # §10.1 analytic check
```

`run` writes `requests.jsonl`, `run.json` (config echo, seed, git SHA, host info,
timing), and a summary to stdout. `--repeat` runs N times and reports the replay
std. dev. of `cvar_99` and `p99` — the noise denominator §7 needs, available
from the start.

Exit non-zero on a failed run so it composes in scripts.

## 12. Build order within this spec

| # | Deliverable | Validated by |
|---|---|---|
| 1 | `clock.rs`, `config.rs`, `dist.rs` | §10.5 |
| 2 | `record.rs`, `report.rs` | unit tests on synthetic logs |
| 3 | `harness.rs` + synthetic target | §10.1, §10.2, §10.3 |
| 4 | `oracle.rs` — deadlines, outcomes, digest (§6) | §10.6 |
| 5 | `downstream.rs` — UDS transport, `mocks` binary | §10.5, §10.7 |
| 6 | `docker/` — compose, cpusets, env capture (§1.4) | §10.8 |
| 7 | CLI, `--repeat`, calibration report | §10.4, penalty sweep |

Steps 1–4 are Phase 1's step 1; step 5 is Phase 1's step 2. Steps 6–7 span both.

Build steps 1–4 against the in-process `Downstream` impl — it is faster to
iterate on and stays useful afterwards as the §10.7 differential fixture. The
process split lands at step 5, before any number is treated as authoritative.

`oracle.rs` joins the §1.3 module list: expected-digest computation, deadline
evaluation, and outcome classification. It is scorer-side, not harness-side —
the service under test must not be able to reach it.

## 13. Open questions

- **Generator capacity ceiling.** A single sleep-then-spawn task has a real max
  rate. §10.3 measures it; if it lands near the scenario rates in §2, the
  generator needs sharding across tasks, which complicates timeline ownership.
  Measure before designing for it.
- **Real-time noise floor.** §10.4 produces the number, for `cvar_99` and `p99`
  both. If replay std. dev. is large relative to plausible naive→expert gaps,
  the §4 determinism spike stops being an optimization and becomes required. This is the earliest cheap read on
  the kill criteria in §10 of the Phase 1 spec — worth looking at as soon as
  §10.4 runs, not at step 4.
- **Timer granularity vs `fast_ms`.** Bimodal with `fast_ms = 3` is near tokio's
  ~1ms timer resolution. If the measured fast mode is consistently biased, either
  raise the floor on `fast_ms` or use a hybrid spin/sleep. Quantify in §10.1
  before deciding.
- **Does CVaR actually beat p99 on replay variance?** §6.5.1 predicts yes and
  §10.4 measures it. The Pareto `alpha <= 2` case is where it plausibly loses.
  The primary-metric choice is meant to follow the data; if it does not hold,
  say so rather than keeping the claim.
- **Linux host for authoritative runs.** §1.2.1 — needed before step 4's
  admission runs. Bare metal is preferable to a cloud VM, where the hypervisor
  reintroduces the migration and timer jitter that pinning is meant to remove.
  If only a VM is available, §10.8's cross-talk test is the check on whether
  pinning survives it.
- **Transport tail.** §10.7 measures it. UDS is the choice on the expectation of
  a tighter tail than TCP; if the measured p99 contribution is large relative to
  plausible naive→expert gaps, the process split needs revisiting before more
  primitives are built on it.
- **Penalty multiplier.** 10× is a guess. §10.6 sweeps it; the risk in both
  directions is real (too small and failing beats being slow, too large and the
  metric flattens into a pass/fail step function), so this needs the measured
  answer before any task is admitted.
- **Whether `requires` over-constrains legitimate fixes.** §6.3 mandates a
  minimum call set. A fix family that legitimately *avoids* a downstream — a
  cache, a fast path, a fallback — would be scored `Incorrect`. Acceptable for
  v1 given the six primitives, all of which restructure *how* calls are made
  rather than *whether*. Revisit at step 5 if a primitive needs it.
- **v2 value function.** §6.7. Deferred until §10.4 gives a noise floor; §6.8's
  three log fields keep it a scorer change rather than a re-run. The unresolved
  design question is which observable signals a service may legitimately use to
  detect that shedding is currently correct.
- **`first_byte_ns` under an in-process trait.** With no streaming response
  there's no meaningful first-byte distinct from completion. The field is in the
  record format because the Phase 1 spec's §1.4 asks for it and adding it later
  would break log
  compatibility, but it will be `None` until there's a transport that makes it
  real. Flagging rather than silently emitting a duplicate of completion.
