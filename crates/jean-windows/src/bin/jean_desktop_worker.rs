//! `jean-desktop-worker.exe` — the process a UIA or capture call actually runs in.
//!
//! Started by [`jean_windows::worker`] (D-051), inside the Job Object `jean-process` creates for every
//! child, with its stdin and stdout wired to pipes this process owns. It reads one request per line,
//! performs it with the crate's own backend implementations, and writes one reply per line. It is not a
//! service, not a daemon, and not reusable across J.E.A.N. runs: it exists for the duration of a
//! conversation and then exits.
//!
//! The reason it exists at all is the sentence in the Phase-4 requirement that the in-process design could
//! not satisfy: *"a timed-out thread alone is not adequate isolation if it remains stuck indefinitely."*
//! A thread parked inside a cross-process `SendMessage` cannot be recalled. This process can be
//! terminated, with its tree, and then nothing is parked anywhere.
//!
//! ## Why the work happens on this thread
//!
//! `UIAutomation::new()` initialises a multithreaded apartment **on the calling thread**, and the
//! [`UiaBackend`] creates one per call rather than sharing an instance. In the parent that was a property
//! of the throwaway worker thread; here it is a property of the process, and it is the same property doing
//! the same job: nothing that reaches into another application's provider shares state with anything that
//! outlives the call.
//!
//! ## What it may write where
//!
//! Only protocol lines go to stdout, and only diagnostics go to stderr. A reply carries control names and
//! values, which are already the crate's redacted-and-bounded forms (`Readback` withholds a password
//! value; names are cut at `MAX_NAME_CHARS`), and stderr carries startup facts and nothing else. This
//! binary never writes a frame anywhere except into the directory it was handed.
//!
//! ## Why it dispatches to the backends and not to the entry points
//!
//! The crate's public `read_tree`/`act`/`capture` functions start a worker. This process *is* a worker, so
//! it calls the [`AccessibilityBackend`] and [`CaptureBackend`] implementations directly, in this process,
//! on this thread. That is the whole of the recursion rule: the supervision lives in one direction only.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use jean_windows::accessibility::{AccessibilityBackend, Locator, Settings, UiaBackend};
use jean_windows::capture::{Backend, CaptureBackend, PrintWindowCapture, ScreenCapture};
use jean_windows::error::WindowsError;
use jean_windows::identity::WindowId;
use jean_windows::worker::{
    decode_request, encode_reply, Body, FrameMeta, Reply, ReplyBody, WireError, PROTOCOL_VERSION,
};

fn main() {
    // The capture path is the only thing here that needs it, and it needs it before its first rectangle:
    // a worker that inherited system-DPI awareness would report coordinates in a space that lies on the
    // second monitor (R-15). The outcome is recorded rather than assumed, because it is process-global and
    // irreversible.
    let awareness = jean_windows::identity::ensure_dpi_awareness();
    let awareness = match &awareness {
        jean_windows::identity::Awareness::Set => "set".to_owned(),
        jean_windows::identity::Awareness::AlreadyFixed(value) => {
            format!("already fixed at {value}")
        }
        jean_windows::identity::Awareness::Unavailable(reason) => {
            format!("unavailable: {reason}")
        }
    };
    eprintln!(
        "jean-desktop-worker pid {} protocol {PROTOCOL_VERSION} awareness {awareness}",
        std::process::id()
    );

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            // The parent closed its write half: this session is over, and exiting is the polite answer.
            Ok(0) => return,
            Ok(_) => {}
            Err(error) => {
                eprintln!("the worker's stdin failed: {error}");
                return;
            }
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let reply = answer(trimmed);
        match encode_reply(&reply) {
            Ok(encoded) => {
                if writer
                    .write_all(format!("{encoded}\n").as_bytes())
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    // The parent stopped reading. There is nobody left to answer.
                    return;
                }
            }
            // A reply that cannot be written is a reply the parent must not be left guessing about, so the
            // refusal travels as a failure the parent can name.
            Err(error) => {
                let fallback = Reply {
                    id: 0,
                    body: ReplyBody::Refused {
                        reason: error.to_string(),
                    },
                };
                let encoded =
                    serde_json::to_string(&fallback).unwrap_or_else(|_| String::from("{}\n"));
                let _ = writer.write_all(format!("{encoded}\n").as_bytes());
                let _ = writer.flush();
            }
        }
    }
}

