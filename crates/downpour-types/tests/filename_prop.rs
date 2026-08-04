//! S1-T4 — filename resolution and sanitisation.
//!
//! Exit criterion S1-C3. The property that matters is not "the name looks reasonable", it
//! is **the name is exactly one normal path component**. A single-component name cannot
//! escape the directory it is joined to, whatever the server put in `Content-Disposition`.
//! `../../etc/passwd` arriving in a header is a routine occurrence, not an exotic attack.

use std::path::{Component, Path};

use downpour_types::filename;
use proptest::prelude::*;
use url::Url;

fn url(s: &str) -> Url {
    s.parse().expect("test URL is well formed")
}

// ---------------------------------------------------------------- resolution

#[test]
fn content_disposition_filename_star_wins_over_filename() {
    // RFC 6266 §4.3: when both are present, `filename*` is used.
    let cd = "attachment; filename=\"fallback.bin\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf";
    assert_eq!(
        filename::resolve(Some(cd), &url("https://example.com/x/ignored.dat")),
        "résumé.pdf"
    );
}

#[test]
fn a_quoted_filename_is_unquoted_and_unescaped() {
    let cd = r#"attachment; filename="my \"quoted\" report.pdf""#;
    assert_eq!(
        filename::resolve(Some(cd), &url("https://example.com/x")),
        "my _quoted_ report.pdf"
    );
}

#[test]
fn an_unquoted_filename_token_is_accepted() {
    let cd = "attachment; filename=archive.tar.gz";
    assert_eq!(
        filename::resolve(Some(cd), &url("https://example.com/x")),
        "archive.tar.gz"
    );
}

#[test]
fn parameter_names_are_case_insensitive() {
    let cd = "ATTACHMENT; FileName=\"Report.PDF\"";
    assert_eq!(
        filename::resolve(Some(cd), &url("https://example.com/x")),
        "Report.PDF"
    );
}

#[test]
fn falls_back_to_the_url_path_and_percent_decodes_it() {
    assert_eq!(
        filename::resolve(None, &url("https://example.com/a/b/report%20final.pdf")),
        "report final.pdf"
    );
}

#[test]
fn a_query_string_is_not_part_of_the_name() {
    assert_eq!(
        filename::resolve(
            None,
            &url("https://example.com/dl/file.zip?token=abc&expires=1")
        ),
        "file.zip"
    );
}

#[test]
fn a_url_with_no_usable_path_yields_the_default() {
    assert_eq!(
        filename::resolve(None, &url("https://example.com/")),
        "download"
    );
    assert_eq!(
        filename::resolve(None, &url("https://example.com")),
        "download"
    );
    assert_eq!(
        filename::resolve(None, &url("https://example.com/a/b/")),
        "download"
    );
}

#[test]
fn a_content_disposition_with_no_filename_falls_through_to_the_url() {
    assert_eq!(
        filename::resolve(Some("attachment"), &url("https://example.com/real.bin")),
        "real.bin"
    );
    assert_eq!(
        filename::resolve(
            Some("inline; size=1234"),
            &url("https://example.com/real.bin")
        ),
        "real.bin"
    );
}

#[test]
fn a_traversal_in_content_disposition_cannot_escape() {
    for attempt in [
        "attachment; filename=\"../../../etc/passwd\"",
        "attachment; filename=\"/etc/shadow\"",
        "attachment; filename=\"..\\\\..\\\\windows\\\\system32\\\\drivers\\\\etc\\\\hosts\"",
        "attachment; filename*=UTF-8''..%2F..%2Fetc%2Fpasswd",
    ] {
        let got = filename::resolve(Some(attempt), &url("https://example.com/x"));
        let p = Path::new(&got);
        assert_eq!(
            p.components().count(),
            1,
            "{attempt:?} resolved to {got:?}, which is not a single component"
        );
        assert!(
            !got.contains("..") || got == "download",
            "{attempt:?} -> {got:?}"
        );
    }
}

// ---------------------------------------------------------------- sanitisation

