//! Part-file creation, allocation, and positional-write proofs for I-2 and I-10.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use downpour_storage::part_file::{PartFile, PartFileError, PreallocationMethod};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(tag: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-part-file-{tag}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn expected_part_path(target: &Path) -> PathBuf {
    let mut part_name = OsString::from(target.as_os_str());
    part_name.push(".dppart");
    PathBuf::from(part_name)
}

#[test]
fn creation_is_exclusive_and_positional_writes_preserve_other_offsets() {
    let directory = TestDirectory::new("exclusive-positional");
    let target = directory.path().join("archive.bin");
    let expected_part = expected_part_path(&target);
    let part = PartFile::create(&target, 32 * 1024).unwrap();

    assert_eq!(part.path(), expected_part);
    assert_eq!(part.total_length(), 32 * 1024);
    assert_eq!(fs::metadata(part.path()).unwrap().len(), 32 * 1024);

    part.write_all_at(24 * 1024, b"right").unwrap();
    part.write_all_at(1024, b"left").unwrap();
    part.write_all_at(24 * 1024 + 2, b"--").unwrap();

    let before_collision = fs::read(part.path()).unwrap();
    let collision = PartFile::create(&target, 32 * 1024).unwrap_err();
    assert!(matches!(collision, PartFileError::AlreadyExists { .. }));
    assert_eq!(fs::read(part.path()).unwrap(), before_collision);

    let bytes = fs::read(part.path()).unwrap();
    assert_eq!(&bytes[1024..1028], b"left");
    assert_eq!(&bytes[24 * 1024..24 * 1024 + 5], b"ri--t");
    assert!(bytes[1028..24 * 1024].iter().all(|byte| *byte == 0));
}

#[test]
fn out_of_bounds_write_is_rejected_without_partial_mutation() {
    let directory = TestDirectory::new("write-bounds");
    let target = directory.path().join("bounded.bin");
    let part = PartFile::create(&target, 16).unwrap();
    part.write_all_at(12, b"safe").unwrap();
    let before = fs::read(part.path()).unwrap();

    let crossing_end = part.write_all_at(14, b"over").unwrap_err();
    assert!(matches!(
        crossing_end,
        PartFileError::OutOfBounds {
            offset: 14,
            length: 4,
            total_length: 16,
        }
    ));
    assert_eq!(fs::read(part.path()).unwrap(), before);

    let overflowing = part.write_all_at(u64::MAX, b"x").unwrap_err();
    assert!(matches!(overflowing, PartFileError::OutOfBounds { .. }));
    assert_eq!(fs::read(part.path()).unwrap(), before);
}

#[test]
fn zero_length_needs_no_reservation_and_accepts_no_bytes() {
    let directory = TestDirectory::new("zero-length");
    let target = directory.path().join("empty.bin");
    let part = PartFile::create(&target, 0).unwrap();

    assert_eq!(part.preallocation_method(), PreallocationMethod::NotNeeded);
    assert!(part.space_reserved());
    assert_eq!(part.allocated_size().unwrap(), 0);
    assert_eq!(fs::metadata(part.path()).unwrap().len(), 0);
    part.write_all_at(0, b"").unwrap();
    assert!(matches!(
        part.write_all_at(0, b"x"),
        Err(PartFileError::OutOfBounds { .. })
    ));
}

#[test]
fn an_unrepresentable_length_is_rejected_before_the_part_file_is_created() {
    let directory = TestDirectory::new("unsupported-length");
    let target = directory.path().join("too-large.bin");
    let expected_part = expected_part_path(&target);

    let error = PartFile::create(&target, u64::MAX).unwrap_err();

    assert!(matches!(
        error,
        PartFileError::UnsupportedLength { length: u64::MAX }
    ));
    assert!(!expected_part.exists());
}

