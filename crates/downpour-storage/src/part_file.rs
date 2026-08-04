//! Exclusive, preallocated part files with cursor-independent positional writes.
//!
//! This module prepares the on-disk target required by I-10 and rejects writes beyond the
//! file's declared extent as a storage-side backstop for I-2. It does not decide grant
//! ownership or commit durability; the S2 durable writer owns those responsibilities.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// The platform operation that established the part file's initial extent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreallocationMethod {
    /// The remote object is empty, so there are no bytes whose space needs reserving.
    NotNeeded,
    /// Linux `fallocate` with `FALLOC_FL_KEEP_SIZE` reserved the full extent.
    FallocateKeepSize,
    /// POSIX `posix_fallocate` reserved the full extent after `fallocate` was unsupported.
    PosixFallocate,
    /// Windows `FileAllocationInfo` reserved the full extent and the allocation query confirmed it.
    FileAllocationInfo,
    /// Only the logical length could be established; physical space is not guaranteed.
    SetLength,
}

impl PreallocationMethod {
    /// Whether this method guarantees that every byte in the declared extent has space reserved.
    #[must_use]
    pub const fn space_reserved(self) -> bool {
        !matches!(self, Self::SetLength)
    }
}

/// An exclusively owned `.dppart` prepared for positional writes.
#[derive(Debug)]
pub struct PartFile {
    file: File,
    path: PathBuf,
    total_length: u64,
    preallocation_method: PreallocationMethod,
}

