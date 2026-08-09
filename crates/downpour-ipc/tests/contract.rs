//! Public versioning, framing, authentication, and strict-dispatch contract.

use std::fmt::Write as _;

use downpour_ipc::{
    AddOptions, AddParams, CodecError, CommandHandler, DownloadId, DownloadView, ErrorData,
    HelloParams, HelloResult, IdParams, MAX_FRAME_BYTES, PROTOCOL_VERSION, Request, Response,
    ResponseKind, RpcError, Session, SessionError, SessionToken, StateResult, SystemStatus,
    VersionParams, WireState, decode_request, decode_response, encode_request, encode_response,
    read_frame,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn id() -> DownloadId {
    DownloadId::new("018f0f0f0f0f70008000000000000001").unwrap()
}

fn requests(token: String) -> Vec<Request> {
    vec![
        Request::Hello(HelloParams {
            protocol_version: PROTOCOL_VERSION,
            client: "dp/0.1.0".to_owned(),
            token,
        }),
        Request::DownloadAdd(AddParams {
            protocol_version: PROTOCOL_VERSION,
            url: "https://example.test/file.bin?secret=never-log".to_owned(),
            target: Some("file.bin".to_owned()),
            options: AddOptions {
                connections: Some(4),
            },
        }),
        Request::DownloadGet(IdParams {
            protocol_version: PROTOCOL_VERSION,
            id: id(),
        }),
        Request::DownloadResume(IdParams {
            protocol_version: PROTOCOL_VERSION,
            id: id(),
        }),
        Request::SystemStatus(VersionParams {
            protocol_version: PROTOCOL_VERSION,
        }),
    ]
}

fn responses() -> Vec<(ResponseKind, Response)> {
    vec![
        (
            ResponseKind::Hello,
            Response::Hello(HelloResult {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".to_owned(),
                capabilities: vec!["h1-segments".to_owned()],
            }),
        ),
        (
            ResponseKind::Added,
            Response::Added(StateResult {
                protocol_version: PROTOCOL_VERSION,
                id: id(),
                state: WireState::Submitted,
            }),
        ),
        (
            ResponseKind::Download,
            Response::Download(DownloadView {
                protocol_version: PROTOCOL_VERSION,
                id: id(),
                state: WireState::Transferring,
                covered: 32,
                total: Some(96),
            }),
        ),
        (
            ResponseKind::Resumed,
            Response::Resumed(StateResult {
                protocol_version: PROTOCOL_VERSION,
                id: id(),
                state: WireState::Transferring,
            }),
        ),
        (
            ResponseKind::Status,
            Response::Status(SystemStatus {
                protocol_version: PROTOCOL_VERSION,
                version: "0.1.0".to_owned(),
                uptime_seconds: 7,
                active: 1,
                queued: 2,
                throughput: 4096,
            }),
        ),
        (
            ResponseKind::Download,
            Response::Error(RpcError {
                code: -32001,
                message: "download not found".to_owned(),
                data: ErrorData {
                    protocol_version: PROTOCOL_VERSION,
                    kind: "download_not_found".to_owned(),
                    download_id: Some(id()),
                    recoverable: false,
                    suggestion: None,
                },
            }),
        ),
    ]
}

#[test]
fn every_s3_request_result_and_error_round_trips_with_its_version() {
    for (index, request) in requests("00".repeat(32)).into_iter().enumerate() {
        let request_id = u64::try_from(index + 1).unwrap();
        let frame = encode_request(request_id, &request).unwrap();
        assert_eq!(
            frame.len() - 4,
            usize::try_from(u32::from_le_bytes(frame[..4].try_into().unwrap())).unwrap()
        );
        assert_eq!(decode_request(&frame[4..]).unwrap(), (request_id, request));
    }
    for (index, (kind, response)) in responses().into_iter().enumerate() {
        let request_id = u64::try_from(index + 20).unwrap();
        let frame = encode_response(request_id, &response).unwrap();
        assert_eq!(
            decode_response(&frame[4..], kind).unwrap(),
            (request_id, response)
        );
    }
}

#[tokio::test]
async fn oversized_length_is_rejected_after_only_the_four_byte_header() {
    let (mut sender, mut receiver) = tokio::io::duplex(16);
    let declared = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
    sender.write_all(&declared.to_le_bytes()).await.unwrap();
    sender.write_u8(0x7a).await.unwrap();

    let error = read_frame(&mut receiver).await.unwrap_err();
    assert!(matches!(
        error,
        CodecError::FrameTooLarge {
            declared,
            maximum: MAX_FRAME_BYTES
        } if declared == MAX_FRAME_BYTES + 1
    ));
    assert_eq!(receiver.read_u8().await.unwrap(), 0x7a);
}

#[derive(Default)]
struct CountingHandler {
    calls: Vec<&'static str>,
}

impl CommandHandler for CountingHandler {
    fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::DownloadAdd(_) => {
                self.calls.push("add");
                Response::Added(StateResult {
                    protocol_version: PROTOCOL_VERSION,
                    id: id(),
                    state: WireState::Submitted,
                })
            }
            Request::DownloadGet(_) => {
                self.calls.push("get");
                Response::Download(DownloadView {
                    protocol_version: PROTOCOL_VERSION,
                    id: id(),
                    state: WireState::Paused,
                    covered: 0,
                    total: None,
                })
            }
            Request::DownloadResume(_) => {
                self.calls.push("resume");
                Response::Resumed(StateResult {
                    protocol_version: PROTOCOL_VERSION,
                    id: id(),
                    state: WireState::Transferring,
                })
            }
            Request::SystemStatus(_) => {
                self.calls.push("status");
                Response::Status(SystemStatus {
                    protocol_version: PROTOCOL_VERSION,
                    version: "0.1.0".to_owned(),
                    uptime_seconds: 0,
                    active: 0,
                    queued: 0,
                    throughput: 0,
                })
            }
            Request::Hello(_) => panic!("hello must be consumed by the session boundary"),
        }
    }
}

