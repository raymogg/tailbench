//! Scenario configuration.
//!
//! Unknown fields are rejected everywhere, so a config written against the full
//! Phase 1 schema fails loudly rather than silently ignoring `[topology]`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

use crate::dist::Distribution;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub scenario: Scenario,
    pub load: Load,
    pub slo: Slo,
    #[serde(default, rename = "request_class")]
    pub request_classes: Vec<RequestClassCfg>,
    #[serde(default, rename = "downstream")]
    pub downstreams: Vec<DownstreamCfg>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    pub seed: u64,
    pub duration_s: f64,
    pub warmup_s: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Arrival {
    Constant,
    Poisson,
    Bursty,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Load {
    pub arrival: Arrival,
    pub rate_rps: f64,
    #[serde(default)]
    pub burstiness_cv: Option<f64>,
    /// rejected rather than ignored -- it needs a measured capacity number
    /// from an expert solution, which does not exist until step 4.
    #[serde(default)]
    pub target_utilization: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slo {
    pub budget_ms: f64,
    /// Defaults to 10 x budget_ms; the multiplier is a guess pending the
    /// sweep, not a result.
    #[serde(default)]
    pub penalty_ms: Option<f64>,
}

impl Slo {
    pub fn penalty_ms(&self) -> f64 {
        self.penalty_ms.unwrap_or(self.budget_ms * 10.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestClassCfg {
    pub name: String,
    pub weight: f64,
    /// a *minimum* call set. Extra calls are permitted (P5 hedging needs
    /// that); order is unconstrained (P4's fix needs that).
    pub requires: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownstreamCfg {
    pub id: String,
    pub distribution: Distribution,
    pub capacity: usize,
    pub timeout_ms: f64,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_toml_str(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Every rejection names the offending value.
    pub fn validate(&self) -> Result<()> {
        let s = &self.scenario;
        if !(s.duration_s.is_finite() && s.duration_s > 0.0) {
            bail!("scenario.duration_s must be finite and > 0, got {}", s.duration_s);
        }
        if !(s.warmup_s.is_finite() && s.warmup_s >= 0.0) {
            bail!("scenario.warmup_s must be finite and >= 0, got {}", s.warmup_s);
        }
        if s.warmup_s >= s.duration_s {
            bail!(
                "scenario.warmup_s ({}) must be < duration_s ({})",
                s.warmup_s, s.duration_s
            );
        }

        if self.load.target_utilization.is_some() {
            bail!(
                "load.target_utilization is not supported yet: it needs a measured \
                 capacity from an expert solution (step 4). Use load.rate_rps."
            );
        }
        if !(self.load.rate_rps.is_finite() && self.load.rate_rps > 0.0) {
            bail!("load.rate_rps must be finite and > 0, got {}", self.load.rate_rps);
        }
        match (self.load.arrival, self.load.burstiness_cv) {
            (Arrival::Bursty, None) => {
                bail!("load.arrival = \"bursty\" requires load.burstiness_cv")
            }
            (Arrival::Bursty, Some(cv)) if !(cv.is_finite() && cv > 1.0) => {
                // CV <= 1 is not bursty; Poisson is exactly CV = 1.
                bail!("load.burstiness_cv must be > 1.0 (Poisson is CV = 1), got {cv}")
            }
            _ => {}
        }

        if !(self.slo.budget_ms.is_finite() && self.slo.budget_ms > 0.0) {
            bail!("slo.budget_ms must be finite and > 0, got {}", self.slo.budget_ms);
        }
        let penalty = self.slo.penalty_ms();
        if !penalty.is_finite() || penalty <= self.slo.budget_ms {
            // below this, quitting strictly dominates any late success.
            bail!(
                "slo.penalty_ms ({penalty}) must be > slo.budget_ms ({}); otherwise \
                 failing scores better than being slow",
                self.slo.budget_ms
            );
        }

        if self.downstreams.is_empty() {
            bail!("at least one [[downstream]] is required");
        }
        let mut seen_ds = HashSet::new();
        for d in &self.downstreams {
            if !seen_ds.insert(d.id.as_str()) {
                bail!("duplicate downstream id {:?}", d.id);
            }
            if d.capacity == 0 {
                bail!("downstream {:?}: capacity must be > 0", d.id);
            }
            if !(d.timeout_ms.is_finite() && d.timeout_ms > 0.0) {
                bail!("downstream {:?}: timeout_ms must be finite and > 0", d.id);
            }
            d.distribution
                .validate()
                .with_context(|| format!("downstream {:?}", d.id))?;
        }

        if self.request_classes.is_empty() {
            bail!("at least one [[request_class]] is required");
        }
        let mut seen_rc = HashSet::new();
        let mut total_weight = 0.0;
        for c in &self.request_classes {
            if !seen_rc.insert(c.name.as_str()) {
                bail!("duplicate request_class name {:?}", c.name);
            }
            if !(c.weight.is_finite() && c.weight > 0.0) {
                bail!("request_class {:?}: weight must be finite and > 0", c.name);
            }
            total_weight += c.weight;
            if c.requires.is_empty() {
                // A class that must do no work has no correctness oracle.
                bail!("request_class {:?}: requires must be non-empty", c.name);
            }
            for r in &c.requires {
                if !seen_ds.contains(r.as_str()) {
                    bail!(
                        "request_class {:?} requires undeclared downstream {:?}",
                        c.name, r
                    );
                }
            }
        }
        if (total_weight - 1.0).abs() > 1e-9 {
            bail!("request_class weights must sum to 1.0, got {total_weight}");
        }

        Ok(())
    }

    /// Stable index for a downstream id. Used as `downstream_id` in the RNG
    /// derivation, so it must depend only on config order.
    pub fn downstream_index(&self, id: &str) -> Option<u16> {
        self.downstreams.iter().position(|d| d.id == id).map(|i| i as u16)
    }
}