impl PartFile {
    /// Create `<target>.dppart` exclusively and prepare its full logical extent.
    ///
    /// An existing file or symbolic link is an ownership collision and is never opened or
    /// truncated. Operational allocation errors such as `ENOSPC` are returned rather than
    /// hidden behind the non-reserving fallback.
    pub fn create(target: impl AsRef<Path>, total_length: u64) -> Result<Self, PartFileError> {
        let target = target.as_ref();
        let signed_length =
            i64::try_from(total_length).map_err(|_| PartFileError::UnsupportedLength {
                length: total_length,
            })?;
        let path = part_path_for(target)?;
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(PartFileError::AlreadyExists { path });
            }
            Err(source) => {
                return Err(PartFileError::Io {
                    operation: "create part file exclusively",
                    path,
                    source,
                });
            }
        };

        let preallocation_method =
            platform::prepare(&file, total_length, signed_length).map_err(|source| {
                PartFileError::Io {
                    operation: "prepare part-file extent",
                    path: path.clone(),
                    source,
                }
            })?;

        Ok(Self {
            file,
            path,
            total_length,
            preallocation_method,
        })
    }

    /// Reopen an existing `.dppart` after an unclean shutdown and re-establish its extent.
    ///
    /// Unlike [`Self::create`] this takes the part path itself, because recovery reads it from
    /// the persisted record rather than deriving it from a target.
    ///
    /// The extent is only ever *grown*. A file longer than `total_length` keeps every byte:
    /// shortening it to make it fit would destroy the bytes a resume is meant to reuse, which
    /// I-10 forbids as firmly as it forbids truncating on `ENOSPC`. Callers get the length
    /// observed before preparation so they can refuse to trust journal records describing bytes
    /// past the end that actually survived.
    pub fn open_existing(
        path: impl AsRef<Path>,
        total_length: u64,
    ) -> Result<RecoveredPartFile, PartFileError> {
        let path = path.as_ref().to_path_buf();
        let signed_length =
            i64::try_from(total_length).map_err(|_| PartFileError::UnsupportedLength {
                length: total_length,
            })?;
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(PartFileError::Missing { path });
            }
            Err(source) => {
                return Err(PartFileError::Io {
                    operation: "reopen part file for recovery",
                    path,
                    source,
                });
            }
        };
        let observed_length = file
            .metadata()
            .map_err(|source| PartFileError::Io {
                operation: "measure recovered part-file extent",
                path: path.clone(),
                source,
            })?
            .len();

        // Preparation ends in `set_len`, which would shorten an overlong file. Only run it when
        // the extent is genuinely short.
        let (preallocation_method, reextended) = if observed_length < total_length {
            let method =
                platform::prepare(&file, total_length, signed_length).map_err(|source| {
                    PartFileError::Io {
                        operation: "restore part-file extent",
                        path: path.clone(),
                        source,
                    }
                })?;
            (method, true)
        } else {
            (PreallocationMethod::SetLength, false)
        };

        let part = Self {
            file,
            path,
            total_length,
            preallocation_method,
        };
        // Without a fresh preparation there is no claim to make about reservation, so measure it
        // instead of assuming it. A crash can leave a sparse file whose blocks were never
        // reserved, and pretending otherwise is how `ENOSPC` becomes a surprise mid-resume.
        let space_reserved = if reextended {
            preallocation_method.space_reserved()
        } else {
            part.allocated_size()? >= total_length
        };

        Ok(RecoveredPartFile {
            part,
            observed_length,
            reextended,
            space_reserved,
        })
    }

    /// The path of the exclusively created `.dppart`.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The exact logical extent accepted by [`Self::write_all_at`].
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Which platform preparation method succeeded.
    #[must_use]
    pub const fn preallocation_method(&self) -> PreallocationMethod {
        self.preallocation_method
    }

    /// Whether the complete declared extent is known to have physical space reserved.
    #[must_use]
    pub const fn space_reserved(&self) -> bool {
        self.preallocation_method.space_reserved()
    }

    /// Query how many physical bytes the filesystem currently reports for this file.
    pub fn allocated_size(&self) -> Result<u64, PartFileError> {
        platform::allocated_size(&self.file).map_err(|source| PartFileError::Io {
            operation: "query part-file allocation",
            path: self.path.clone(),
            source,
        })
    }

    /// Write all of `bytes` at `offset` without using or changing a shared seek cursor.
    ///
    /// The full write is rejected before I/O if it would cross the declared extent. This is a
    /// storage-side bound only; the durable writer additionally validates the allocator grant.
    pub fn write_all_at(&self, offset: u64, bytes: &[u8]) -> Result<(), PartFileError> {
        let length = u64::try_from(bytes.len()).map_err(|_| PartFileError::BufferTooLarge)?;
        let in_bounds = offset
            .checked_add(length)
            .is_some_and(|end| end <= self.total_length);
        if !in_bounds {
            return Err(PartFileError::OutOfBounds {
                offset,
                length,
                total_length: self.total_length,
            });
        }

        platform::write_all_at(&self.file, offset, bytes).map_err(|source| PartFileError::Io {
            operation: "write part-file bytes positionally",
            path: self.path.clone(),
            source,
        })
    }

    /// Force all previously written part-file data to stable storage.
    ///
    /// The durable writer calls this before it appends any `BlockComplete` record. Metadata not
    /// needed to retrieve the bytes may remain unsynchronised, matching `fdatasync` on Linux and
    /// `FlushFileBuffers` on Windows.
    pub fn sync_data(&self) -> Result<(), PartFileError> {
        self.file.sync_data().map_err(|source| PartFileError::Io {
            operation: "synchronise part-file data",
            path: self.path.clone(),
            source,
        })
    }
}

/// An existing part file reopened for recovery.
///
/// Carries the extent observed *before* preparation, which is the only evidence available about
/// which journalled bytes actually survived the crash.
#[derive(Debug)]
pub struct RecoveredPartFile {
    part: PartFile,
    observed_length: u64,
    reextended: bool,
    space_reserved: bool,
}

impl RecoveredPartFile {
    /// The file extent as it was found, before any extent was restored.
    #[must_use]
    pub const fn observed_length(&self) -> u64 {
        self.observed_length
    }

    /// Whether the extent had to be grown back to the representation length.
    #[must_use]
    pub const fn reextended(&self) -> bool {
        self.reextended
    }

    /// Whether the full declared extent is now known to have physical space reserved.
    #[must_use]
    pub const fn space_reserved(&self) -> bool {
        self.space_reserved
    }

    /// Borrow the reopened part file.
    #[must_use]
    pub const fn part(&self) -> &PartFile {
        &self.part
    }

    /// Take the reopened part file, discarding the recovery observations.
    #[must_use]
    pub fn into_part(self) -> PartFile {
        self.part
    }
}

