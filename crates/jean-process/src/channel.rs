use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::PeekNamedPipe;
use windows::Win32::System::IO::CancelSynchronousIo;

use crate::{piped, LaunchSpec, PipedControl};
use jean_protocol::interaction::{MAX_FRAME, QUEUE_CAPACITY};

pub struct Channel {
    child: piped::PipedChild,
    stdout: File,
    stderr: File,
    writer: Option<JoinHandle<()>>,
    outgoing: Option<mpsc::SyncSender<Vec<u8>>>,
    stopping: Arc<AtomicBool>,
    write_failed: Arc<AtomicBool>,
    buffered: Vec<u8>,
    eof: bool,
}

impl Channel {
    pub fn spawn(spec: &LaunchSpec) -> io::Result<Self> {
        let child = piped::spawn(spec).map_err(io::Error::other)?;
        let prepared = (|| {
            Ok((
                child.stdin_file().map_err(io::Error::other)?,
                child.stdout_file().map_err(io::Error::other)?,
                child.stderr_file().map_err(io::Error::other)?,
            ))
        })();
        let (mut stdin, stdout, stderr) = match prepared {
            Ok(files) => files,
            Err(error) => {
                let _ = child.control().terminate();
                return Err(error);
            }
        };
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));
        let write_failed = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let failed = write_failed.clone();
        let control = child.control();
        let writer = std::thread::Builder::new()
            .name("jean-worker-write".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let Ok(bytes) = rx.recv() else { break };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if stdin.write_all(&bytes).is_err() {
                        failed.store(true, Ordering::Release);
                        let _ = control.terminate();
                        break;
                    }
                }
            });
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                let _ = child.control().terminate();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            stdout,
            stderr,
            writer: Some(writer),
            outgoing: Some(tx),
            stopping,
            write_failed,
            buffered: Vec::new(),
            eof: false,
        })
    }

    pub fn control(&self) -> PipedControl {
        self.child.control()
    }

    pub fn send(&self, bytes: Vec<u8>) -> io::Result<()> {
        if bytes.len() > MAX_FRAME {
            return Err(io::Error::other("oversized worker frame"));
        }
        self.outgoing
            .as_ref()
            .ok_or_else(|| io::Error::other("worker is stopping"))?
            .try_send(bytes)
            .map_err(|_| io::Error::other("worker output queue full or disconnected"))
    }

    pub fn poll(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.write_failed.load(Ordering::Acquire) {
            return Err(io::Error::other("worker write disconnected"));
        }
        let mut scratch = [0u8; 4096];
        let _ = read_available(&mut self.stderr, &mut scratch);
        if let Some(end) = self.buffered.iter().position(|b| *b == b'\n') {
            return Ok(Some(self.buffered.drain(..=end).collect()));
        }
        if self.eof {
            return Err(io::Error::other("worker disconnected"));
        }
        let room = MAX_FRAME - self.buffered.len();
        if room == 0 {
            return Err(io::Error::other("oversized worker frame"));
        }
        let limit = room.min(scratch.len());
        match read_available(&mut self.stdout, &mut scratch[..limit])? {
            Some(0) => self.eof = true,
            Some(n) => self.buffered.extend_from_slice(&scratch[..n]),
            None => {}
        }
        if let Some(end) = self.buffered.iter().position(|b| *b == b'\n') {
            Ok(Some(self.buffered.drain(..=end).collect()))
        } else if self.eof {
            Err(io::Error::other("worker disconnected or truncated a frame"))
        } else {
            Ok(None)
        }
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        self.stopping.store(true, Ordering::Release);
        self.outgoing.take();
        let result = self.child.control().terminate().map_err(io::Error::other);
        self.cancel_write();
        result
    }

    fn cancel_write(&self) {
        if let Some(writer) = &self.writer {
            // Cancellation is retried by the owner until the writer has actually exited.
            let _ = unsafe { CancelSynchronousIo(HANDLE(writer.as_raw_handle())) };
        }
    }

    pub fn reap(&mut self) -> bool {
        self.cancel_write();
        if self.child.control().is_running() || !self.child.control().tree_empty().unwrap_or(false)
        {
            return false;
        }
        if self
            .writer
            .as_ref()
            .is_some_and(|writer| !writer.is_finished())
        {
            return false;
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        true
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        let _ = self.terminate();
        let until = Instant::now() + Duration::from_secs(2);
        while !self.reap() && Instant::now() < until {
            std::thread::yield_now();
        }
        // Detaching a blocked writer would retain the job and permit unbounded orphaned threads.
        if !self.reap() {
            std::process::abort();
        }
    }
}

fn read_available(file: &mut File, buffer: &mut [u8]) -> io::Result<Option<usize>> {
    let mut available = 0;
    let peek = unsafe {
        PeekNamedPipe(
            HANDLE(file.as_raw_handle()),
            None,
            0,
            None,
            Some(&mut available),
            None,
        )
    };
    if let Err(error) = peek {
        let code = error.code().0 as u32 & 0xffff;
        if code == 109 || code == 233 {
            return Ok(Some(0));
        }
        return Err(io::Error::other(error));
    }
    if available == 0 {
        return Ok(None);
    }
    let count = buffer.len().min(available as usize);
    file.read(&mut buffer[..count]).map(Some)
}
