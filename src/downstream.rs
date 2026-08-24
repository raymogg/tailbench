//! The `Downstream` trait and its two impls (§1.2).
//!
//! `InProcess` is the development and §10.7-differential impl. `UdsClient`
//! talks to the `mocks` binary over a Unix socket, which is the configuration
//! authoritative runs use -- separate processes on disjoint pinned cores, so
//! the service under test is the only meaningful user of its runtime.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, Semaphore};

use crate::clock::{ms_to_duration, Clock};
use crate::config::{Config, DownstreamCfg};
use crate::record::{CallOutcome, CallSpan};
use crate::rng::{call_digest, call_rng};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CallCtx {
    pub request_id: u64,
    pub attempt: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallReply {
    pub digest: Option<u64>,
    pub queue_wait_ns: u64,
    pub service_ns: u64,
    pub outcome: CallOutcome,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallRequest {
    /// Correlation id. The server handles calls concurrently per connection, so
    /// replies can arrive out of order and must be matched, not assumed FIFO.
    pub tag: u64,
    pub downstream: String,
    pub ctx: CallCtx,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaggedReply {
    pub tag: u64,
    pub reply: CallReply,
}

pub trait Downstream: Send + Sync {
    fn call(
        &self,
        name: &str,
        ctx: CallCtx,
    ) -> impl std::future::Future<Output = Result<CallReply>> + Send;
}

// ---------------------------------------------------------------------------
// In-process cluster
// ---------------------------------------------------------------------------

struct Slot {
    cfg: DownstreamCfg,
    index: u16,
    permits: Arc<Semaphore>,
}

/// The mock cluster (§8). Deterministic given `(seed, request_id,
/// downstream_id)` and independent of wall clock and arrival order.
pub struct InProcessCluster<C: Clock> {
    slots: Vec<Slot>,
    seed: u64,
    clock: C,
}

impl<C: Clock> InProcessCluster<C> {
    pub fn new(cfg: &Config, clock: C) -> Self {
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
        InProcessCluster {
            slots,
            seed: cfg.scenario.seed,
            clock,
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
        let start = self.clock.now();

        // 1. Capacity permit. Waiting here is queueing delay and counts toward
        //    the call's latency -- it is how a downstream saturates.
        let permit = slot.permits.clone().acquire_owned().await?;
        let queued = self.clock.now();
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

        // 2. Service time, from a per-call-site RNG (§5.3).
        let mut rng = call_rng(self.seed, ctx.request_id, slot.index, ctx.attempt);
        let service = slot.cfg.distribution.sample(&mut rng);

        let remaining = timeout - queue_wait;
        let (served, outcome) = if service >= remaining {
            (remaining, CallOutcome::Timeout)
        } else {
            (service, CallOutcome::Ok)
        };

        self.clock.sleep_until(queued + served).await;
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

impl<C: Clock> Downstream for InProcessCluster<C> {
    async fn call(&self, name: &str, ctx: CallCtx) -> Result<CallReply> {
        self.call_inner(name, ctx).await
    }
}

// ---------------------------------------------------------------------------
// UDS client
// ---------------------------------------------------------------------------

/// Unix-socket client for the out-of-process mock cluster.
///
/// UDS over TCP for a tighter tail: no TCP stack, no Nagle, no port exhaustion.
/// §10.7 measures what the boundary costs.
///
/// One connection, multiplexed: a writer half behind a mutex and a single
/// reader task that dispatches replies to per-call oneshots by tag. A
/// connection pool would also work, but multiplexing keeps a slow call from
/// blocking an unrelated one on the same socket.
pub struct UdsClient {
    writer: Mutex<tokio::io::WriteHalf<UnixStream>>,
    pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<CallReply>>>>,
    next_tag: AtomicU64,
}

impl UdsClient {
    pub async fn connect(path: &str) -> Result<Arc<Self>> {
        let stream = UnixStream::connect(path).await?;
        let (mut rd, wr) = tokio::io::split(stream);
        let pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<CallReply>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let p = pending.clone();
        tokio::spawn(async move {
            loop {
                let mut len = [0u8; 4];
                if rd.read_exact(&mut len).await.is_err() {
                    break;
                }
                let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
                if rd.read_exact(&mut buf).await.is_err() {
                    break;
                }
                let Ok(tagged) = bincode::deserialize::<TaggedReply>(&buf) else {
                    break;
                };
                if let Some(tx) = p.lock().await.remove(&tagged.tag) {
                    let _ = tx.send(tagged.reply);
                }
            }
        });

        Ok(Arc::new(UdsClient {
            writer: Mutex::new(wr),
            pending,
            next_tag: AtomicU64::new(0),
        }))
    }
}

impl Downstream for UdsClient {
    async fn call(&self, name: &str, ctx: CallCtx) -> Result<CallReply> {
        let tag = self.next_tag.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(tag, tx);

        let req = CallRequest {
            tag,
            downstream: name.to_string(),
            ctx,
        };
        let bytes = bincode::serialize(&req)?;
        {
            let mut w = self.writer.lock().await;
            w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
            w.write_all(&bytes).await?;
            w.flush().await?;
        }
        Ok(rx.await?)
    }
}

/// Turn a reply into the span the record carries (§9.1).
pub fn span_of(name: &str, ctx: CallCtx, reply: &CallReply) -> CallSpan {
    CallSpan {
        downstream_id: name.to_string(),
        attempt: ctx.attempt,
        queue_wait_ns: reply.queue_wait_ns,
        service_ns: reply.service_ns,
        outcome: reply.outcome,
    }
}

/// Round-trip latency of the transport itself, for §10.7's baseline.
pub async fn transport_probe<D: Downstream, C: Clock>(
    d: &D,
    clock: &C,
    name: &str,
    n: usize,
) -> Result<Vec<u64>> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t0 = clock.now();
        let _ = d
            .call(
                name,
                CallCtx {
                    request_id: u64::MAX - i as u64,
                    attempt: 0,
                },
            )
            .await?;
        out.push(clock.now().saturating_duration_since(t0).as_nanos() as u64);
    }
    Ok(out)
}
