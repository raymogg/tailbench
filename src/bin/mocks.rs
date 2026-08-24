//! Mock downstream cluster server (§1.2, container 2).
//!
//! Runs on its own pinned cores so its sleeps and timers do not share a tokio
//! runtime with the service under test.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use tailbench::clock::RealClock;
use tailbench::config::Config;
use tailbench::downstream::{CallRequest, InProcessCluster, TaggedReply};

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
    let cluster = Arc::new(InProcessCluster::new(&cfg, RealClock));

    if args.socket.exists() {
        std::fs::remove_file(&args.socket)?;
    }
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&args.socket)?;

    // Readiness marker: the service waits for this rather than for container
    // start, so the first requests do not hit an absent peer and poison the
    // warmup (§1.4).
    let ready = args.socket.with_extension("ready");
    std::fs::write(&ready, b"ok")?;

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
                if !is_disconnect(&e) {
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
async fn serve(stream: UnixStream, cluster: Arc<InProcessCluster<RealClock>>) -> Result<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4096);

    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&(bytes.len() as u32).to_le_bytes()).await.is_err()
                || wr.write_all(&bytes).await.is_err()
                || wr.flush().await.is_err()
            {
                break;
            }
        }
    });

    let result = async {
        loop {
            let mut len = [0u8; 4];
            rd.read_exact(&mut len).await?;
            let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
            rd.read_exact(&mut buf).await?;
            let req: CallRequest = bincode::deserialize(&buf)?;

            let cluster = cluster.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Ok(reply) = cluster.call_inner(&req.downstream, req.ctx).await {
                    let tagged = TaggedReply { tag: req.tag, reply };
                    if let Ok(bytes) = bincode::serialize(&tagged) {
                        let _ = tx.send(bytes).await;
                    }
                }
            });
        }
    }
    .await;

    drop(tx);
    let _ = writer.await;
    result
}

fn is_disconnect(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
        )
    })
}
