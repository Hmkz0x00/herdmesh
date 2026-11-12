//! Calibrates `hm_proto::budget::estimate_tokens` against a real BPE
//! tokenizer (`o200k_base` via tiktoken-rs, dev-dependency only).
//!
//! Every fixture under `tests/fixtures/budget/` must be within ±15%,
//! and so must the aggregate over the whole corpus. Fixture file names are
//! `<category>_<nn>.<ext>`; per-category error is printed with
//! `cargo test -p hm-proto --test budget_calibration -- --nocapture`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use hm_proto::budget::{CharHeuristic, PretokenHeuristic, TokenEstimator, estimate_tokens};

const TOLERANCE: f64 = 0.15;

fn fixtures() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/budget");
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .expect("fixture dir")
        .map(|e| {
            let p = e.expect("dir entry").path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            // Normalise line endings so Windows checkouts (autocrlf) measure
            // the same text as Linux/macOS.
            let text = std::fs::read_to_string(&p)
                .expect("utf-8 fixture")
                .replace("\r\n", "\n");
            (name, text)
        })
        .collect();
    out.sort();
    out
}

fn rel_err(est: usize, real: usize) -> f64 {
    (est as f64 - real as f64) / real as f64
}

#[test]
fn corpus_shape() {
    let fx = fixtures();
    assert!(fx.len() >= 20, "want ~20 fixtures, got {}", fx.len());
    for (name, text) in &fx {
        let n = text.chars().count();
        assert!(
            (200..=2000).contains(&n),
            "{name}: {n} chars, want 200..=2000"
        );
    }
}

#[test]
fn heuristic_within_15_percent_of_o200k() {
    let bpe = tiktoken_rs::o200k_base().expect("o200k vocab");
    let mut failures = Vec::new();
    let mut by_cat: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let (mut est_sum, mut real_sum) = (0usize, 0usize);

    for (name, text) in fixtures() {
        let real = bpe.encode_ordinary(&text).len();
        let est = estimate_tokens(&text);
        let err = rel_err(est, real);
        println!(
            "{name:<16} real={real:>4} est={est:>4} err={:>+6.1}%",
            err * 100.0
        );
        if err.abs() > TOLERANCE {
            failures.push(format!("{name}: {:+.1}%", err * 100.0));
        }
        let cat = name.split('_').next().unwrap_or("?").to_owned();
        let e = by_cat.entry(cat).or_default();
        e.0 += est;
        e.1 += real;
        est_sum += est;
        real_sum += real;
    }

    for (cat, (est, real)) in &by_cat {
        println!(
            "category {cat:<6} err={:>+6.1}%",
            rel_err(*est, *real) * 100.0
        );
    }
    let agg = rel_err(est_sum, real_sum);
    println!(
        "aggregate        real={real_sum} est={est_sum} err={:+.1}%",
        agg * 100.0
    );

    assert!(failures.is_empty(), "outside ±15%: {failures:?}");
    assert!(
        agg.abs() <= TOLERANCE,
        "aggregate error {:+.1}%",
        agg * 100.0
    );
}

#[test]
fn default_estimator_is_pretoken_heuristic() {
    let s = "hello, world: /crates/hm-proto 12345";
    assert_eq!(estimate_tokens(s), PretokenHeuristic.estimate(s));
}

/// The plain divisor is only promised on aggregate, not per fixture.
#[test]
fn char_heuristic_aggregate_within_15_percent() {
    let bpe = tiktoken_rs::o200k_base().expect("o200k vocab");
    let h = CharHeuristic::default();
    let (mut est, mut real) = (0usize, 0usize);
    for (_, text) in fixtures() {
        est += h.estimate(&text);
        real += bpe.encode_ordinary(&text).len();
    }
    let err = rel_err(est, real);
    println!("CharHeuristic aggregate err={:+.1}%", err * 100.0);
    assert!(
        err.abs() <= TOLERANCE,
        "aggregate error {:+.1}%",
        err * 100.0
    );
}
