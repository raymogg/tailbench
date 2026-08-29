//! The mock downstream cluster and the client that reaches it.
//!
//! `DownstreamCluster` runs inside the `downstreams` process. Everything else talks to it
//! through `UdsClient` over a Unix socket -- separate processes on disjoint
//! pinned cores, so the program under test is the only meaningful user of its
//! runtime. There is deliberately no in-process shortcut: a second mode would
//! produce numbers that are not comparable to the real one.

use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

use crate::clock::ms_to_duration;
use crate::config::{Config, DownstreamCfg};
use crate::record::CallOutcome;
use crate::rng::{call_digest, call_rng};
// The client half -- `CallCtx`, `CallReply`, `CallRequest`, `TaggedReply`,
// `UdsClient`, `span_of` -- moved to `tailbench-abi`. The program needs it and
// must not have the simulator below, which is what draws the seeded latencies.
pub use tailbench_abi::call::{span_of, CallCtx, CallReply, CallRequest, TaggedReply, UdsClient};

// In-process cluster
// ---------------------------------------------------------------------------

struct Slot {
    cfg: DownstreamCfg,
    index: u16,
    permits: Arc<Semaphore>,
}

/// The mock downstream cluster. Lives in the `downstreams` process; the program
/// under test reaches it only through `UdsClient`.
///
/// Deterministic given `(seed, request_id, downstream_id)` and independent of
/// wall clock and arrival order.
pub struct DownstreamCluster {
    slots: Vec<Slot>,
    seed: u64,
}

impl DownstreamCluster {
    pub fn new(cfg: &Config) -> Self {
        let slots = cfg
            .downstreams
            .iter()
            .enumerate()
            .map(|(i, d)| Slot {
                cfg: d.clone(),
                index: i as u16,
                permits: Arc::new(Semaphore::new(d.capacity)),
            })
            .collect();
        DownstreamCluster {
            slots,
            seed: cfg.scenario.seed,
        }
    }

    fn slot(&self, name: &str) -> Option<&Slot> {
        self.slots.iter().find(|s| s.cfg.id == name)
    }

    pub async fn call_inner(&self, name: &str, ctx: CallCtx) -> Result<CallReply> {
        let slot = match self.slot(name) {
            Some(s) => s,
            None => anyhow::bail!("unknown downstream {name:?}"),
        };
        let timeout = ms_to_duration(slot.cfg.timeout_ms);
        let start = Instant::now();

        // 1. Capacity permit. Waiting here is queueing delay and counts toward
        //    the call's latency -- it is how a downstream saturates.
        let permit = slot.permits.clone().acquire_owned().await?;
        let queued = Instant::now();
        let queue_wait = queued.saturating_duration_since(start);

        // The timeout applies to queue + service, not service alone: a request
        // that spent 240ms queued and 20ms served has waited 260ms. Timing out
        // only on service time would make a saturated downstream look healthy.
        if queue_wait >= timeout {
            drop(permit);
            return Ok(CallReply {
                digest: None,
                queue_wait_ns: queue_wait.as_nanos() as u64,
                service_ns: 0,
                outcome: CallOutcome::Timeout,
            });
        }

        // 2. Service time, from a per-call-site RNG.
        let mut rng = call_rng(self.seed, ctx.request_id, slot.index, ctx.attempt);
        let service = slot.cfg.distribution.sample(&mut rng);

        let remaining = timeout - queue_wait;
        let (served, outcome) = if service >= remaining {
            (remaining, CallOutcome::Timeout)
        } else {
            (service, CallOutcome::Ok)
        };

        tokio::time::sleep_until((queued + served).into()).await;
        drop(permit);

        let digest = match outcome {
            CallOutcome::Ok => Some(call_digest(self.seed, ctx.request_id, slot.index)),
            _ => None,
        };
        Ok(CallReply {
            digest,
            queue_wait_ns: queue_wait.as_nanos() as u64,
            service_ns: served.as_nanos() as u64,
            outcome,
        })
    }
}

// ---------------------------------------------------------------------------
// Transport probe
// ---------------------------------------------------------------------------
//
// `UdsClient` itself now lives in `tailbench-abi::call` -- the program needs it.
// UDS over TCP for a tighter tail: no TCP stack, no Nagle, no port exhaustion.

/// Round-trip latency of the transport itself, for the baseline.
pub async fn transport_probe(
    d: &UdsClient,
    name: &str,
    n: usize,
) -> Result<Vec<u64>> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t0 = Instant::now();
        let _ = d
            .call(
                name,
                CallCtx {
                    request_id: u64::MAX - i as u64,
                    attempt: 0,
                },
            )
            .await?;
        out.push(Instant::now().saturating_duration_since(t0).as_nanos() as u64);
    }
    Ok(out)
}
