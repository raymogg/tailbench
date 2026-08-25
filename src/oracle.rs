//! Success definition. Scorer-side: the service under test must never be
//! able to reach this module.

use std::collections::HashSet;

use crate::config::Config;
use crate::record::{CallOutcome, CallSpan, Outcome};
use crate::rng::{call_digest, fold_digest};
use crate::protocol::ProgramReply;
use crate::timeline::ScheduledRequest;

pub struct Oracle {
    seed: u64,
    downstream_index: Vec<(String, u16)>,
}

impl Oracle {
    pub fn new(cfg: &Config) -> Self {
        Oracle {
            seed: cfg.scenario.seed,
            downstream_index: cfg
                .downstreams
                .iter()
                .enumerate()
                .map(|(i, d)| (d.id.clone(), i as u16))
                .collect(),
        }
    }

    fn index_of(&self, id: &str) -> Option<u16> {
        self.downstream_index
            .iter()
            .find(|(n, _)| n == id)
            .map(|(_, i)| *i)
    }

    /// The digest a correct service must produce.
    ///
    /// Computable offline from `(seed, request_id, downstream_id)` without the
    /// service's cooperation, which is what makes it unforgeable: the values
    /// are only obtainable by actually calling the downstreams.
    pub fn expected_digest(&self, req: &ScheduledRequest) -> u64 {
        let mut digests: Vec<u64> = req
            .required
            .iter()
            .filter_map(|name| self.index_of(name))
            .map(|idx| call_digest(self.seed, req.request_id, idx))
            .collect();
        fold_digest(req.nonce, &mut digests)
    }

    /// every required downstream needs at least one *successful* call.
    ///
    /// A minimum, not an exact set -- extra calls are fine (P5 hedging needs
    /// that), and order is unconstrained (P4's fix needs that).
    pub fn required_calls_met(&self, req: &ScheduledRequest, spans: &[CallSpan]) -> bool {
        let ok: HashSet<&str> = spans
            .iter()
            .filter(|s| s.outcome == CallOutcome::Ok)
            .map(|s| s.downstream_id.as_str())
            .collect();
        req.required.iter().all(|r| ok.contains(r.as_str()))
    }

    /// Classify one request.
    ///
    /// Note the ordering: the deadline is checked *first*. A late-but-correct
    /// response is `Expired`, not `Ok` -- past the deadline the response has no
    /// value, so correctness no longer matters.
    pub fn classify(
        &self,
        req: &ScheduledRequest,
        resp: &Result<ProgramReply, anyhow::Error>,
        completion_ns: u64,
        deadline_ns: u64,
    ) -> (Outcome, Option<bool>, bool) {
        let resp = match resp {
            Ok(r) => r,
            Err(_) => return (Outcome::Error, None, false),
        };
        if resp.error.is_some() {
            return (Outcome::Error, None, false);
        }

        let calls_met = self.required_calls_met(req, &resp.spans);
        let digest_ok = resp.digest.map(|d| d == self.expected_digest(req));

        if completion_ns > deadline_ns {
            return (Outcome::Expired, digest_ok, calls_met);
        }
        if !calls_met || digest_ok != Some(true) {
            return (Outcome::Incorrect, digest_ok, calls_met);
        }
        (Outcome::Ok, digest_ok, calls_met)
    }
}
