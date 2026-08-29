//! The program's client for the downstream cluster.
//!
//! The wire types and the multiplexing client, lifted verbatim from
//! `downstream.rs` -- bincode is positional, so `CallRequest` and `TaggedReply`
//! field order is load-bearing.
//!
//! `DownstreamCluster`, which *simulates* the downstreams, stays harness-side.
//! It is what draws the seeded latencies, and the program must not hold it.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::span::{CallOutcome, CallSpan};


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

// ---------------------------------------------------------------------------

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

    /// Issue one downstream call and wait for its reply.
    pub async fn call(&self, name: &str, ctx: CallCtx) -> Result<CallReply> {
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

/// Turn a reply into the span the record carries.
pub fn span_of(name: &str, ctx: CallCtx, reply: &CallReply) -> CallSpan {
    CallSpan {
        downstream_id: name.to_string(),
        attempt: ctx.attempt,
        queue_wait_ns: reply.queue_wait_ns,
        service_ns: reply.service_ns,
        outcome: reply.outcome,
    }
}
