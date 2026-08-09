//! Native local-socket permissions, provisioning, and authenticated byte flow.

use std::sync::atomic::{AtomicU64, Ordering};

use downpour_ipc::{
    CommandHandler, HelloParams, LocalListener, LocalStream, PROTOCOL_VERSION, Request, Response,
    ResponseKind, SecretString, Session, StateResult, SystemStatus, VersionParams, WireState,
    decode_response, encode_request, encode_response, read_client_token,
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
fn assert_native_permissions(_paths: &downpour_ipc::EndpointPaths) {
    todo!("native current-user SID DACL assertion is added with the Windows implementation")
}
