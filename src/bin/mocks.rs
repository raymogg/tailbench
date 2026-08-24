//! Mock downstream cluster server (container 2).
//!
//! Runs on its own pinned cores so its sleeps and timers do not share a tokio
//! runtime with the service under test.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixStream;

use tailbench::clock::RealClock;
use tailbench::config::Config;
use tailbench::downstream::{CallRequest, MockCluster, TaggedReply};
use tailbench::ready;
use tailbench::wire::{read_msg, write_msg};

#[derive(Parser, Debug)]
#[command(about = "tailbench mock downstream cluster")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long, default_value = "/run/tailbench/mocks.sock")]
    socket: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let cluster = Arc::new(MockCluster::new(&cfg, RealClock));

    let listener = ready::bind(&args.socket)?;

    eprintln!(
        "mocks: listening on {} ({} downstreams)",
        args.socket.display(),
        cfg.downstreams.len()
    );

    loop {
        let (stream, _) = listener.accept().await?;
        let cluster = cluster.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(stream, cluster).await {
                // Client disconnect at end of run is normal, not an error.
                if !ready::is_disconnect(&e) {
                    eprintln!("mocks: connection error: {e}");
                }
            }
        });
    }
}

/// One connection. Reads are sequential but *handling* is not: each request is
/// spawned and replies are written by a single writer task.
///
/// Handling inline would serialize every call on a connection, manufacturing a
/// bottleneck that is not in the scenario -- the same class of error as
/// closed-loop load generation.
async fn serve(stream: UnixStream, cluster: Arc<MockCluster<RealClock>>) -> Result<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<TaggedReply>(4096);

    let writer = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            if write_msg(&mut wr, &reply).await.is_err() {
                break;
            }
        }
    });

    let result = async {
        loop {
            let req: CallRequest = read_msg(&mut rd).await?;

            let cluster = cluster.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Ok(reply) = cluster.call_inner(&req.downstream, req.ctx).await {
                    let _ = tx.send(TaggedReply { tag: req.tag, reply }).await;
                }
            });
        }
    }
    .await;

    drop(tx);
    let _ = writer.await;
    result
}
