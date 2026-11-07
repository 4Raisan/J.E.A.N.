//! A one-shot HTTP stub for the adapter tests.
//!
//! Deliberately not a provider: no model server is installed, started, or downloaded to run this
//! suite. The stub answers each connection with bytes the test supplies and records the request so
//! its shape can be asserted. Deterministic and offline, which is what makes "the provider timed
//! out" and "the provider answered 502 once" testable at all. Declared from `lib.rs` under
//! `cfg(test)`, so it is not part of the shipped crate.
//!
//! The connection discipline is the interesting part. The stub reads the request until
//! `Content-Length` is satisfied, writes the whole response, shuts down *its* write half to send
//! FIN, and only then waits for the client to close. Closing earlier would reset the connection
//! while the response is still sitting unread in the client's receive queue, which Windows reports
//! as error 10054 — a failure of the test harness, not of the code under test. Every response
//! helper here declares `Connection: close` so the client does not pool a socket the stub is about
//! to forget.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Stub {
    address: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stub {
    /// Answer every request with the same bytes.
    pub fn spawn(reply: String) -> Self {
        Self::spawn_dyn(move |_| reply.clone())
    }

    /// Build the answer from the request, so a test can vary the response or assert on the request.
    pub fn spawn_dyn(build: impl Fn(&str) -> String + Send + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        let address = listener.local_addr().expect("local addr").to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut socket) = incoming else { continue };
                let text = read_request(&mut socket);
                let answer = build(&text);
                recorded.lock().expect("record lock").push(text);
                let _ = socket.write_all(answer.as_bytes());
                let _ = socket.shutdown(Shutdown::Write);
                wait_for_client_to_close(&mut socket);
            }
        });
        Self {
            address,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// A `base_url` pointing at this stub.
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The `host:port` this stub listens on, for a test that wants a different scheme.
    pub fn authority(&self) -> String {
        self.address.clone()
    }

    /// Accept the request and then say nothing, holding the connection open.
    ///
    /// This is the shape a slow provider has: it is how a test proves that cancelling interrupts a
    /// parked read rather than waiting out the deadline. The stall ends when the peer closes, which
    /// is what `Agent::shutdown()` does, so dropping the stub cannot leave a thread parked.
    pub fn spawn_stalling() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        let address = listener.local_addr().expect("local addr").to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut socket) = incoming else { continue };
                let text = read_request(&mut socket);
                recorded.lock().expect("record lock").push(text);
                // No answer. Wait for the client to give up, with a bound so a test that never
                // cancels cannot hang the suite.
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                let mut drain = [0u8; 512];
                while std::time::Instant::now() < deadline && !stopping.load(Ordering::Relaxed) {
                    match socket.read(&mut drain) {
                        // A read timeout here is the point, not a failure: it is how this loop
                        // notices `stopping`. Only a clean EOF ends the stall early.
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(_) => break,
                    }
                }
            }
        });
        Self {
            address,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// Every request the stub received, raw bytes as text.
    pub fn bodies(&self) -> Vec<String> {
        self.requests.lock().expect("record lock").clone()
    }

    /// The JSON body of the first request, for asserting what was actually sent.
    pub fn first_json(&self) -> serde_json::Value {
        let raw = self.bodies().first().cloned().unwrap_or_default();
        let Some(at) = raw.find("\r\n\r\n") else {
            panic!("the stub recorded no request body: {raw:?}");
        };
        serde_json::from_str(&raw[at + 4..])
            .unwrap_or_else(|e| panic!("the request body was not JSON ({e}): {:?}", &raw[at + 4..]))
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.shutdown();
        // The accept loop is blocked in `incoming()`; one connection releases it.
        let _ = std::net::TcpStream::connect(&self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read until the head is complete and `Content-Length` bytes follow it.
fn read_request(socket: &mut std::net::TcpStream) -> String {
    // One second, not five: a client that sends a complete request never waits on this, and a
    // test that asks for TLS against this plaintext listener does. The shorter bound is what keeps
    // the transport's own error the thing under test rather than a deadline racing it.
    let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match socket.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if is_complete(&buffer) || buffer.len() > 4 * 1024 * 1024 {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Whether a buffer holds a whole request: head terminator plus the declared body.
fn is_complete(buffer: &[u8]) -> bool {
    let head_end = match find(buffer, b"\r\n\r\n") {
        Some(at) => at,
        None => return false,
    };
    let declared = content_length(&buffer[..head_end]).unwrap_or(0);
    buffer.len() >= head_end + 4 + declared
}

fn content_length(head: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(head).lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Keep the socket open until the peer closes it, or until it is clear it will not.
fn wait_for_client_to_close(socket: &mut std::net::TcpStream) {
    let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));
    let mut drain = [0u8; 512];
    let deadline = std::time::Instant::now() + Duration::from_millis(2_000);
    while std::time::Instant::now() < deadline {
        match socket.read(&mut drain) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// Wrap a body in a complete `Content-Length` response.
pub fn ok_body(payload: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
}

/// A `Transfer-Encoding: chunked` response, framed in `chunk_len` pieces.
///
/// This is the shape the previous hand-rolled client refused outright, and the one that declares no
/// total length: a test using it proves the read-side ceiling, not a header-side one.
pub fn chunked_body(payload: &str, chunk_len: usize) -> String {
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    );
    let step = chunk_len.max(1);
    let mut rest = payload;
    while !rest.is_empty() {
        let take = rest.len().min(step);
        let (piece, remainder) = rest.split_at(take);
        // Hex length, as the transfer encoding specifies, not decimal.
        out.push_str(&format!("{:x}\r\n{}\r\n", piece.len(), piece));
        rest = remainder;
    }
    out.push_str("0\r\n\r\n");
    out
}

/// A response with a status and a short body, for the non-200 paths.
pub fn status_only(status: u16, reason: &str, payload: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
}
