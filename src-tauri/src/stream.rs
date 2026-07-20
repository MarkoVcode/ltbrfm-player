//! Network ingest for an Icecast/SHOUTcast MP3 stream.
//!
//! A dedicated thread pulls bytes over HTTP, strips ICY inline metadata
//! (`StreamTitle='…'`) and writes the clean audio bytes into a [`BytePipe`].
//! The decode thread reads that pipe through [`PipeReader`], which implements
//! Symphonia's [`MediaSource`] and is `Send + Sync` (a live HTTP body is not,
//! so decoupling through the pipe is what makes decoding possible).

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use symphonia::core::io::MediaSource;

/// Soft cap on buffered audio bytes; writers block for space, giving end-to-end
/// backpressure from the sound card all the way to the TCP socket.
const PIPE_CAP: usize = 256 * 1024;

struct PipeState {
    buf: VecDeque<u8>,
    closed: bool,
}

/// A bounded, blocking single-producer/single-consumer byte pipe.
pub struct BytePipe {
    state: Mutex<PipeState>,
    space: Condvar,
    data: Condvar,
}

impl BytePipe {
    pub fn new() -> Arc<Self> {
        Arc::new(BytePipe {
            state: Mutex::new(PipeState {
                buf: VecDeque::with_capacity(PIPE_CAP),
                closed: false,
            }),
            space: Condvar::new(),
            data: Condvar::new(),
        })
    }

    /// Write all bytes, blocking while the buffer is full. Returns early if the
    /// pipe is closed or `stop` is raised.
    fn write_all(&self, mut bytes: &[u8], stop: &AtomicBool) {
        let mut guard = self.state.lock().unwrap();
        while !bytes.is_empty() {
            if guard.closed || stop.load(Ordering::Relaxed) {
                return;
            }
            if guard.buf.len() >= PIPE_CAP {
                let (g, _) = self
                    .space
                    .wait_timeout(guard, Duration::from_millis(200))
                    .unwrap();
                guard = g;
                continue;
            }
            let can = (PIPE_CAP - guard.buf.len()).min(bytes.len());
            guard.buf.extend(&bytes[..can]);
            bytes = &bytes[can..];
            self.data.notify_one();
        }
    }

    pub fn close(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.closed = true;
        self.data.notify_all();
        self.space.notify_all();
    }
}

/// Read side of a [`BytePipe`]; this is what Symphonia decodes from.
pub struct PipeReader {
    pipe: Arc<BytePipe>,
}

impl PipeReader {
    pub fn new(pipe: Arc<BytePipe>) -> Self {
        PipeReader { pipe }
    }
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut guard = self.pipe.state.lock().unwrap();
        loop {
            if !guard.buf.is_empty() {
                let n = guard.buf.len().min(out.len());
                for slot in out.iter_mut().take(n) {
                    *slot = guard.buf.pop_front().unwrap();
                }
                self.pipe.space.notify_one();
                return Ok(n);
            }
            if guard.closed {
                return Ok(0); // clean EOF
            }
            guard = self
                .pipe
                .data
                .wait_timeout(guard, Duration::from_millis(500))
                .unwrap()
                .0;
        }
    }
}

impl Seek for PipeReader {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "live stream is not seekable",
        ))
    }
}

impl MediaSource for PipeReader {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

// ---------------------------------------------------------------------------
// ICY metadata demultiplexer
// ---------------------------------------------------------------------------

enum Seg {
    Audio(usize),
    MetaLen,
    Meta(usize, Vec<u8>),
}

struct IcyDemux {
    metaint: usize,
    seg: Seg,
    last_title: String,
}

impl IcyDemux {
    fn new(metaint: usize) -> Self {
        IcyDemux {
            metaint,
            seg: if metaint == 0 {
                Seg::Audio(usize::MAX)
            } else {
                Seg::Audio(metaint)
            },
            last_title: String::new(),
        }
    }