fn payload(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

#[test]
fn hello_authenticates_once_then_each_s3_command_reaches_the_typed_dispatcher() {
    let raw_token = [0x5a; 32];
    let mut token_text = String::with_capacity(64);
    for byte in raw_token {
        write!(&mut token_text, "{byte:02x}").unwrap();
    }
    let mut session = Session::new(SessionToken::from_bytes(raw_token));
    let mut handler = CountingHandler::default();
    let hello = payload(json!({
        "jsonrpc": "2.0", "id": 1, "method": "hello",
        "params": {"protocol_version": 1, "client": "dp/0.1.0", "token": token_text}
    }));
    let (_, response) = session.handle_payload(&hello, &mut handler).unwrap();
    assert!(matches!(response, Response::Hello(_)));
    assert!(handler.calls.is_empty());

    for (index, request) in requests("unused".to_owned())
        .into_iter()
        .skip(1)
        .enumerate()
    {
        let frame = encode_request(u64::try_from(index + 2).unwrap(), &request).unwrap();
        session.handle_payload(&frame[4..], &mut handler).unwrap();
    }
    assert_eq!(handler.calls, ["add", "get", "resume", "status"]);
}

#[test]
fn malformed_unauthenticated_and_newer_messages_never_dispatch() {
    let token = SessionToken::from_bytes([0x11; 32]);
    let mut handler = CountingHandler::default();
    let mut session = Session::new(token.clone());
    assert!(matches!(
        session.handle_payload(b"not-json", &mut handler),
        Err(SessionError::Codec(CodecError::InvalidMessage))
    ));
    let mut session = Session::new(token.clone());
    assert!(matches!(
        session.handle_payload(
            &payload(json!({"jsonrpc":"2.0","id":1,"method":"download.get","params":{"protocol_version":1,"id":"018f0f0f0f0f70008000000000000001"}})),
            &mut handler,
        ),
        Err(SessionError::AuthenticationRequired)
    ));
    let mut session = Session::new(token.clone());
    assert!(matches!(
        session.handle_payload(
            &payload(json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":1,"client":"dp","token":"11".repeat(32),"extra":true}})),
            &mut handler,
        ),
        Err(SessionError::Codec(CodecError::InvalidMessage))
    ));
    let mut session = Session::new(token.clone());
    assert!(matches!(
        session.handle_payload(
            &payload(json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":1,"client":"dp","token":"wrong"}})),
            &mut handler,
        ),
        Err(SessionError::AuthenticationFailed)
    ));
    let mut session = Session::new(token);
    assert!(matches!(
        session.handle_payload(
            &payload(json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":2,"client":"dp","token":"11".repeat(32)}})),
            &mut handler,
        ),
        Err(SessionError::UnsupportedVersion {
            requested: 2,
            supported: PROTOCOL_VERSION
        })
    ));
    assert!(handler.calls.is_empty());
}

#[test]
fn generated_tokens_are_distinct_and_all_formatting_is_redacted() {
    let first = SessionToken::generate().unwrap();
    let second = SessionToken::generate().unwrap();
    assert_ne!(first, second);
    assert_eq!(format!("{first}"), "[redacted]");
    assert_eq!(format!("{first:?}"), "SessionToken([redacted])");
}
