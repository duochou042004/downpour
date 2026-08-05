//! S1-T10 — proof that the runner actually evaluates expectations.
//!
//! ADR-0010's third contract rule: the runner fails closed. This is the test that makes that
//! claim checkable rather than aspirational. A declarative runner which silently ignores what it
//! does not understand reports green forever while checking nothing, and the corpus metric climbs
//! the whole time — risk R-7 in `state/progress.json`.
//!
//! **If this file ever passes the fixture, every green result in the corpus is meaningless.**

use std::path::Path;

use downpour_corpus::case::Case;
use downpour_corpus::runner::{CaseScratch, run_case};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("self-test")
        .join(name)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runner_fails_a_case_whose_expectation_is_wrong() {
    let case = Case::from_path(&fixture("deliberately-wrong.yaml")).expect("the fixture loads");
    let scratch = CaseScratch::new(&case.id).expect("scratch directory");

    let report = run_case(&case, scratch.path()).await;

    assert!(
        !report.passed(),
        "the runner PASSED a case that claims a gzipped ranged response completes and is renamed. \
         Every other corpus result is now suspect. See ADR-0010 contract rule 3."
    );
    // And it must say why, in terms a case author can act on.
    let described = report.describe();
    assert!(
        described.contains("expected the download to succeed"),
        "the report must name the violated expectation, got: {described}"
    );
    assert!(
        described.contains("file_renamed"),
        "file_renamed is checked on every case (I-4), got: {described}"
    );
}

#[test]
fn a_case_with_an_unknown_key_is_rejected_rather_than_partially_applied() {
    // The specific hazard: a typo'd or unsupported expectation silently dropped, so the case
    // passes while asserting less than it says.
    let yaml = r#"
id: typo
category: framing
description: a case with a misspelled expectation
references: ["ADR-0010"]
server:
  content: { size: 1KiB, seed: 1 }
expect:
  final_state: completed
  file_renamed: true
  file_renmaed: true
"#;
    let error = serde_norway::from_str::<Case>(yaml)
        .expect_err("an unknown key must be a hard error, not a dropped assertion");
    let message = error.to_string();
    assert!(
        message.contains("file_renmaed"),
        "the error must name the key, got: {message}"
    );
}

#[test]
fn a_case_that_expects_failure_must_say_which_failure() {
    // "It failed somehow" passes for the wrong reason and stops detecting regressions.
    let yaml = r#"
id: vague
category: framing
description: expects failure without naming a kind
references: ["ADR-0010"]
server:
  content: { size: 1KiB, seed: 1 }
expect:
  final_state: failed
  file_renamed: false
"#;
    let case: Case = serde_norway::from_str(yaml).expect("it parses");
    let _ = case;
    let error = Case::from_path(Path::new("/nonexistent")).expect_err("missing file");
    let _ = error;

    // Validation runs in `from_path`, so assert it through a real file.
    let dir = CaseScratch::new("vague").expect("scratch");
    let path = dir.path().join("vague.yaml");
    std::fs::write(&path, yaml).expect("write the fixture");
    let error = Case::from_path(&path).expect_err("a failure case with no error_kind is invalid");
    assert!(error.to_string().contains("error_kind"), "got: {error}");
}

#[test]
fn a_case_naming_an_unimplemented_generator_is_rejected() {
    // ADR-0010 gives a new generator a new name rather than redefining blake3-ctr-v1. A case that
    // names one this build does not implement must fail loudly, not fall back to the one it has.
    let yaml = r#"
id: future-generator
category: framing
description: names a generator this build does not implement
references: ["ADR-0010"]
server:
  content: { size: 1KiB, seed: 1, generator: blake3-ctr-v2 }
expect:
  final_state: completed
  file_renamed: true
"#;
    let dir = CaseScratch::new("gen").expect("scratch");
    let path = dir.path().join("future-generator.yaml");
    std::fs::write(&path, yaml).expect("write the fixture");
    let error = Case::from_path(&path).expect_err("an unimplemented generator is invalid");
    assert!(error.to_string().contains("blake3-ctr-v2"), "got: {error}");
}