#[test]
fn traversal_and_platform_hazards_are_neutralised() {
    let cases = [
        ("../../etc/passwd", "passwd"),
        ("/etc/shadow", "shadow"),
        ("..\\..\\windows\\system32\\config", "config"),
        ("..", "download"),
        (".", "download"),
        ("...", "download"),
        ("", "download"),
        ("   ", "download"),
        // Windows reserved device names are unusable as filenames on Windows even with an
        // extension, and a cross-platform download manager cannot produce them.
        ("CON", "_CON"),
        ("con.txt", "_con.txt"),
        ("LPT9.log", "_LPT9.log"),
        ("NUL", "_NUL"),
        ("aux.tar.gz", "_aux.tar.gz"),
        // ...but a name that merely starts with those letters is fine.
        ("console.log", "console.log"),
        ("nullable.bin", "nullable.bin"),
        ("file\0name", "file_name"),
        ("a\nb\tc", "a_b_c"),
        ("tricky<>:\"|?*.bin", "tricky_______.bin"),
        // Windows silently strips trailing dots and spaces, which turns a verified name
        // into a different file on disk. Strip them ourselves so the name we verify is
        // the name we get.
        ("trailing.  ", "trailing"),
        ("trailing dots...", "trailing dots"),
        // Non-ASCII is not a hazard and must survive untouched.
        ("réal-nàme.tar.gz", "réal-nàme.tar.gz"),
        ("日本語.pdf", "日本語.pdf"),
    ];
    for (raw, expected) in cases {
        assert_eq!(filename::sanitise(raw), expected, "sanitising {raw:?}");
    }
}

#[test]
fn an_over_long_name_is_truncated_but_keeps_its_extension() {
    let long = format!("{}.tar.gz", "a".repeat(400));
    let got = filename::sanitise(&long);
    assert!(got.len() <= 255, "{} bytes is over the limit", got.len());
    assert!(got.ends_with(".tar.gz"), "{got:?} lost its extension");
}

#[test]
fn truncation_never_splits_a_multibyte_character() {
    // 400 three-byte characters: a byte-wise truncation at 255 would land mid-character
    // and produce invalid UTF-8.
    let long = "あ".repeat(400);
    let got = filename::sanitise(&long);
    assert!(got.len() <= 255);
    assert!(std::str::from_utf8(got.as_bytes()).is_ok());
}

proptest! {
    /// The load-bearing property for S1-C3: whatever the input, the result is exactly one
    /// normal path component. `Path::join` on such a name cannot leave the target directory.
    #[test]
    fn sanitised_name_is_always_exactly_one_normal_component(
        raw in proptest::collection::vec(any::<char>(), 0..120)
    ) {
        let raw: String = raw.into_iter().collect();
        let name = filename::sanitise(&raw);

        prop_assert!(!name.is_empty(), "input {:?} produced an empty name", raw);
        prop_assert!(name.len() <= 255, "input {:?} produced {} bytes", raw, name.len());
        prop_assert!(!name.contains('/'), "{:?} contains a forward slash", name);
        prop_assert!(!name.contains('\\'), "{:?} contains a backslash", name);
        prop_assert!(!name.contains('\0'), "{:?} contains a NUL", name);
        prop_assert!(!name.chars().any(char::is_control), "{:?} contains a control char", name);
        prop_assert_ne!(name.as_str(), ".");
        prop_assert_ne!(name.as_str(), "..");

        let mut components = Path::new(&name).components();
        match components.next() {
            Some(Component::Normal(_)) => {}
            other => prop_assert!(
                false, "input {:?} -> {:?} whose first component is {:?}", raw, name, other
            ),
        }
        prop_assert!(
            components.next().is_none(),
            "input {:?} -> {:?} has more than one component", raw, name
        );
    }

    /// Sanitising an already-sanitised name changes nothing. Without this, a name that
    /// round-trips through storage could drift on each pass, and the name we verified
    /// before renaming would stop matching the name on disk.
    #[test]
    fn sanitise_is_idempotent(raw in proptest::collection::vec(any::<char>(), 0..120)) {
        let raw: String = raw.into_iter().collect();
        let once = filename::sanitise(&raw);
        prop_assert_eq!(filename::sanitise(&once), once);
    }

    /// Resolution never panics, whatever a server puts in the header.
    #[test]
    fn resolve_never_panics(raw in proptest::collection::vec(any::<char>(), 0..120)) {
        let raw: String = raw.into_iter().collect();
        let name = filename::resolve(Some(&raw), &url("https://example.com/fallback.bin"));
        prop_assert_eq!(Path::new(&name).components().count(), 1);
    }
}
