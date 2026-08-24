//! The service under test.
//!
//! Receives requests from the load generator, calls the downstreams each
//! request requires, and returns a digest folded from their replies.
//!
//! This is the correct, fault-free implementation: every required call is made,
//! concurrently, with no artificial limit. The fault primitives are variations
//! on this file, and it is the only process a model under evaluation may edit.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixStream;

use tailbench::downstream::{span_of, CallCtx, UdsClient};
use tailbench::protocol::{ServiceReply, ServiceRequest};
use tailbench::ready;
use tailbench::rng::fold_digest;
use tailbench::wire::{read_msg, write_msg};

#[derive(Parser, Debug)]
#[command(about = "tailbench service under test")]
struct Args {
    /// Socket this service listens on, for the load generator.
    #[arg(long, default_value = "/run/tailbench/service.sock")]
    listen: PathBuf,
    /// Socket the mock downstream cluster is listening on.
    #[arg(long, default_value = "/run/tailbench/mocks.sock")]
    mocks: PathBuf,
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    ready::wait_for(&args.mocks).await?;
    let mocks = UdsClient::connect(args.mocks.to_str().unwrap()).await?;
    eprintln!("service: connected to mocks at {}", args.mocks.display());

    let listener = ready::bind(&args.listen)?;
    eprintln!("service: listening on {}", args.listen.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let mocks = mocks.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(stream, mocks, args.verbose).await {
                if !ready::is_disconnect(&e) {
                    eprintln!("service: connection error: {e}");
                }
            }
        });
    }
}

/// One load-generator connection.
///
/// Requests are handled concurrently and replies are written by a single
/// writer task. Handling inline would serialize every request behind the one
/// before it, which is a bottleneck the scenario never asked for.
async fn serve(stream: UnixStream, mocks: Arc<UdsClient>, verbose: bool) -> Result<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ServiceReply>(4096);

    let writer = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            if write_msg(&mut wr, &reply).await.is_err() {
                break;
            }
        }
    });

    let result = async {
        loop {
            let req: ServiceRequest = read_msg(&mut rd).await?;
            let mocks = mocks.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let reply = handle(req, &mocks, verbose).await;
                let _ = tx.send(reply).await;
            });
        }
    }
    .await;

    drop(tx);
    let _ = writer.await;
    result
}

async fn handle(req: ServiceRequest, mocks: &UdsClient, verbose: bool) -> ServiceReply {
    let ctx = CallCtx {
        request_id: req.request_id,
        attempt: 0,
    };

    // Every required downstream, concurrently. Sequential awaits here would be
    // fault primitive P4.
    let calls = req.required.iter().map(|name| async move {
        (name.clone(), mocks.call(name, ctx).await)
    });
    let results = futures::future::join_all(calls).await;

    let mut spans = Vec::with_capacity(results.len());
    let mut digests = Vec::with_capacity(results.len());
    for (name, reply) in results {
        match reply {
            Ok(reply) => {
                spans.push(span_of(&name, ctx, &reply));
                if let Some(d) = reply.digest {
                    digests.push(d);
                }
            }
            Err(e) => {
                return ServiceReply {
                    tag: req.tag,
                    digest: None,
                    spans,
                    error: Some(format!("{name}: {e}")),
                }
            }
        }
    }

    if verbose {
        eprintln!(
            "service: request {} -> {:?} ({} calls)",
            req.request_id,
            req.required,
            spans.len()
        );
    }

    ServiceReply {
        tag: req.tag,
        digest: Some(fold_digest(req.nonce, &mut digests)),
        spans,
        error: None,
    }
}
