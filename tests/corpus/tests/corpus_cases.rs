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
    let mut skipped: Vec<String> = Vec::new();

    for (path, case) in &cases {
        if case.slow {
            continue;
        }
        let scratch = CaseScratch::new(&case.id).expect("scratch directory");
        let report = run_case(case, scratch.path()).await;
        // A skip is neither a pass nor a failure. Counting one as green would report coverage
        // on a platform where the case never ran, which is the false confidence the corpus
        // exists to prevent — so skips are named, counted apart, and printed every run.
        if let Some(reason) = report.skipped() {
            skipped.push(format!("{}: {reason}", case.id));
            continue;
        }
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
    if !skipped.is_empty() {
        println!(
            "{} case(s) could not run here:\n  {}",
            skipped.len(),
            skipped.join("\n  ")
        );
    }
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

/// Every category's case count is checked against `docs/09-testing-strategy.md`'s inventory.
///
/// The totals in that table are the corpus's plan, and until now nothing compared them to what
/// exists — so a category could sit at a third of its target indefinitely and the only way to
/// notice was to count directories by hand. That is the same silent rot the invariant-proof
/// deferrals guard exists to prevent, one level up: a number nobody checks stops being a number
/// anyone can act on.
///
/// It fails on exactly one thing: a category directory docs/09 does not name. That catches a
/// misspelled directory and a case filed under a category that does not exist, both of which
/// otherwise sit there being counted in the total while belonging to nothing.
///
/// It deliberately does NOT fail on a shortfall, nor on an overshoot. docs/09's column is headed
/// "initial target": categories fill stage by stage — `proxies` and `media` cannot have cases
/// before the features they describe exist — and a category that grows past its initial number
/// is the corpus doing its job, not drift. The first version of this test asserted an upper
/// bound and immediately failed on `framing` at 19 and `validators` at 16, which is the test
/// being wrong rather than the corpus.
#[test]
fn every_category_is_named_by_the_docs_09_inventory() {
    // docs/09-testing-strategy.md §3.2's table, transcribed. If that table changes, this changes
    // with it — which is the point: the transcription is what makes the drift visible.
    let inventory: [(&str, usize); 10] = [
        ("ranges", 25),
        ("validators", 15),
        ("framing", 15),
        ("session", 20),
        ("connections", 20),
        ("redirects", 10),
        ("proxies", 12),
        ("protocols", 15),
        ("local", 12),
        ("media", 15),
    ];

    let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("cases");
    let mut on_disk: Vec<(String, usize)> = std::fs::read_dir(&cases_dir)
        .expect("the cases directory is readable")
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let count = std::fs::read_dir(entry.path())
                .expect("a category directory is readable")
                .filter_map(std::result::Result::ok)
                .filter(|f| f.path().extension().and_then(|e| e.to_str()) == Some("yaml"))
                .count();
            (entry.file_name().to_string_lossy().into_owned(), count)
        })
        .collect();
    on_disk.sort();

    let mut problems: Vec<String> = Vec::new();
    for (name, count) in &on_disk {
        let Some((_, target)) = inventory.iter().find(|(n, _)| n == name) else {
            problems.push(format!(
                "category {name:?} has {count} case(s) but docs/09 §3.2 does not name it — \
                 either the directory is misspelled or the inventory needs updating"
            ));
            continue;
        };
        let _ = target;
    }

    assert!(problems.is_empty(), "{}", problems.join("\n"));

    // Printed every run so the shortfalls are visible rather than something to go and count.
    let filled: usize = on_disk.iter().map(|(_, c)| c).sum();
    let planned: usize = inventory.iter().map(|(_, t)| t).sum();
    println!("corpus inventory: {filled} of {planned} planned");
    for (name, target) in inventory {
        let have = on_disk
            .iter()
            .find(|(n, _)| n == name)
            .map_or(0, |(_, c)| *c);
        let mark = if have >= target { "met" } else { "   " };
        println!("  {mark} {name:<12} {have:>3} / {target}");
    }
}