#[test]
fn a_mid_transfer_mutation_the_server_cannot_enact_is_rejected() {
    // S2-T9 implements ADR-0010's `behaviour`, so the blanket rejection is gone. What replaces
    // it must keep the same property: a case describing a mutation the server will not perform
    // has to fail to LOAD, never load and quietly change nothing. An `etag-changed-midway` case
    // that passes without any ETag ever changing is the most dangerous false green available
    // here, because it reports I-3 as proven while proving nothing.
    let header = r#"
id: midway
category: validators
description: changes the etag part way through
references: ["INVARIANTS.md#i-3"]
server:
  content: { size: 1KiB, seed: 1 }
"#;
    let expectations = r#"
expect:
  final_state: failed
  error_kind: validator_mismatch
  file_renamed: false
"#;

    // An effect the server has no code for.
    let unknown_effect = r#"
  behaviour:
    - at: { bytes_served: "40%" }
      then: { set_last_modified: "Mon, 3 Aug 2026 10:00:00 GMT" }
"#;
    // A trigger the server has no code for.
    let unknown_trigger = r#"
  behaviour:
    - at: { requests_served: 2 }
      then: { set_etag: '"v2"' }
"#;
    // Fires before the first byte, so nothing changes *mid*-transfer and the case would pass
    // without the client ever meeting a changed representation.
    let never_midway = r#"
  behaviour:
    - at: { bytes_served: "0%" }
      then: { set_etag: '"v2"' }
"#;

    for (label, behaviour) in [
        ("an unimplemented effect", unknown_effect),
        ("an unimplemented trigger", unknown_trigger),
        ("a trigger that fires before the transfer", never_midway),
    ] {
        let dir = CaseScratch::new("midway").expect("scratch");
        let path = dir.path().join("midway.yaml");
        std::fs::write(&path, format!("{header}{behaviour}{expectations}"))
            .expect("write the fixture");
        assert!(
            Case::from_path(&path).is_err(),
            "{label} loaded instead of being rejected; a case describing a mutation the server \
             does not perform would report I-3 as proven while nothing changed mid-transfer"
        );
    }

    // The one shape the server does enact must still load, or the corpus cannot express I-3.
    let good = r#"
  behaviour:
    - at: { bytes_served: "40%" }
      then: { set_etag: '"v2"' }
"#;
    let dir = CaseScratch::new("midway-ok").expect("scratch");
    let path = dir.path().join("midway.yaml");
    std::fs::write(&path, format!("{header}{good}{expectations}")).expect("write the fixture");
    Case::from_path(&path).expect("the implemented mutation must load");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runner_detects_a_server_that_serves_corrupt_bytes() {
    // The most important self-test in the project. Every other case relies on the runner's
    // byte-for-byte comparison firing when it should, and a comparison that is only ever observed
    // passing cannot be told from one that never runs. Here the server returns the right length
    // with correct headers and wrong contents — silent corruption, exactly as it occurs in the
    // wild — and the runner must catch it and name the offset.
    let case = Case::from_path(&fixture("serves-corrupt-bytes.yaml")).expect("the fixture loads");
    let scratch = CaseScratch::new(&case.id).expect("scratch directory");

    let report = run_case(&case, scratch.path()).await;

    assert!(
        !report.passed(),
        "the runner accepted a file of the right size with the wrong contents. The corpus cannot \
         detect corruption, so every green result in it means nothing."
    );
    assert!(
        report.silent_corruption,
        "the finding must be flagged as silent corruption"
    );

    let described = report.describe();
    assert!(
        described.contains("SILENT CORRUPTION"),
        "the report must say so unmistakably, got: {described}"
    );
    assert!(
        described.contains("byte 32768"),
        "the report must name the first corrupt offset so it is actionable, got: {described}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runner_detects_corrupt_bytes_in_an_unfinished_part_file() {
    // The companion to `the_runner_detects_a_server_that_serves_corrupt_bytes`, and it exists
    // because S2-T8 narrowed what gets compared. Part files are preallocated now (I-10), so most
    // of an unfinished one is a hole; comparing it whole would report the absence of bytes nobody
    // claimed. The runner instead compares the ranges the journal records as durable — and a
    // narrowing like that is precisely how a real check becomes a no-op nobody notices, because
    // every case keeps passing.
    //
    // This fixture fails for a genuine reason (truncated body) that the runner already checks,
    // so the assertions below can only be satisfied by the byte comparison actually running
    // inside the durable prefix.
    let case = Case::from_path(&fixture("corrupt-bytes-in-an-unfinished-part-file.yaml"))
        .expect("the fixture loads");
    let scratch = CaseScratch::new(&case.id).expect("scratch directory");

    let report = run_case(&case, scratch.path()).await;

    assert!(
        report.silent_corruption,
        "the runner accepted an unfinished part file whose durable bytes are wrong. Every corpus \
         case that ends in a .dppart is then checking nothing. Report was: {}",
        report.describe()
    );
    let described = report.describe();
    assert!(
        described.contains("SILENT CORRUPTION"),
        "the report must say so unmistakably, got: {described}"
    );
    assert!(
        described.contains("byte 0"),
        "the first corrupt offset must be named so it is actionable, got: {described}"
    );
}