#[test]
fn a_target_without_a_filename_is_rejected_before_creation() {
    let error = PartFile::create(Path::new(""), 1).unwrap_err();

    assert!(matches!(error, PartFileError::InvalidTarget { .. }));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_preallocation_reserves_physical_blocks_before_the_first_write() {
    use std::os::unix::fs::MetadataExt;

    const LENGTH: u64 = 8 * 1024 * 1024;
    let directory = TestDirectory::new("linux-preallocation");
    let target = directory.path().join("reserved.bin");
    let part = PartFile::create(&target, LENGTH).unwrap();
    let metadata = fs::metadata(part.path()).unwrap();

    assert!(matches!(
        part.preallocation_method(),
        PreallocationMethod::FallocateKeepSize | PreallocationMethod::PosixFallocate
    ));
    assert!(part.space_reserved());
    assert_eq!(metadata.len(), LENGTH);
    assert!(metadata.blocks().saturating_mul(512) >= LENGTH);
    assert!(part.allocated_size().unwrap() >= LENGTH);
}

#[cfg(windows)]
#[test]
fn windows_preallocation_marks_sparse_and_reserves_physical_clusters() {
    use std::os::windows::fs::MetadataExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;

    const LENGTH: u64 = 8 * 1024 * 1024;
    let directory = TestDirectory::new("windows-preallocation");
    let target = directory.path().join("reserved.bin");
    let part = PartFile::create(&target, LENGTH).unwrap();
    let metadata = fs::metadata(part.path()).unwrap();
    let allocated_size = part.allocated_size().unwrap();

    assert_eq!(
        part.preallocation_method(),
        PreallocationMethod::FileAllocationInfo,
        "post-sparse allocation was only {allocated_size} bytes"
    );
    assert!(part.space_reserved());
    assert_eq!(metadata.file_size(), LENGTH);
    assert_ne!(metadata.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE, 0);
    assert!(allocated_size >= LENGTH);
}

/// A representation with no stated length grows as bytes arrive, and reports no reservation.
///
/// The honest position, not a degraded one: I-10 turns "disk full at 97%" into a start-time error
/// by reserving the full length up front, and a server that never stated a length has given us
/// nothing to reserve. Claiming `space_reserved` here would be claiming a protection that does
/// not exist.
#[test]
fn a_growable_part_file_starts_empty_and_never_claims_reserved_space() {
    let directory = TestDirectory::new("growable");
    let target = directory.path().join("payload.bin");
    let mut part = PartFile::create_growable(&target).expect("an unused target is exclusive");

    assert_eq!(part.total_length(), 0);
    assert!(!part.space_reserved());
    assert_eq!(part.preallocation_method(), PreallocationMethod::SetLength);
    assert!(matches!(
        part.write_all_at(0, b"x"),
        Err(PartFileError::OutOfBounds { .. })
    ));

    part.extend_to(4).expect("growing is allowed");
    part.write_all_at(0, b"abcd").expect("the extent now fits");
    part.extend_to(8).expect("growing again is allowed");
    part.write_all_at(4, b"efgh").expect("the new extent fits");
    part.sync_data().expect("data reaches stable storage");

    assert_eq!(fs::read(part.path()).unwrap(), b"abcdefgh");
    assert_eq!(part.total_length(), 8);
}

/// I-10: a growable extent never shrinks, because shrinking destroys resumable bytes.
#[test]
fn a_growable_part_file_refuses_to_shrink() {
    let directory = TestDirectory::new("growable-shrink");
    let target = directory.path().join("payload.bin");
    let mut part = PartFile::create_growable(&target).expect("exclusive");
    part.extend_to(16).expect("grow");
    part.write_all_at(0, b"0123456789abcdef").expect("fill");

    assert!(matches!(
        part.extend_to(8),
        Err(PartFileError::CannotShrink {
            current: 16,
            requested: 8
        })
    ));
    assert_eq!(
        fs::read(part.path()).unwrap().len(),
        16,
        "the refusal must leave every byte in place"
    );
    part.extend_to(16)
        .expect("re-stating the same extent is a no-op");
}

/// The exclusive create applies to the growable constructor too: a collision is another owner.
#[test]
fn a_growable_part_file_creation_is_exclusive() {
    let directory = TestDirectory::new("growable-exclusive");
    let target = directory.path().join("payload.bin");
    let _first = PartFile::create_growable(&target).expect("first owner wins");
    assert!(matches!(
        PartFile::create_growable(&target),
        Err(PartFileError::AlreadyExists { .. })
    ));
}

/// B-37 — a part file reached through a symlink or reparse point is refused, not written through.
///
/// The hazard is not hypothetical arithmetic: recovery reopens the recorded part path read-write
/// and a resumed transfer writes into it at recorded offsets. If that path is a link to something
/// the user cares about, the download silently overwrites the victim. Nothing about the resumed
/// download looks wrong while it happens.
///
/// Two properties matter and only one of them is "an error was returned". The other is that the
/// victim's bytes are untouched, because a refusal that arrives after the first `pwrite` is not a
/// refusal. The open is therefore no-follow at the syscall, not a check followed by an open: a
/// check-then-open leaves a window in which the path can be replaced.
#[test]
#[cfg(unix)]
fn a_part_path_that_is_a_symlink_is_refused_and_its_target_is_untouched() {
    let directory = TestDirectory::new("nofollow-symlink");
    let dir = directory.path();
    let victim_path = dir.join("something-the-user-cares-about");
    let victim_bytes = b"the user's data, which this download does not own".to_vec();
    std::fs::write(&victim_path, &victim_bytes).expect("write the victim");
    let part_path = dir.join("download.dppart");
    std::os::unix::fs::symlink(&victim_path, &part_path).expect("plant the symlink");

    let error = PartFile::open_existing(&part_path, 4096)
        .expect_err("a part path that is a symlink must be refused");
    assert!(
        matches!(error, PartFileError::NotARegularFile { .. }),
        "the refusal must name the reason so recovery can report it: {error:?}"
    );

    assert_eq!(
        std::fs::read(&victim_path).expect("the victim is still readable"),
        victim_bytes,
        "the symlink target was modified"
    );
    assert!(
        std::fs::symlink_metadata(&part_path)
            .expect("the link is still there")
            .file_type()
            .is_symlink(),
        "the link itself must be left alone as evidence"
    );
}

/// The same refusal on Windows, where the mechanism is a reparse point rather than a symlink.
///
/// Creating a symlink on Windows needs either Developer Mode or `SeCreateSymbolicLinkPrivilege`,
/// so this skips rather than fails when it cannot plant one — a test that silently passes because
/// it could not set up its own hazard would be worse than one that says so.
#[test]
#[cfg(windows)]
fn a_part_path_that_is_a_reparse_point_is_refused_and_its_target_is_untouched() {
    let directory = TestDirectory::new("nofollow-reparse");
    let dir = directory.path();
    let victim_path = dir.join("something-the-user-cares-about");
    let victim_bytes = b"the user's data, which this download does not own".to_vec();
    std::fs::write(&victim_path, &victim_bytes).expect("write the victim");
    let part_path = dir.join("download.dppart");
    if std::os::windows::fs::symlink_file(&victim_path, &part_path).is_err() {
        eprintln!("skipping: this account cannot create symbolic links");
        return;
    }

    let error = PartFile::open_existing(&part_path, 4096)
        .expect_err("a part path that is a reparse point must be refused");
    assert!(
        matches!(error, PartFileError::NotARegularFile { .. }),
        "the refusal must name the reason so recovery can report it: {error:?}"
    );
    assert_eq!(
        std::fs::read(&victim_path).expect("the victim is still readable"),
        victim_bytes,
        "the reparse-point target was modified"
    );
}
