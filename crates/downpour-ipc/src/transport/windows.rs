//! Windows current-user SID endpoint and token-file security.

#![allow(unsafe_code)]

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr;

use interprocess::local_socket::tokio::Listener as NativeListener;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Name, ToNsName as _};
use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptor as _, AsSecurityDescriptorExt as _, SecurityDescriptor,
};
use widestring::U16CString;
use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SetFileSecurityW, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_READ, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{EndpointPaths, TransportError};
use crate::SessionToken;

pub(super) fn prepare_paths(runtime_root: &Path) -> Result<EndpointPaths, TransportError> {
    let sid = current_user_sid()?;
    let descriptor = descriptor_for_sid(&sid)?;
    let runtime_dir = runtime_root.join("downpour");
    create_or_secure_directory(&runtime_dir, &descriptor)?;
    Ok(EndpointPaths {
        token_file: runtime_dir.join("session.token"),
        pipe_name: format!("downpour-{sid}"),
        runtime_dir,
    })
}

pub(super) fn create_listener(paths: &EndpointPaths) -> Result<NativeListener, TransportError> {
    let sid = current_user_sid()?;
    let descriptor = descriptor_for_sid(&sid)?;
    Ok(ListenerOptions::new()
        .name(native_name(paths)?)
        .security_descriptor(descriptor)
        .create_tokio()?)
}

pub(super) fn native_name(paths: &EndpointPaths) -> Result<Name<'_>, TransportError> {
    Ok(paths.pipe_name.as_str().to_ns_name::<GenericNamespaced>()?)
}

pub(super) fn provision_token(
    paths: &EndpointPaths,
    token: &SessionToken,
) -> Result<(), TransportError> {
    let sid = current_user_sid()?;
    let descriptor = descriptor_for_sid(&sid)?;
    let nonce = SessionToken::generate().map_err(io::Error::other)?;
    let temporary = paths
        .runtime_dir
        .join(format!(".session.token.{}", nonce.to_wire().expose()));
    let mut file = create_secure_file(&temporary, &descriptor)?;
    file.write_all(token.to_wire().expose().as_bytes())?;
    file.sync_all()?;
    drop(file);

    let temporary_wide = wide_path(&temporary);
    let token_wide = wide_path(&paths.token_file);
    // SAFETY: both buffers are NUL-terminated for the duration of the call and name distinct,
    // validated files inside the already secured runtime directory.
    let installed = unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            token_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if installed == 0 {
        let error = io::Error::last_os_error();
        if let Err(cleanup_error) = fs::remove_file(&temporary) {
            return Err(io::Error::other(format!(
                "token install failed ({error}); temporary cleanup also failed ({cleanup_error})"
            ))
            .into());
        }
        return Err(error.into());
    }
    Ok(())
}

fn current_user_sid() -> io::Result<String> {
    let mut raw_token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a process pseudo-handle; raw_token points to writable
    // HANDLE storage and TOKEN_QUERY is the only requested access.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken succeeded and transferred ownership of a real token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token as RawHandle) };

    let mut required = 0_u32;
    // SAFETY: the null buffer and zero length are the documented size-query form; required is
    // writable and token remains open for the call.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle() as _,
            TokenUser,
            ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u8; usize::try_from(required).map_err(io::Error::other)?];
    // SAFETY: buffer has the exact byte capacity Windows requested and stays live while TOKEN_USER
    // and its contained SID pointer are read.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle() as _,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful TokenUser query initialized a TOKEN_USER at the buffer start.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut string_sid = ptr::null_mut();
    // SAFETY: user.User.Sid is owned by the live query buffer and string_sid is writable output.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut string_sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid_result = copy_local_wide_string(string_sid);
    // SAFETY: ConvertSidToStringSidW allocated string_sid with LocalAlloc on success.
    if !unsafe { LocalFree(string_sid.cast()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    sid_result
}

fn copy_local_wide_string(pointer: *const u16) -> io::Result<String> {
    if pointer.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null SID string",
        ));
    }
    let mut length = 0_usize;
    // SAFETY: ConvertSidToStringSidW promises a readable NUL-terminated allocation.
    while unsafe { *pointer.add(length) } != 0 {
        length = length
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SID string too long"))?;
    }
    // SAFETY: the loop established that exactly length initialized u16 values precede the NUL.
    let slice = unsafe { std::slice::from_raw_parts(pointer, length) };
    String::from_utf16(slice).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a non-UTF-16 SID string",
        )
    })
}

fn descriptor_for_sid(sid: &str) -> io::Result<SecurityDescriptor> {
    let sddl = U16CString::from_str(format!("O:{sid}G:{sid}D:P(A;;GA;;;{sid})"))
        .map_err(io::Error::other)?;
    SecurityDescriptor::deserialize(sddl.as_ucstr())
}

fn create_or_secure_directory(path: &Path, descriptor: &SecurityDescriptor) -> io::Result<()> {
    let wide = wide_path(path);
    let attributes = security_attributes(descriptor)?;
    // SAFETY: wide is NUL-terminated and attributes borrows a live validated descriptor.
    let created = unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) };
    if created == 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        // SAFETY: wide remains NUL-terminated and readable for this call.
        let flags = unsafe { GetFileAttributesW(wide.as_ptr()) };
        if flags == INVALID_FILE_ATTRIBUTES {
            return Err(io::Error::last_os_error());
        }
        if flags & FILE_ATTRIBUTE_DIRECTORY == 0 || flags & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Downpour runtime path is not a real directory",
            ));
        }
        // SAFETY: wide identifies the checked real directory and descriptor is valid and live.
        if unsafe {
            SetFileSecurityW(
                wide.as_ptr(),
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor.as_sd().cast_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn create_secure_file(path: &Path, descriptor: &SecurityDescriptor) -> io::Result<File> {
    let wide = wide_path(path);
    let attributes = security_attributes(descriptor)?;
    // SAFETY: wide is NUL-terminated; attributes borrows a live validated descriptor; CREATE_NEW
    // prevents following or replacing an existing path; the returned handle is checked below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a unique owned handle which File now closes exactly once.
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

fn security_attributes(descriptor: &SecurityDescriptor) -> io::Result<SECURITY_ATTRIBUTES> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .map_err(io::Error::other)?,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);
    Ok(attributes)
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
