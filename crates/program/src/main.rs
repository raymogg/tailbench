//! The program under test -- THE ONLY FILE AN AGENT MAY EDIT.
//!
//! Everything else in this repository is measurement apparatus: `loadgen`
//! schedules and scores requests, and `downstreams` simulates the dependencies.
//! Editing any of it changes the ruler rather than the thing being measured,
//! and the scores stop meaning anything.
//!
//! This crate depends on `tailbench-abi` -- the wire protocol, the downstream
//! client, and `fold_digest` -- and not on the harness. The scorer and the
//! seeded draws are therefore unreachable from here: `use
//! tailbench::oracle::Oracle` does not resolve, and `call_digest` is not in the
//! ABI. That is deliberate, and it is the reason the scores mean something.
//!
//! What this file does: receive a request from the load generator, call the
//! downstreams that request requires, fold their replies into a digest, and
//! return it before the deadline.
//!
//! What is fixed and cannot be optimized away:
//!   - Every downstream in `required` needs at least one *successful* call.
//!     The digest is folded from values only obtainable by actually calling
//!     them, so skipping work and fabricating an answer scores `Incorrect`.
//!   - A reply after `deadline` scores `Expired`, correct or not.
//!
//! What is open, and is the point of the exercise:
//!   - Fan-out strategy: concurrent, sequential, staged, prioritized.
//!   - Extra calls are legal. `required` is a *minimum*, so retries and
//!     hedging are permitted, and call order is unconstrained.
//!   - Timeouts, and what to do when one fires.
//!
//! This version is the correct, fault-free baseline: every required call made
//! concurrently, no artificial limit. The fault primitives are variations on
//! this one file.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixStream;

use tailbench_abi::call::{span_of, CallCtx, UdsClient};
use tailbench_abi::protocol::{ProgramReply, ProgramRequest};
use tailbench_abi::ready;
use tailbench_abi::digest::fold_digest;
use tailbench_abi::wire::{read_msg, write_msg};

#[derive(Parser, Debug)]
#[command(about = "tailbench program under test")]
struct Args {
    /// Socket this program listens on, for the load generator.
    #[arg(long, default_value = "/run/tailbench/program.sock")]
    listen: PathBuf,
    /// Socket the mock downstream cluster is listening on.
    #[arg(long, default_value = "/run/tailbench/downstreams.sock")]
    downstreams: PathBuf,
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    ready::wait_for(&args.downstreams).await?;
    let downstreams = UdsClient::connect(args.downstreams.to_str().unwrap()).await?;
    eprintln!("program: connected to downstreams at {}", args.downstreams.display());

    let listener = ready::bind(&args.listen)?;
    eprintln!("program: listening on {}", args.listen.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let downstreams = downstreams.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(stream, downstreams, args.verbose).await {
                if !ready::is_disconnect(&e) {
                    eprintln!("program: connection error: {e}");
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
async fn serve(stream: UnixStream, downstreams: Arc<UdsClient>, verbose: bool) -> Result<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ProgramReply>(4096);

    let writer = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            if write_msg(&mut wr, &reply).await.is_err() {
                break;
            }
        }
    });

    let result = async {
        loop {
            let req: ProgramRequest = read_msg(&mut rd).await?;
            let downstreams = downstreams.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let reply = handle(req, &downstreams, verbose).await;
                let _ = tx.send(reply).await;
            });
        }
    }
    .await;

    drop(tx);
    let _ = writer.await;
    result
}

async fn handle(req: ProgramRequest, downstreams: &UdsClient, verbose: bool) -> ProgramReply {
    let ctx = CallCtx {
        request_id: req.request_id,
        attempt: 0,
    };

    // Every required downstream, concurrently. Sequential awaits here would be
    // fault primitive P4.
    let calls = req.required.iter().map(|name| async move {
        (name.clone(), downstreams.call(name, ctx).await)
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
                return ProgramReply {
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
            "program: request {} -> {:?} ({} calls)",
            req.request_id,
            req.required,
            spans.len()
        );
    }

    ProgramReply {
        tag: req.tag,
        digest: Some(fold_digest(req.nonce, &mut digests)),
        spans,
        error: None,
    }
}
