//! Load generator's client for the service under test.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex};

use crate::protocol::{ServiceReply, ServiceRequest};
use crate::timeline::ScheduledRequest;
use crate::wire::{read_msg, write_msg};

/// One connection, multiplexed by tag: replies may arrive out of order, since
/// the service handles requests concurrently.
pub struct ServiceClient {
    writer: Mutex<tokio::io::WriteHalf<UnixStream>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServiceReply>>>>,
    next_tag: AtomicU64,
}

impl ServiceClient {
    pub async fn connect(socket: &Path) -> Result<Arc<Self>> {
        let stream = UnixStream::connect(socket).await?;
        let (mut rd, wr) = tokio::io::split(stream);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServiceReply>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let p = pending.clone();
        tokio::spawn(async move {
            while let Ok(reply) = read_msg::<_, ServiceReply>(&mut rd).await {
                if let Some(tx) = p.lock().await.remove(&reply.tag) {
                    let _ = tx.send(reply);
                }
            }
        });

        Ok(Arc::new(ServiceClient {
            writer: Mutex::new(wr),
            pending,
            next_tag: AtomicU64::new(0),
        }))
    }

    pub async fn call(&self, req: &ScheduledRequest) -> Result<ServiceReply> {
        let tag = self.next_tag.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(tag, tx);

        let msg = ServiceRequest {
            tag,
            request_id: req.request_id,
            required: req.required.clone(),
            nonce: req.nonce,
        };
        write_msg(&mut *self.writer.lock().await, &msg).await?;
        Ok(rx.await?)
    }
}
