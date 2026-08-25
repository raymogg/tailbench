//! Load generator's client for the program under test.
//!
//! Harness apparatus, not part of the code under test: this is how the
//! measurement talks to the program, so optimizing it would be tampering
//! with the instrument.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex};

use crate::protocol::{ProgramReply, ProgramRequest};
use crate::timeline::ScheduledRequest;
use crate::wire::{read_msg, write_msg};

/// One connection, multiplexed by tag: replies may arrive out of order, since
/// the program handles requests concurrently.
pub struct ProgramClient {
    writer: Mutex<tokio::io::WriteHalf<UnixStream>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ProgramReply>>>>,
    next_tag: AtomicU64,
}

impl ProgramClient {
    pub async fn connect(socket: &Path) -> Result<Arc<Self>> {
        let stream = UnixStream::connect(socket).await?;
        let (mut rd, wr) = tokio::io::split(stream);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ProgramReply>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let p = pending.clone();
        tokio::spawn(async move {
            while let Ok(reply) = read_msg::<_, ProgramReply>(&mut rd).await {
                if let Some(tx) = p.lock().await.remove(&reply.tag) {
                    let _ = tx.send(reply);
                }
            }
        });

        Ok(Arc::new(ProgramClient {
            writer: Mutex::new(wr),
            pending,
            next_tag: AtomicU64::new(0),
        }))
    }

    pub async fn call(&self, req: &ScheduledRequest) -> Result<ProgramReply> {
        let tag = self.next_tag.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(tag, tx);

        let msg = ProgramRequest {
            tag,
            request_id: req.request_id,
            required: req.required.clone(),
            nonce: req.nonce,
        };
        write_msg(&mut *self.writer.lock().await, &msg).await?;
        Ok(rx.await?)
    }
}