/// Why a part file could not be created, prepared, inspected, or written.
#[derive(Debug, Error)]
pub enum PartFileError {
    /// The recorded part path does not exist, so its bytes are gone.
    #[error("part file is missing: {path}", path = .path.display())]
    Missing {
        /// The recorded part path that no longer resolves.
        path: PathBuf,
    },
    /// The target has no filename to which `.dppart` can be appended.
    #[error("target path has no filename: {target}", target = .target.display())]
    InvalidTarget {
        /// The unusable target path.
        target: PathBuf,
    },
    /// The requested extent cannot fit the signed 64-bit offsets used by both supported OSes.
    #[error("part-file length {length} cannot be represented by the platform file APIs")]
    UnsupportedLength {
        /// The rejected extent.
        length: u64,
    },
    /// The derived `.dppart` already exists, so another owner or recovery artifact wins.
    #[error("part file already exists: {path}", path = .path.display())]
    AlreadyExists {
        /// The colliding part path.
        path: PathBuf,
    },
    /// A complete positional write would cross the declared extent.
    #[error(
        "write [{offset}, {end}) is outside part-file extent [0, {total_length})",
        end = offset.saturating_add(*length)
    )]
    OutOfBounds {
        /// Requested starting offset.
        offset: u64,
        /// Requested byte count.
        length: u64,
        /// The only accepted extent.
        total_length: u64,
    },
    /// The in-memory buffer length did not fit in the `u64` offset domain.
    #[error("write buffer length cannot be represented as u64")]
    BufferTooLarge,
    /// A filesystem operation failed.
    #[error("could not {operation} at {path}: {source}", path = .path.display())]
    Io {
        /// The operation whose result could not be ignored.
        operation: &'static str,
        /// The part path involved.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: io::Error,
    },
}

fn part_path_for(target: &Path) -> Result<PathBuf, PartFileError> {
    if target.file_name().is_none() {
        return Err(PartFileError::InvalidTarget {
            target: target.to_path_buf(),
        });
    }
    let mut path = OsString::from(target.as_os_str());
    path.push(".dppart");
    Ok(PathBuf::from(path))
}

