//! Native local-socket permissions, provisioning, and authenticated byte flow.

use std::sync::atomic::{AtomicU64, Ordering};

use downpour_ipc::{
    CommandHandler, HelloParams, LocalListener, LocalStream, PROTOCOL_VERSION, Request, Response,
    ResponseKind, SecretString, Session, StateResult, SystemStatus, TransportError, VersionParams,
    WireState, decode_response, encode_request, encode_response, read_client_token,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-ipc-transport-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if self.0.exists() {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

#[tokio::test]
async fn native_endpoint_is_user_scoped_and_authenticates_before_dispatch() {
    tokio::time::timeout(std::time::Duration::from_secs(2), native_endpoint_proof())
        .await
        .expect("native endpoint proof timed out");
}

async fn native_endpoint_proof() {
    let root = TestDirectory::new();
    let listener = LocalListener::bind(&root.0).unwrap();
    let paths = listener.paths().clone();
    let token = listener.session_token();

    assert_eq!(paths.runtime_dir(), root.0.join("downpour"));
    assert!(paths.token_file().is_file());
    assert_native_permissions(&paths);
    let client_token = read_client_token(&paths).unwrap();
    assert!(token.matches_wire(client_token.expose()));
    assert!(LocalListener::bind(&root.0).is_err());
    assert_eq!(
        read_client_token(&paths).unwrap().expose(),
        client_token.expose()
    );

    let server = tokio::spawn(async move {
        let mut handler = CountingHandler::default();
        let mut rejected = listener.accept().await.unwrap();
        let payload = rejected.receive().await.unwrap();
        let mut rejected_session = Session::new(token.clone());
        assert!(
            rejected_session
                .handle_payload(&payload, &mut handler)
                .is_err()
        );
        assert_eq!(handler.calls, 0);
        drop(rejected);

        let mut accepted = listener.accept().await.unwrap();
        let mut session = Session::new(token);
        for _ in 0..2 {
            let payload = accepted.receive().await.unwrap();
            let (id, response) = session.handle_payload(&payload, &mut handler).unwrap();
            accepted
                .send(&encode_response(id, &response).unwrap())
                .await
                .unwrap();
        }
        handler.calls
    });

    let mut rejected = LocalStream::connect(&paths).await.unwrap();
    rejected
        .send(&encode_request(1, &hello("wrong")).unwrap())
        .await
        .unwrap();
    drop(rejected);

    let mut accepted = LocalStream::connect(&paths).await.unwrap();
    accepted
        .send(&encode_request(2, &hello(client_token.expose())).unwrap())
        .await
        .unwrap();
    let hello_response = accepted.receive().await.unwrap();
    assert!(matches!(
        decode_response(&hello_response, ResponseKind::Hello).unwrap(),
        (2, Response::Hello(_))
    ));
    accepted
        .send(
            &encode_request(
                3,
                &Request::SystemStatus(VersionParams {
                    protocol_version: PROTOCOL_VERSION,
                }),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let status_response = accepted.receive().await.unwrap();
    assert!(matches!(
        decode_response(&status_response, ResponseKind::Status).unwrap(),
        (3, Response::Status(_))
    ));
    assert_eq!(server.await.unwrap(), 1);
}

#[tokio::test]
async fn malformed_client_token_file_is_refused() {
    let root = TestDirectory::new();
    let listener = LocalListener::bind(&root.0).unwrap();
    std::fs::write(listener.paths().token_file(), "AA-not-canonical").unwrap();

    assert!(matches!(
        read_client_token(listener.paths()),
        Err(TransportError::InvalidTokenFile)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn existing_broad_or_symlink_runtime_directory_is_refused() {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _, symlink};

    let broad_root = TestDirectory::new();
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(broad_root.0.join("downpour"))
        .unwrap();
    assert!(LocalListener::bind(&broad_root.0).is_err());

    let linked_root = TestDirectory::new();
    let victim = TestDirectory::new();
    std::fs::set_permissions(&victim.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&victim.0, linked_root.0.join("downpour")).unwrap();
    assert!(LocalListener::bind(&linked_root.0).is_err());
    assert!(!victim.0.join("session.token").exists());
    assert!(!victim.0.join("daemon.sock").exists());
}

fn hello(token: &str) -> Request {
    Request::Hello(HelloParams {
        protocol_version: PROTOCOL_VERSION,
        client: "native-test".to_owned(),
        token: SecretString::new(token),
    })
}

#[derive(Default)]
struct CountingHandler {
    calls: usize,
}

impl CommandHandler for CountingHandler {
    fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::SystemStatus(_) => {
                self.calls += 1;
                Response::Status(SystemStatus {
                    protocol_version: PROTOCOL_VERSION,
                    version: "test".to_owned(),
                    uptime_seconds: 1,
                    active: 0,
                    queued: 0,
                    throughput: 0,
                })
            }
            Request::DownloadAdd(_) => Response::Added(StateResult {
                protocol_version: PROTOCOL_VERSION,
                id: downpour_ipc::DownloadId::new("018f0f0f0f0f70008000000000000001").unwrap(),
                state: WireState::Submitted,
            }),
            Request::Hello(_) | Request::DownloadGet(_) | Request::DownloadResume(_) => {
                panic!("native transport proof sent an unexpected request")
            }
        }
    }
}

#[cfg(unix)]
fn assert_native_permissions(paths: &downpour_ipc::EndpointPaths) {
    use std::os::unix::fs::PermissionsExt as _;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    assert_eq!(mode(paths.runtime_dir()), 0o700);
    assert_eq!(mode(paths.token_file()), 0o600);
    assert_eq!(mode(paths.socket_path()), 0o600);
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn assert_native_permissions(paths: &downpour_ipc::EndpointPaths) {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle, RawHandle};
    use std::ptr;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut raw_token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle; raw_token is writable HANDLE storage and
    // the test requests only query access.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) },
        0,
        "{:?}",
        std::io::Error::last_os_error()
    );
    // SAFETY: OpenProcessToken returned one owned handle, closed once by OwnedHandle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token as RawHandle) };
    let mut required = 0_u32;
    // SAFETY: null plus zero length is the documented size query and required is writable.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle() as _,
            TokenUser,
            ptr::null_mut(),
            0,
            &mut required,
        )
    };
    assert!(required > 0, "{:?}", std::io::Error::last_os_error());
    let mut user_buffer = vec![0_u8; usize::try_from(required).unwrap()];
    // SAFETY: the buffer has exactly the requested writable capacity and the token remains open.
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle() as _,
                TokenUser,
                user_buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        },
        0,
        "{:?}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the successful query initialized TOKEN_USER at the buffer start; user_buffer lives
    // through every descriptor comparison below.
    let expected_sid = unsafe { (*user_buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let pipe_path = std::path::PathBuf::from(format!(r"\\.\pipe\{}", paths.pipe_name()));

    for path in [paths.runtime_dir(), paths.token_file(), pipe_path.as_path()] {
        assert_single_current_user_ace(path, expected_sid);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn assert_single_current_user_ace(
    path: &std::path::Path,
    expected_sid: windows_sys::Win32::Security::PSID,
) {
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
        EqualSid, GetAce, GetAclInformation, GetFileSecurityW, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, OWNER_SECURITY_INFORMATION,
        SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let requested = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut required = 0_u32;
    // SAFETY: wide is NUL-terminated, the null descriptor with length zero is the documented
    // size-query form, and required points to writable storage.
    unsafe { GetFileSecurityW(wide.as_ptr(), requested, ptr::null_mut(), 0, &mut required) };
    assert!(required > 0, "{:?}", std::io::Error::last_os_error());
    let mut descriptor = vec![0_u8; usize::try_from(required).unwrap()];
    // SAFETY: descriptor has the exact writable length Windows requested and all pointers remain
    // live for the call.
    assert_ne!(
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                requested,
                descriptor.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        },
        0,
        "{:?}",
        std::io::Error::last_os_error()
    );
    let descriptor_ptr = descriptor.as_mut_ptr().cast();
    let mut owner = ptr::null_mut();
    let mut owner_defaulted = 0;
    // SAFETY: descriptor_ptr references the initialized descriptor buffer and outputs are writable.
    assert_ne!(
        unsafe { GetSecurityDescriptorOwner(descriptor_ptr, &mut owner, &mut owner_defaulted) },
        0,
        "{:?}",
        std::io::Error::last_os_error()
    );
    // SAFETY: both SIDs are live and validated by their respective successful Windows queries.
    assert_ne!(unsafe { EqualSid(owner, expected_sid) }, 0, "{path:?}");

    let mut dacl_present = 0;
    let mut dacl = ptr::null_mut();
    let mut dacl_defaulted = 0;
    // SAFETY: descriptor_ptr is valid and all output pointers are writable.
    assert_ne!(
        unsafe {
            GetSecurityDescriptorDacl(
                descriptor_ptr,
                &mut dacl_present,
                &mut dacl,
                &mut dacl_defaulted,
            )
        },
        0,
        "{:?}",
        std::io::Error::last_os_error()
    );
    assert_ne!(dacl_present, 0, "{path:?}");
    assert!(!dacl.is_null(), "{path:?}");
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl points inside the live descriptor and size is writable at its exact type size.
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>()).unwrap(),
                AclSizeInformation,
            )
        },
        0,
        "{:?}",
        std::io::Error::last_os_error()
    );
    assert_eq!(size.AceCount, 1, "{path:?}");
    let mut ace = ptr::null_mut();
    // SAFETY: the ACL reports exactly one ACE, so index zero exists and ace is writable output.
    assert_ne!(
        unsafe { GetAce(dacl, 0, &mut ace) },
        0,
        "{:?}",
        std::io::Error::last_os_error()
    );
    // SAFETY: GetAce returned a pointer to an ACCESS_ALLOWED_ACE-sized entry; the type check below
    // occurs before reading its SidStart field.
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(u32::from(allowed.Header.AceType), ACCESS_ALLOWED_ACE_TYPE);
    let ace_sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
    // SAFETY: ACCESS_ALLOWED_ACE stores its variable-length SID starting at SidStart; both SIDs
    // remain live for this comparison.
    assert_ne!(unsafe { EqualSid(ace_sid, expected_sid) }, 0, "{path:?}");

    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor_ptr is valid and both scalar outputs are writable.
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor_ptr, &mut control, &mut revision) },
        0,
        "{:?}",
        std::io::Error::last_os_error()
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0, "{path:?}");
}
