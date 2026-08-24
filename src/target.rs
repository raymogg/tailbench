//! The interface the harness drives, plus synthetic targets used to
//! validate the harness itself.

use anyhow::Result;
use rand_chacha::ChaCha8Rng;
use std::sync::Arc;

use crate::clock::Clock;
use crate::downstream::{span_of, CallCtx, UdsClient};
use crate::dist::Distribution;
use crate::record::{CallSpan, Outcome};
use crate::rng::{call_rng, fold_digest};
use crate::timeline::ScheduledRequest;

/// What a target hands back for one request.
pub struct Response {
    pub digest: Option<u64>,
    pub spans: Vec<CallSpan>,
    /// Set only when the service itself failed or refused. Deadline evaluation
    /// is the harness's job, never the target's.
    pub failure: Option<Outcome>,
}

pub trait Target: Send + Sync + 'static {
    fn handle(
        &self,
        req: &ScheduledRequest,
    ) -> impl std::future::Future<Output = Result<Response>> + Send;
}

/// Calls every required downstream concurrently and folds the digests.
///
/// This is the "expert-shaped" reference behaviour: correct, unbounded
/// concurrency, no faults. Fault primitives (step 4+) are variations on it.
pub struct FanoutTarget {
    pub downstreams: Arc<UdsClient>,
    pub seed: u64,
}

impl Target for FanoutTarget {
    async fn handle(&self, req: &ScheduledRequest) -> Result<Response> {
        let mut spans = Vec::with_capacity(req.required.len());
        let mut digests = Vec::with_capacity(req.required.len());

        let futs = req.required.iter().map(|name| {
            let ctx = CallCtx {
                request_id: req.request_id,
                attempt: 0,
            };
            let ds = self.downstreams.clone();
            let name = name.clone();
            async move {
                let reply = ds.call(&name, ctx).await;
                (name, ctx, reply)
            }
        });

        for (name, ctx, reply) in futures::future::join_all(futs).await {
            let reply = reply?;
            spans.push(span_of(&name, ctx, &reply));
            if let Some(d) = reply.digest {
                digests.push(d);
            }
        }

        Ok(Response {
            digest: Some(fold_digest(req.nonce, &mut digests)),
            spans,
            failure: None,
        })
    }
}

/// Sleeps for a draw from a distribution and returns. No downstreams, no
/// concurrency limit.
///
/// Under unbounded concurrency there is no queueing, so measured e2e latency
/// should converge to the distribution itself -- which is what allows the whole
/// pipeline to be checked against a closed form.
pub struct SyntheticTarget<C: Clock> {
    pub dist: Distribution,
    pub clock: C,
    pub seed: u64,
}

impl<C: Clock> Target for SyntheticTarget<C> {
    async fn handle(&self, req: &ScheduledRequest) -> Result<Response> {
        let mut rng: ChaCha8Rng = call_rng(self.seed, req.request_id, u16::MAX, 0);
        let d = self.dist.sample(&mut rng);
        self.clock.sleep_until(self.clock.now() + d).await;
        // No downstream calls, so no required-call check applies; the harness
        // treats an empty `requires` as satisfied.
        Ok(Response {
            digest: Some(fold_digest(req.nonce, &mut Vec::new())),
            spans: Vec::new(),
            failure: None,
        })
    }
}
