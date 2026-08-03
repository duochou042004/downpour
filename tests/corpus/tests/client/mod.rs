//! A reference HTTP client for verifying the pathology server.
//!
//! Deliberately **not** `downpour-http`: checking our server with our client would be circular,
//! and either one being wrong would look like agreement.
//!
//! HTTP/1.1 is read by hand here rather than through a library, because the pathologies that
//! matter are wire-level — a `Content-Length` that disagrees with the body, no framing at all,
//! a truncated body — and a conforming library's job is to hide or reject exactly those. This
//! reader reports what arrived. HTTP/2 goes through `hyper`, which is a third-party
//! implementation and therefore a legitimate oracle.

use std::net::SocketAddr;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// What arrived, without interpretation.
pub(crate) struct RawResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl RawResponse {
    /// Look up a header by lowercase name.
    pub(crate) fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }
}

/// One HTTP/1.1 request on a fresh connection, read until the connection closes.
///
/// Reading to EOF rather than to `Content-Length` is the point: it is the only way to observe a
/// body that disagrees with its declared length.
pub(crate) async fn h1_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> RawResponse {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("connect to the pathology server");

    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write the request");
    stream.flush().await.expect("flush");

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read the response");
    parse_h1(&raw).0
}

/// Several HTTP/1.1 requests on one connection, to prove keep-alive works.
pub(crate) async fn h1_pipeline(
    addr: SocketAddr,
    requests: &[(String, Vec<(String, String)>)],
) -> Vec<RawResponse> {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("connect to the pathology server");
    let mut out = Vec::new();
    let mut buffered: Vec<u8> = Vec::new();

    for (index, (path, headers)) in requests.iter().enumerate() {
        let last = index + 1 == requests.len();
        let mut request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(if last {
            "Connection: close\r\n\r\n"
        } else {
            "\r\n"
        });
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write the request");
        stream.flush().await.expect("flush");

        // Read until a complete response is buffered, so a keep-alive response with a
        // Content-Length is consumed exactly and the next one starts clean.
        loop {
            if let (response, Some(consumed)) = parse_h1(&buffered)
                && response.status != 0
            {
                out.push(response);
                buffered.drain(..consumed);
                break;
            }
            let mut chunk = [0_u8; 8192];
            let read = stream.read(&mut chunk).await.expect("read");
            if read == 0 {
                let (response, _) = parse_h1(&buffered);
                if response.status != 0 {
                    out.push(response);
                }
                return out;
            }
            buffered.extend_from_slice(&chunk[..read]);
        }
    }
    out
}

/// Parse a response, returning how many bytes it consumed when that is knowable.
///
/// A status of 0 means "not a complete response yet".
fn parse_h1(raw: &[u8]) -> (RawResponse, Option<usize>) {
    let empty = RawResponse {
        status: 0,
        headers: Vec::new(),
        body: Vec::new(),
    };
    let Some(head_end) = find(raw, b"\r\n\r\n") else {
        return (empty, None);
    };
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.split("\r\n");

    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }

    let body_start = head_end + 4;
    let available = &raw[body_start.min(raw.len())..];
    let chunked = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));

    if chunked {
        let (body, consumed) = decode_chunked(available);
        let response = RawResponse {
            status,
            headers,
            body,
        };
        return (response, consumed.map(|c| body_start + c));
    }

    let declared = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok());

    match declared {
        // Report what actually arrived, even when it is less than declared — that is the whole
        // reason this reader exists.
        Some(length) if available.len() >= length => {
            let response = RawResponse {
                status,
                headers,
                body: available[..length].to_vec(),
            };
            (response, Some(body_start + length))
        }
        Some(_) | None => {
            let response = RawResponse {
                status,
                headers,
                body: available.to_vec(),
            };
            (response, None)
        }
    }
}

fn decode_chunked(raw: &[u8]) -> (Vec<u8>, Option<usize>) {
    let mut body = Vec::new();
    let mut position = 0_usize;
    loop {
        let Some(line_end) = find(&raw[position..], b"\r\n") else {
            return (body, None);
        };
        let size_line = String::from_utf8_lossy(&raw[position..position + line_end]);
        let Ok(size) = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16)
        else {
            return (body, None);
        };
        position += line_end + 2;
        if size == 0 {
            return (body, Some(position + 2));
        }
        if position + size + 2 > raw.len() {
            return (body, None);
        }
        body.extend_from_slice(&raw[position..position + size]);
        position += size + 2;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// One HTTP/2 GET over cleartext (h2c, prior knowledge), via `hyper` as the reference client.
pub(crate) async fn h2_get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> RawResponse {
    let stream = TcpStream::connect(addr)
        .await
        .expect("connect to the pathology server");
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, connection) =
        hyper::client::conn::http2::handshake(hyper_util::rt::TokioExecutor::new(), io)
            .await
            .expect("h2c handshake");

    tokio::spawn(async move {
        // The connection future must be driven for the request to progress. An error here means
        // the server closed, which the assertions below will surface.
        if let Err(error) = connection.await {
            eprintln!("h2 connection ended: {error}");
        }
    });

    let mut builder = hyper::Request::builder()
        .method("GET")
        .uri(format!("http://{addr}{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(Empty::<Bytes>::new())
        .expect("build the request");

    let response = sender
        .send_request(request)
        .await
        .expect("send the request");
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("read the body")
        .to_bytes()
        .to_vec();

    RawResponse {
        status,
        headers,
        body,
    }
}
