//! S1-T10/T11 — run every declarative corpus case.
//!
//! Each YAML file under `tests/corpus/cases/` is enacted by the pathology server and run against
//! the engine. The runner imposes the assertions no case may opt out of (ADR-0010): a byte-for-byte
//! comparison against the generator, and `file_renamed` (I-4).
//!
//! Loading is part of the test. A case that will not parse — including one carrying a key the
//! runner does not understand — is a **failure**, never a skip. `runner_self_test.rs` proves the
//! evaluation is real by feeding the runner a case whose expectation is deliberately wrong.

use std::path::{Path, PathBuf};

use downpour_corpus::case::Case;
use downpour_corpus::runner::{CaseScratch, discover, run_case};

fn cases_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("cases")
}

/// Load every case, so a malformed one fails loudly and immediately rather than being skipped.
fn load_all() -> Vec<(PathBuf, Case)> {
    let root = cases_root();
    let files = discover(&root).expect("the cases directory is readable");
    assert!(!files.is_empty(), "no cases found under {}", root.display());

    files
        .into_iter()
        .map(|path| {
            let case = Case::from_path(&path)
                .unwrap_or_else(|error| panic!("{} failed to load: {error}", path.display()));
            (path, case)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_corpus_case_passes() {
    let cases = load_all();
    let mut failures = Vec::new();
    let mut ran = 0_usize;

    for (path, case) in &cases {
        if case.slow {
            continue;
        }
        let scratch = CaseScratch::new(&case.id).expect("scratch directory");
        let report = run_case(case, scratch.path()).await;
        ran += 1;
        if !report.passed() {
            failures.push(format!("{}\n{}", path.display(), report.describe()));
        }
    }

    assert!(ran > 0, "no non-slow cases ran");
    assert!(
        failures.is_empty(),
        "{} of {ran} corpus case(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    println!("{ran} corpus cases passed");
}

#[test]
fn every_case_has_a_unique_id_matching_its_filename() {
    // The id is cited in commit messages and bug reports, so a duplicate or a mismatch makes a
    // case unfindable — and an id that drifts from its filename is how a "fixed" case gets edited
    // in one place and asserted in another.
    let mut seen: Vec<String> = Vec::new();
    for (path, case) in load_all() {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        assert_eq!(
            case.id,
            stem,
            "{} declares id {:?}",
            path.display(),
            case.id
        );
        assert!(!seen.contains(&case.id), "duplicate case id {:?}", case.id);
        seen.push(case.id);
    }
}

#[test]
fn every_case_lives_in_its_category_directory() {
    // Otherwise `ls tests/corpus/cases/ranges | wc -l` stops being a true count, and the corpus
    // inventory is the whole point of the format being declarative (ADR-0010).
    for (path, case) in load_all() {
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        assert_eq!(
            parent,
            Some(case.category.directory()),
            "{} is category {:?} but sits in {:?}",
            path.display(),
            case.category,
            parent
        );
    }
}

#[test]
fn the_recorded_case_count_matches_reality() {
    // metrics.corpus_cases_total in state/progress.json is quoted as evidence, so it has to be
    // true. This test is what stops it drifting into a number nobody checks.
    let cases = load_all();
    let progress = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../state/progress.json"),
    )
    .expect("progress.json is readable");

    let needle = "\"corpus_cases_total\":";
    let recorded: usize = progress
        .split(needle)
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("corpus_cases_total is present and numeric");

    assert_eq!(
        recorded,
        cases.len(),
        "state/progress.json records {recorded} corpus cases but {} exist on disk",
        cases.len()
    );
}