#[cfg(target_os = "linux")]
mod platform {
    #![allow(unsafe_code)]

    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileExt, MetadataExt};

    use rustix::fs::{FallocateFlags, fallocate};

    use super::PreallocationMethod;

    trait AllocationOperations {
        fn fallocate_keep_size(&self) -> io::Result<()>;
        fn posix_fallocate(&self) -> io::Result<()>;
        fn set_len(&self) -> io::Result<()>;
    }

    struct SystemAllocation<'a> {
        file: &'a File,
        length: u64,
        signed_length: i64,
    }

    impl AllocationOperations for SystemAllocation<'_> {
        fn fallocate_keep_size(&self) -> io::Result<()> {
            fallocate(self.file, FallocateFlags::KEEP_SIZE, 0, self.length)
                .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
        }

        fn posix_fallocate(&self) -> io::Result<()> {
            // SAFETY: `self.file` owns a live regular-file descriptor for the duration of the
            // call. Both offsets are non-negative, and `signed_length` was checked from `u64`
            // before the part path was created.
            let result =
                unsafe { libc::posix_fallocate(self.file.as_raw_fd(), 0, self.signed_length) };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(result))
            }
        }

        fn set_len(&self) -> io::Result<()> {
            self.file.set_len(self.length)
        }
    }

    pub(super) fn prepare(
        file: &File,
        length: u64,
        signed_length: i64,
    ) -> io::Result<PreallocationMethod> {
        if length == 0 {
            file.set_len(0)?;
            return Ok(PreallocationMethod::NotNeeded);
        }
        prepare_with(&SystemAllocation {
            file,
            length,
            signed_length,
        })
    }

    fn prepare_with(operations: &impl AllocationOperations) -> io::Result<PreallocationMethod> {
        match operations.fallocate_keep_size() {
            Ok(()) => {
                operations.set_len()?;
                return Ok(PreallocationMethod::FallocateKeepSize);
            }
            Err(error) if is_unsupported(&error) => {}
            Err(error) => return Err(error),
        }

        match operations.posix_fallocate() {
            Ok(()) => {
                operations.set_len()?;
                Ok(PreallocationMethod::PosixFallocate)
            }
            Err(error) if is_unsupported(&error) => {
                operations.set_len()?;
                Ok(PreallocationMethod::SetLength)
            }
            Err(error) => Err(error),
        }
    }

    fn is_unsupported(error: &io::Error) -> bool {
        error.raw_os_error().is_some_and(|code| {
            code == libc::EOPNOTSUPP || code == libc::ENOSYS || code == libc::EINVAL
        })
    }

    pub(super) fn allocated_size(file: &File) -> io::Result<u64> {
        Ok(file.metadata()?.blocks().saturating_mul(512))
    }

    pub(super) fn write_all_at(file: &File, offset: u64, bytes: &[u8]) -> io::Result<()> {
        FileExt::write_all_at(file, bytes, offset)
    }

    #[cfg(test)]
    mod tests {
        use std::cell::RefCell;

        use super::*;

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum Event {
            Fallocate,
            PosixFallocate,
            SetLength,
        }

        #[derive(Clone, Copy)]
        enum Outcome {
            Success,
            Unsupported,
            NoSpace,
        }

        struct FakeAllocation {
            fallocate: Outcome,
            posix: Outcome,
            events: RefCell<Vec<Event>>,
        }

        impl FakeAllocation {
            fn result(outcome: Outcome) -> io::Result<()> {
                match outcome {
                    Outcome::Success => Ok(()),
                    Outcome::Unsupported => Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
                    Outcome::NoSpace => Err(io::Error::from_raw_os_error(libc::ENOSPC)),
                }
            }
        }

        impl AllocationOperations for FakeAllocation {
            fn fallocate_keep_size(&self) -> io::Result<()> {
                self.events.borrow_mut().push(Event::Fallocate);
                Self::result(self.fallocate)
            }

            fn posix_fallocate(&self) -> io::Result<()> {
                self.events.borrow_mut().push(Event::PosixFallocate);
                Self::result(self.posix)
            }

            fn set_len(&self) -> io::Result<()> {
                self.events.borrow_mut().push(Event::SetLength);
                Ok(())
            }
        }

        fn fake(fallocate: Outcome, posix: Outcome) -> FakeAllocation {
            FakeAllocation {
                fallocate,
                posix,
                events: RefCell::new(Vec::new()),
            }
        }

        #[test]
        fn fallocate_success_sets_length_without_trying_a_weaker_reservation() {
            let operations = fake(Outcome::Success, Outcome::NoSpace);

            let method = prepare_with(&operations).unwrap();

            assert_eq!(method, PreallocationMethod::FallocateKeepSize);
            assert_eq!(
                *operations.events.borrow(),
                [Event::Fallocate, Event::SetLength]
            );
        }

        #[test]
        fn unsupported_fallocate_uses_posix_fallocate_before_logical_length() {
            let operations = fake(Outcome::Unsupported, Outcome::Success);

            let method = prepare_with(&operations).unwrap();

            assert_eq!(method, PreallocationMethod::PosixFallocate);
            assert_eq!(
                *operations.events.borrow(),
                [Event::Fallocate, Event::PosixFallocate, Event::SetLength]
            );
        }

        #[test]
        fn unsupported_reservation_falls_back_truthfully_to_logical_length() {
            let operations = fake(Outcome::Unsupported, Outcome::Unsupported);

            let method = prepare_with(&operations).unwrap();

            assert_eq!(method, PreallocationMethod::SetLength);
            assert!(!method.space_reserved());
            assert_eq!(
                *operations.events.borrow(),
                [Event::Fallocate, Event::PosixFallocate, Event::SetLength]
            );
        }

        #[test]
        fn no_space_is_not_hidden_by_a_weaker_fallback() {
            for operations in [
                fake(Outcome::NoSpace, Outcome::Success),
                fake(Outcome::Unsupported, Outcome::NoSpace),
            ] {
                let error = prepare_with(&operations).unwrap_err();
                assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                assert!(!operations.events.borrow().contains(&Event::SetLength));
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::fs::FileExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ALLOCATION_INFO, FILE_STANDARD_INFO, FileAllocationInfo, FileStandardInfo,
        GetFileInformationByHandleEx, SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    use super::PreallocationMethod;

    trait AllocationOperations {
        fn mark_sparse(&self) -> io::Result<()>;
        fn request_allocation(&self) -> io::Result<()>;
        fn set_len(&self) -> io::Result<()>;
        fn allocated_size(&self) -> io::Result<u64>;
    }

    struct SystemAllocation<'a> {
        file: &'a File,
        length: u64,
        signed_length: i64,
    }

    impl AllocationOperations for SystemAllocation<'_> {
        fn mark_sparse(&self) -> io::Result<()> {
            let mut bytes_returned = 0;
            // SAFETY: the handle is a live exclusively created regular file. This control code
            // takes no input or output buffer; all null pointers and zero lengths therefore match
            // the documented FSCTL_SET_SPARSE contract. `bytes_returned` lives through the call.
            let result = unsafe {
                DeviceIoControl(
                    self.file.as_raw_handle(),
                    FSCTL_SET_SPARSE,
                    null(),
                    0,
                    null_mut(),
                    0,
                    &mut bytes_returned,
                    null_mut(),
                )
            };
            bool_result(result)
        }

        fn request_allocation(&self) -> io::Result<()> {
            let info = FILE_ALLOCATION_INFO {
                AllocationSize: self.signed_length,
            };
            let size = structure_size::<FILE_ALLOCATION_INFO>()?;
            // SAFETY: `info` has the exact representation and size required by
            // FileAllocationInfo and remains alive for the synchronous call. The handle is a
            // writable regular file owned by this PartFile.
            let result = unsafe {
                SetFileInformationByHandle(
                    self.file.as_raw_handle(),
                    FileAllocationInfo,
                    (&raw const info).cast::<c_void>(),
                    size,
                )
            };
            bool_result(result)
        }

        fn set_len(&self) -> io::Result<()> {
            self.file.set_len(self.length)
        }

        fn allocated_size(&self) -> io::Result<u64> {
            query_allocated_size(self.file)
        }
    }

    pub(super) fn prepare(
        file: &File,
        length: u64,
        signed_length: i64,
    ) -> io::Result<PreallocationMethod> {
        if length == 0 {
            file.set_len(0)?;
            return Ok(PreallocationMethod::NotNeeded);
        }
        prepare_with(
            &SystemAllocation {
                file,
                length,
                signed_length,
            },
            length,
        )
    }

    fn prepare_with(
        operations: &impl AllocationOperations,
        length: u64,
    ) -> io::Result<PreallocationMethod> {
        match operations.request_allocation() {
            Ok(()) => {
                operations.set_len()?;
                if let Err(error) = operations.mark_sparse()
                    && !is_unsupported(&error)
                {
                    return Err(error);
                }
                if operations.allocated_size()? >= length {
                    Ok(PreallocationMethod::FileAllocationInfo)
                } else {
                    Ok(PreallocationMethod::SetLength)
                }
            }
            Err(error) if is_unsupported(&error) => {
                if let Err(error) = operations.mark_sparse()
                    && !is_unsupported(&error)
                {
                    return Err(error);
                }
                operations.set_len()?;
                Ok(PreallocationMethod::SetLength)
            }
            Err(error) => Err(error),
        }
    }

    fn is_unsupported(error: &io::Error) -> bool {
        error.raw_os_error().is_some_and(|raw| {
            u32::try_from(raw).is_ok_and(|code| {
                matches!(
                    code,
                    ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER
                )
            })
        })
    }

    fn bool_result(result: i32) -> io::Result<()> {
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn structure_size<T>() -> io::Result<u32> {
        u32::try_from(size_of::<T>())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Win32 structure too large"))
    }

    fn query_allocated_size(file: &File) -> io::Result<u64> {
        let mut info = FILE_STANDARD_INFO::default();
        let size = structure_size::<FILE_STANDARD_INFO>()?;
        // SAFETY: `info` is a writable FILE_STANDARD_INFO with the exact size passed to the
        // synchronous call and lives until it returns. The file handle remains valid throughout.
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        bool_result(result)?;
        u64::try_from(info.AllocationSize).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows reported a negative file allocation size",
            )
        })
    }

    pub(super) fn allocated_size(file: &File) -> io::Result<u64> {
        query_allocated_size(file)
    }

    pub(super) fn write_all_at(file: &File, offset: u64, mut bytes: &[u8]) -> io::Result<()> {
        let mut written = 0_u64;
        while !bytes.is_empty() {
            let current_offset = offset.checked_add(written).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "positional write offset overflow",
                )
            })?;
            let count = FileExt::seek_write(file, bytes, current_offset)?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write the complete positional buffer",
                ));
            }
            bytes = &bytes[count..];
            let count = u64::try_from(count).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "write count does not fit u64")
            })?;
            written = written.checked_add(count).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "written byte count overflow")
            })?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::cell::RefCell;

        use windows_sys::Win32::Foundation::{ERROR_DISK_FULL, ERROR_NOT_SUPPORTED};

        use super::*;

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum Event {
            Sparse,
            Allocate,
            SetLength,
            Query,
        }

        struct FakeAllocation {
            sparse_error: Option<i32>,
            allocation_error: Option<i32>,
            allocated_size: u64,
            events: RefCell<Vec<Event>>,
        }

        impl AllocationOperations for FakeAllocation {
            fn mark_sparse(&self) -> io::Result<()> {
                self.events.borrow_mut().push(Event::Sparse);
                error_or_success(self.sparse_error)
            }

            fn request_allocation(&self) -> io::Result<()> {
                self.events.borrow_mut().push(Event::Allocate);
                error_or_success(self.allocation_error)
            }

            fn set_len(&self) -> io::Result<()> {
                self.events.borrow_mut().push(Event::SetLength);
                Ok(())
            }

            fn allocated_size(&self) -> io::Result<u64> {
                self.events.borrow_mut().push(Event::Query);
                Ok(self.allocated_size)
            }
        }

        fn error_or_success(code: Option<i32>) -> io::Result<()> {
            code.map_or(Ok(()), |code| Err(io::Error::from_raw_os_error(code)))
        }

        fn code(value: u32) -> i32 {
            i32::try_from(value).unwrap()
        }

        fn fake(
            sparse_error: Option<u32>,
            allocation_error: Option<u32>,
            allocated_size: u64,
        ) -> FakeAllocation {
            FakeAllocation {
                sparse_error: sparse_error.map(code),
                allocation_error: allocation_error.map(code),
                allocated_size,
                events: RefCell::new(Vec::new()),
            }
        }

        #[test]
        fn verified_allocation_is_the_only_reserving_windows_result() {
            let operations = fake(None, None, 4096);
            assert_eq!(
                prepare_with(&operations, 4096).unwrap(),
                PreallocationMethod::FileAllocationInfo
            );
            assert_eq!(
                *operations.events.borrow(),
                [
                    Event::Allocate,
                    Event::SetLength,
                    Event::Sparse,
                    Event::Query
                ]
            );

            let short = fake(None, None, 2048);
            let method = prepare_with(&short, 4096).unwrap();
            assert_eq!(method, PreallocationMethod::SetLength);
            assert!(!method.space_reserved());
        }

        #[test]
        fn unsupported_windows_features_fall_back_without_claiming_reservation() {
            let operations = fake(Some(ERROR_NOT_SUPPORTED), Some(ERROR_NOT_SUPPORTED), 0);

            let method = prepare_with(&operations, 4096).unwrap();

            assert_eq!(method, PreallocationMethod::SetLength);
            assert!(!method.space_reserved());
            assert_eq!(
                *operations.events.borrow(),
                [Event::Allocate, Event::Sparse, Event::SetLength]
            );
        }

        #[test]
        fn windows_disk_full_is_not_hidden_at_either_preparation_step() {
            let sparse = fake(Some(ERROR_DISK_FULL), None, 0);

            let error = prepare_with(&sparse, 4096).unwrap_err();

            assert_eq!(error.raw_os_error(), Some(code(ERROR_DISK_FULL)));
            assert_eq!(
                *sparse.events.borrow(),
                [Event::Allocate, Event::SetLength, Event::Sparse]
            );

            let allocation = fake(None, Some(ERROR_DISK_FULL), 0);

            let error = prepare_with(&allocation, 4096).unwrap_err();

            assert_eq!(error.raw_os_error(), Some(code(ERROR_DISK_FULL)));
            assert_eq!(*allocation.events.borrow(), [Event::Allocate]);
        }
    }
}
