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

    assert_eq!(
        part.preallocation_method(),
        PreallocationMethod::FileAllocationInfo
    );
    assert!(part.space_reserved());
    assert_eq!(metadata.file_size(), LENGTH);
    assert_ne!(metadata.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE, 0);
    assert!(part.allocated_size().unwrap() >= LENGTH);
}
