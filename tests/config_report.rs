//! §3.1 validation rules and §9.3 aggregation.

use tailbench::config::Config;
use tailbench::report::{cvar, percentile};

fn base() -> String {
    r#"
[scenario]
id = "t"
seed = 1
duration_s = 10.0
warmup_s = 1.0
[load]
arrival = "poisson"
rate_rps = 100.0
[slo]
budget_ms = 100.0
[[request_class]]
name = "c"
weight = 1.0
requires = ["svc_a"]
[[downstream]]
id = "svc_a"
distribution = { kind = "constant", ms = 5.0 }
capacity = 8
timeout_ms = 200.0
"#
    .to_string()
}

#[test]
fn base_config_is_valid() {
    assert!(Config::from_str(&base()).is_ok());
}

#[test]
fn rejects_bad_configs() {
    let cases = vec![
        ("warmup >= duration", base().replace("warmup_s = 1.0", "warmup_s = 10.0")),
        ("zero rate", base().replace("rate_rps = 100.0", "rate_rps = 0.0")),
        ("zero capacity", base().replace("capacity = 8", "capacity = 0")),
        ("weights not 1", base().replace("weight = 1.0", "weight = 0.5")),
        ("empty requires", base().replace(r#"requires = ["svc_a"]"#, "requires = []")),
        (
            "undeclared downstream",
            base().replace(r#"requires = ["svc_a"]"#, r#"requires = ["nope"]"#),
        ),
        (
            "penalty <= budget",
            base().replace("budget_ms = 100.0", "budget_ms = 100.0\npenalty_ms = 50.0"),
        ),
        (
            "bursty without cv",
            base().replace(r#"arrival = "poisson""#, r#"arrival = "bursty""#),
        ),
        (
            "target_utilization not supported",
            base().replace("rate_rps = 100.0", "rate_rps = 100.0\ntarget_utilization = 0.85"),
        ),
        (
            "unknown field",
            base().replace("[topology]", "").replace("seed = 1", "seed = 1\nbogus = 3"),
        ),
    ];
    for (name, text) in cases {
        assert!(Config::from_str(&text).is_err(), "{name} should be rejected");
    }
}

#[test]
fn percentile_is_nearest_rank() {
    let xs: Vec<f64> = (1..=100).map(|i| i as f64).collect();
    assert_eq!(percentile(&xs, 0.99), 99.0);
    assert_eq!(percentile(&xs, 0.50), 50.0);
    assert_eq!(percentile(&xs, 1.0), 100.0);
}

/// §6.5.2: mean of the worst ceil(n * 0.01).
#[test]
fn cvar_is_mean_of_worst_tail() {
    let xs: Vec<f64> = (1..=100).map(|i| i as f64).collect();
    assert_eq!(cvar(&xs, 0.99), 100.0);
    // Worst 10 of 100 -> mean(91..100) = 95.5
    assert_eq!(cvar(&xs, 0.90), 95.5);
}

/// §6.5.1: the reason CVaR is the optimization target. p99 is flat in the
/// penalty below 1% failures and equals it above; CVaR responds throughout.
#[test]
fn cvar_responds_to_failures_where_p99_does_not() {
    let good: Vec<f64> = (0..995).map(|_| 10.0).collect();
    let with_failures = |k: usize, penalty: f64| {
        let mut v = good.clone();
        v.extend(std::iter::repeat(penalty).take(k));
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    };

    // 5 failures in 1000 = 0.5%, below the p99 threshold.
    let a = with_failures(5, 1000.0);
    let b = with_failures(5, 5000.0);
    assert_eq!(
        percentile(&a, 0.99),
        percentile(&b, 0.99),
        "p99 should be blind to the penalty value below 1% failures"
    );
    assert!(
        cvar(&b, 0.99) > cvar(&a, 0.99),
        "cvar must respond to the penalty value"
    );
}