/// One line in, one reply out. Nothing else in this binary decides anything.
///
/// The request ceiling is enforced by [`decode_request`], which this binary shares with the parent rather
/// than restating: a worker that accepted an unbounded line could not be trusted to have been addressed by
/// the process that started it.
fn answer(line: &str) -> Reply {
    let request = match decode_request(line) {
        Ok(request) => request,
        Err(reason) => {
            return Reply {
                id: 0,
                body: ReplyBody::Refused { reason },
            };
        }
    };
    let id = request.id;
    Reply {
        id,
        body: match request.body {
            Body::Hello {
                version,
                parent_pid,
            } => {
                if version != PROTOCOL_VERSION {
                    return Reply {
                        id,
                        body: ReplyBody::Refused {
                            reason: format!(
                                "this worker speaks {PROTOCOL_VERSION} and the parent asked for {version} \
                                 (parent pid {parent_pid})"
                            ),
                        },
                    };
                }
                ReplyBody::Pong {
                    version: PROTOCOL_VERSION,
                    pid: std::process::id(),
                }
            }
            other => match perform(other) {
                Ok(body) => body,
                Err(error) => ReplyBody::Failure(WireError::from(&error)),
            },
        },
    }
}

/// Run one desktop call, on this thread, with the crate's own backends.
///
/// The bounds travel with the request: the tree cap and the pattern probe are the caller's, and the
/// timeout is *not* here, because the party that decides when a call has taken too long is the one holding
/// the job handle.
fn perform(body: Body) -> Result<ReplyBody, WindowsError> {
    match body {
        Body::Hello { .. } => Err(WindowsError::InvalidSetting {
            reason: "the handshake was already completed".to_owned(),
        }),
        Body::ReadTree {
            window,
            max_nodes,
            probe_patterns,
        } => {
            let settings = settings(max_nodes, probe_patterns);
            Ok(ReplyBody::Tree(
                UiaBackend.read_tree(WindowId { hwnd: window }, &settings)?,
            ))
        }
        Body::Resolve {
            window,
            locator,
            max_nodes,
            probe_patterns,
        } => {
            let settings = settings(max_nodes, probe_patterns);
            Ok(ReplyBody::Controls(UiaBackend.resolve(
                WindowId { hwnd: window },
                &locator,
                &settings,
            )?))
        }
        Body::Probe { window, max_nodes } => {
            let settings = settings(max_nodes, false);
            Ok(ReplyBody::Probe(
                UiaBackend.probe(WindowId { hwnd: window }, &settings)?,
            ))
        }
        Body::ReadState {
            window,
            locator,
            probe_patterns,
            max_nodes,
        } => {
            let settings = settings(max_nodes, probe_patterns);
            Ok(ReplyBody::State(UiaBackend.read_state(
                WindowId { hwnd: window },
                &locator,
                &settings,
            )?))
        }
        Body::Act {
            window,
            locator,
            action,
            witness,
            probe_patterns,
            max_nodes,
        } => {
            let settings = settings(max_nodes, probe_patterns);
            let witness: Option<&Locator> = witness.as_ref();
            Ok(ReplyBody::Effect(Box::new(UiaBackend.act(
                WindowId { hwnd: window },
                &locator,
                &action,
                witness,
                &settings,
            )?)))
        }
        Body::Capture {
            window: hwnd,
            max_pixels,
            backend,
            frame_dir,
        } => {
            let id = WindowId { hwnd };
            let frame = match backend {
                Backend::PrintWindow => PrintWindowCapture.capture(id, max_pixels)?,
                Backend::ScreenBitBlt => ScreenCapture.capture(id, max_pixels)?,
            };
            // The pixels are far too large for a line protocol, and the directory was made by the parent
            // for exactly this file. The metadata and the byte count are checked by
            // `FrameMeta::into_capture` on the other side.
            let file = match frame_dir {
                Some(dir) => {
                    let path = std::path::Path::new(&dir)
                        .join(format!("frame-{}-{hwnd}.bgra", std::process::id()));
                    std::fs::write(&path, &frame.pixels).map_err(|error| {
                        WindowsError::CaptureUnusable {
                            hwnd,
                            reason: format!(
                                "the frame could not be written to {}: {error}",
                                path.display()
                            ),
                        }
                    })?;
                    Some(path.to_string_lossy().into_owned())
                }
                // Asked for metadata only: the quality verdict costs no pixels, and any caller that then
                // wants a capture is refused by `FrameMeta::into_capture` rather than given an empty one.
                None => None,
            };
            Ok(ReplyBody::Frame(FrameMeta {
                window: hwnd,
                width: frame.width,
                height: frame.height,
                hash: frame.hash,
                mean_luma: frame.mean_luma,
                uniform: frame.uniform,
                uniform_argb: frame.uniform_argb,
                render_dpi: frame.render_dpi,
                captured_at_ms: frame.captured_at_ms,
                backend: frame.backend,
                file,
                // The parent reads the pixels out of the directory it owns; they never cross the pipe.
                pixels: None,
            }))
        }
    }
}

/// The bounds a backend is asked to work inside.
///
/// `call_timeout` is not a deadline here — the parent owns that — but `Settings::validate` and a few
/// in-crate retries read the field, so it carries the crate default rather than a fiction.
fn settings(max_nodes: usize, probe_patterns: bool) -> Settings {
    Settings {
        call_timeout: Duration::from_secs(5),
        max_nodes: max_nodes.max(1),
        probe_patterns,
    }
}