    /// Split a chunk into audio (written to the pipe) and metadata (parsed).
    fn feed(
        &mut self,
        chunk: &[u8],
        pipe: &BytePipe,
        stop: &AtomicBool,
        on_title: &mut dyn FnMut(String),
    ) {
        if self.metaint == 0 {
            pipe.write_all(chunk, stop);
            return;
        }
        let mut idx = 0;
        while idx < chunk.len() {
            match &mut self.seg {
                Seg::Audio(left) => {
                    let take = (*left).min(chunk.len() - idx);
                    pipe.write_all(&chunk[idx..idx + take], stop);
                    idx += take;
                    *left -= take;
                    if *left == 0 {
                        self.seg = Seg::MetaLen;
                    }
                }
                Seg::MetaLen => {
                    let len = chunk[idx] as usize * 16;
                    idx += 1;
                    self.seg = if len == 0 {
                        Seg::Audio(self.metaint)
                    } else {
                        Seg::Meta(len, Vec::with_capacity(len))
                    };
                }
                Seg::Meta(rem, acc) => {
                    let take = (*rem).min(chunk.len() - idx);
                    acc.extend_from_slice(&chunk[idx..idx + take]);
                    idx += take;
                    *rem -= take;
                    if *rem == 0 {
                        if let Some(title) = parse_stream_title(acc) {
                            if title != self.last_title {
                                self.last_title = title.clone();
                                on_title(title);
                            }
                        }
                        self.seg = Seg::Audio(self.metaint);
                    }
                }
            }
        }
    }
}

fn parse_stream_title(bytes: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(bytes);
    let start = s.find("StreamTitle='")? + "StreamTitle='".len();
    let rest = &s[start..];
    let end = rest.find("';").unwrap_or(rest.len());
    let title = rest[..end].trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

// ---------------------------------------------------------------------------
// Fetch loop
// ---------------------------------------------------------------------------

/// Connect and pump the stream into `pipe` until `stop`, EOF, or an error.
/// `on_title` is called whenever the ICY `StreamTitle` changes. Always closes
/// the pipe on exit so the decoder unblocks.
pub fn run<F>(
    url: &str,
    stop: Arc<AtomicBool>,
    pipe: Arc<BytePipe>,
    mut on_title: F,
) -> io::Result<()>
where
    F: FnMut(String),
{
    let result = pump(url, &stop, &pipe, &mut on_title);
    pipe.close();
    result
}

fn pump(
    url: &str,
    stop: &AtomicBool,
    pipe: &BytePipe,
    on_title: &mut dyn FnMut(String),
) -> io::Result<()> {
    // No total timeout — this is an endless stream. TCP keepalive lets the OS
    // detect a dead peer and fail the blocking read instead of hanging forever.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(15))
        .user_agent("LTBR-FM-Receiver/0.1")
        .build()
        .map_err(to_io)?;

    let mut resp = client
        .get(url)
        .header("Icy-MetaData", "1")
        .send()
        .map_err(to_io)?;

    if !resp.status().is_success() {
        return Err(io::Error::other(format!("HTTP {}", resp.status())));
    }

    let metaint = resp
        .headers()
        .get("icy-metaint")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let mut demux = IcyDemux::new(metaint);
    let mut buf = [0u8; 8192];

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match resp.read(&mut buf) {
            Ok(0) => return Ok(()), // stream ended
            Ok(n) => demux.feed(&buf[..n], pipe, stop, on_title),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn to_io(e: reqwest::Error) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_title() {
        let meta = b"StreamTitle='Roots Manuva - Witness';StreamUrl='https://ltbr.fm';";
        assert_eq!(
            parse_stream_title(meta).as_deref(),
            Some("Roots Manuva - Witness")
        );
    }

    #[test]
    fn empty_title_is_none() {
        assert_eq!(parse_stream_title(b"StreamTitle='';"), None);
        assert_eq!(parse_stream_title(b"no title here"), None);
    }

    #[test]
    fn icy_demux_splits_audio_and_metadata() {
        // metaint = 4: four audio bytes, then a length byte, then padded meta.
        let pipe = BytePipe::new();
        let stop = AtomicBool::new(false);
        let mut demux = IcyDemux::new(4);

        let title = "StreamTitle='X - Y';";
        let blocks = title.len().div_ceil(16);
        let mut meta = vec![blocks as u8];
        meta.extend_from_slice(title.as_bytes());
        meta.resize(1 + blocks * 16, 0);

        let mut chunk = vec![1u8, 2, 3, 4]; // audio
        chunk.extend_from_slice(&meta); // metadata block
        chunk.extend_from_slice(&[5, 6, 7, 8]); // more audio

        let mut got_title = String::new();
        demux.feed(&chunk, &pipe, &stop, &mut |t| got_title = t);
        pipe.close();

        // Audio bytes should have passed through, metadata stripped.
        let mut reader = PipeReader::new(pipe);
        let mut audio = Vec::new();
        reader.read_to_end(&mut audio).unwrap();
        assert_eq!(audio, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(got_title, "X - Y");
    }
}
