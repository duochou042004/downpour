//! Every corpus case that `docs/agent/INVARIANTS.md` names as an invariant's proof either exists or
//! is explicitly deferred.
//!
//! **This test exists because the gap it guards was real.** I-6 states its proof as three corpus
//! cases — `accept-ranges-lies`, `head-differs-from-get`, `cdn-edge-disagrees` — and for most of
//! Stage 1 only the first existed. Nothing detected that. The stage was very nearly declared
//! complete with two thirds of one invariant's stated proof missing, and the only reason it was
//! caught was a manual grep that happened to be run.
//!
//! INVARIANTS.md is Normative and says "violations block releases". A proof list nobody checks is
//! not a proof list, so this makes it mechanical.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Case names appearing in a `**Proof:** corpus cases ...` line of INVARIANTS.md.
///
/// Only that phrasing is parsed. Invariants proved by simulation or property tests say so
/// differently and are out of scope here.
fn cases_named_in_invariants() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(repo_root().join("docs/agent/INVARIANTS.md"))
        .expect("INVARIANTS.md is readable");

    let mut named = BTreeMap::new();
    let mut current_invariant = String::new();

    // A proof list can wrap onto the following line, so accumulate until the sentence ends.
    let mut pending: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("### ") {
            current_invariant = rest.split(' ').next().unwrap_or("").to_owned();
        }
        let candidate = match line.strip_prefix("**Proof:** corpus cases") {
            Some(rest) => Some(rest.to_owned()),
            None => pending.take().map(|open| format!("{open} {line}")),
        };
        let Some(text) = candidate else { continue };
        if !text.contains('.') {
            pending = Some(text);
            continue;
        }
        for token in text.split('`').skip(1).step_by(2) {
            let name = token.trim();
            if !name.is_empty() {
                named.insert(name.to_owned(), current_invariant.clone());
            }
        }
    }
    assert!(
        !named.is_empty(),
        "parsed no proof-list case names; the parser or the doc changed"
    );
    named
}

fn existing_cases() -> BTreeMap<String, PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("cases");
    downpour_corpus::runner::discover(&root)
        .expect("the cases directory is readable")
        .into_iter()
        .filter_map(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| (s.to_owned(), p.clone()))
        })
        .collect()
}

fn deferrals() -> BTreeMap<String, (String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("invariant-proof-deferrals.yaml");
    let text = std::fs::read_to_string(&path).expect("the deferrals file is readable");
    let raw: BTreeMap<String, BTreeMap<String, String>> =
        serde_norway::from_str(&text).expect("the deferrals file parses");
    raw.into_iter()
        .map(|(name, fields)| {
            let stage = fields.get("stage").cloned().unwrap_or_default();
            let reason = fields.get("reason").cloned().unwrap_or_default();
            (name, (stage, reason))
        })
        .collect()
}

#[test]
fn every_named_proof_case_exists_or_is_deferred_with_a_reason() {
    let named = cases_named_in_invariants();
    let existing = existing_cases();
    let deferred = deferrals();

    let mut missing = Vec::new();
    for (case, invariant) in &named {
        if existing.contains_key(case) {
            continue;
        }
        match deferred.get(case) {
            Some((stage, reason)) => {
                assert!(!stage.is_empty(), "{case} is deferred with no stage");
                assert!(
                    reason.trim().len() > 30,
                    "{case} is deferred with no substantive reason: {reason:?}"
                );
            }
            None => missing.push(format!("{case} (proof of {invariant})")),
        }
    }

    assert!(
        missing.is_empty(),
        "INVARIANTS.md names {} corpus case(s) that neither exist nor are deferred:\n  {}\n\
         Either write the case, or add it to tests/corpus/invariant-proof-deferrals.yaml with a \
         stage and a reason. A proof list nobody checks is not a proof list.",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn no_deferral_is_stale() {
    // A deferral for a case that now exists would quietly excuse work already done, and the file
    // would drift into a list nobody trusts.
    let existing = existing_cases();
    let deferred = deferrals();
    let stale: Vec<&String> = deferred
        .keys()
        .filter(|n| existing.contains_key(*n))
        .collect();
    assert!(
        stale.is_empty(),
        "these cases now exist but are still listed as deferred: {stale:?}"
    );
}

#[test]
fn every_deferral_is_actually_named_by_an_invariant() {
    // Otherwise the file becomes a place to park unrelated ideas.
    let named = cases_named_in_invariants();
    let unknown: Vec<String> = deferrals()
        .keys()
        .filter(|n| !named.contains_key(*n))
        .cloned()
        .collect();
    assert!(
        unknown.is_empty(),
        "these deferrals are not named as any invariant's proof in INVARIANTS.md: {unknown:?}"
    );
}
