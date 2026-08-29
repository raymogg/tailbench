//! The program under test must not reach harness internals.
//!
//! The crate boundary is the real enforcement: `crates/program` depends on
//! `tailbench-abi` and not on `tailbench`, so `use tailbench::oracle::Oracle;`
//! fails with "unresolved module or unlinked crate", and `call_digest` is not
//! in the ABI to import. Both were verified by trying them.
//!
//! This test is the second line of defence, and it catches what the boundary
//! cannot. A crate split stops the program *linking* the scorer; it does not
//! stop it reimplementing `call_digest` from the algorithm, or reading the seed
//! out of `scenarios/*.toml` -- under `scripts/run.sh` the program inherits the
//! repo root as its cwd. Text matching is weaker than types, but it is what
//! covers those paths.
//!
//! What is being protected: the program must never compute an expected digest
//! or read the scoring rule. If it can, it fabricates answers instead of
//! calling downstreams, and every score in `results/` is meaningless.

const PROGRAM: &str = include_str!("../../program/src/main.rs");

/// Harness modules, each with the reason it is off limits -- a bare list of
/// names ages into noise once someone asks "why not this one?".
const FORBIDDEN_MODULES: &[(&str, &str)] = &[
    ("oracle", "it could read the scoring rule"),
    ("config", "it holds the seed every digest derives from"),
    ("report", "it is the scoring output, not an input"),
    ("timeline", "it is post-hoc analysis of the run being measured"),
    ("load_generator", "it schedules the requests being served"),
    ("loadgen_client", "it is the other end of the connection"),
    ("distributions", "it is the latency the downstreams draw from"),
];

/// The seeded RNG entry points. `fold_digest` is deliberately absent: folding
/// digests obtained by actually calling downstreams is the program's job.
/// These five would let it derive those values instead of earning them.
const FORBIDDEN_RNG: &[(&str, &str)] = &[
    ("call_digest", "it is the expected answer, computable from the seed alone"),
    ("call_rng", "it draws the latencies the program is being timed against"),
    ("payload_nonce", "with call_digest it works backward toward the seed"),
    ("arrival_rng", "it is the arrival schedule the program is served"),
    ("class_rng", "it is the request mix the program is served"),
];

/// Does `needle` appear as a whole identifier, not as part of a longer one?
///
/// The load-bearing case is `call_digest` vs `fold_digest`: a plain
/// `contains()` for `call_digest` does not match `fold_digest`, but a plain
/// `contains()` for `call_rng` *does* match a hypothetical `recall_rng`, and
/// `config` matches `configure`. Require a non-identifier character (or an
/// edge) on both sides.
fn mentions_identifier(haystack: &str, needle: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    haystack.match_indices(needle).any(|(i, _)| {
        let before_ok = haystack[..i].chars().next_back().is_none_or(|c| !is_ident(c));
        let after_ok = haystack[i + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_ident(c));
        before_ok && after_ok
    })
}

/// Source with `//` comments stripped.
///
/// Needed because program.rs's own header prose says the library "carries the
/// wire types and the oracle" -- flagging that would make the test fail on the
/// untouched baseline, and the only ways out are editing the file the agent is
/// supposed to own or dropping the `oracle` check entirely. What matters is a
/// reference the compiler resolves, so drop the comments and keep the code.
///
/// Line-based, so it does not understand `/* */` or a `//` inside a string
/// literal. Both would have to appear in program.rs before that matters.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A module is *reached* by naming it in a path or a `use`, not by mentioning
/// it in prose. Checked as a qualified `tailbench::<module>`, a bare
/// `<module>::` path through an earlier import, and any `use` line naming it
/// (which covers a grouped `use tailbench::{oracle, ready};`).
fn reaches_module(code: &str, module: &str) -> bool {
    let qualified = format!("tailbench::{module}");
    let path = format!("{module}::");
    code.lines().any(|line| {
        line.contains(&qualified)
            || (line.contains(&path) && mentions_identifier(line, module))
            || (line.trim_start().starts_with("use ") && mentions_identifier(line, module))
    })
}

#[test]
fn program_does_not_reach_harness_modules() {
    let code = code_only(PROGRAM);
    for (module, why) in FORBIDDEN_MODULES {
        assert!(
            !reaches_module(&code, module),
            "program.rs must not reach `{module}`: {why}"
        );
    }
}

#[test]
fn program_does_not_use_seeded_rng() {
    let code = code_only(PROGRAM);
    for (func, why) in FORBIDDEN_RNG {
        assert!(
            !mentions_identifier(&code, func),
            "program.rs must not use `{func}`: {why}"
        );
    }
}

/// The checks above are only worth anything if they still fire on the real
/// thing and stay quiet on its lookalikes. `fold_digest` is the case that
/// matters: the program legitimately folds values it got by calling
/// downstreams, and a match for `call_digest` that also flagged `fold_digest`
/// would have to be loosened until it caught nothing.
#[test]
fn matching_is_neither_too_loose_nor_too_tight() {
    // Still present, so the counterexample below is a live one.
    assert!(
        mentions_identifier(PROGRAM, "fold_digest"),
        "program.rs no longer uses fold_digest -- if that is intended, drop this \
         assertion; if not, the matching above just lost its counterexample"
    );

    // Not too loose: near-misses and prose must not trip the checks.
    assert!(!mentions_identifier("fold_digest(nonce, &mut d)", "call_digest"));
    assert!(!mentions_identifier("recall_rng(seed)", "call_rng"));
    assert!(!reaches_module("let configure = 1;", "config"));
    assert!(!reaches_module("//! the library carries the oracle", "oracle"));

    // Not too tight: every form a real reach would take is caught.
    assert!(reaches_module("use tailbench::oracle::Oracle;", "oracle"));
    assert!(reaches_module("use tailbench::{config, ready};", "config"));
    assert!(reaches_module("    let c = oracle::Oracle::new();", "oracle"));
    assert!(mentions_identifier("use tailbench::rng::call_digest;", "call_digest"));
}
