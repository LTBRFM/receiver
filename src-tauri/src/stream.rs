//! Network ingest for an Icecast/SHOUTcast MP3 stream.
//!
//! A dedicated thread pulls bytes over HTTP, strips ICY inline metadata and
//! writes the clean audio bytes into a [`BytePipe`]. The decode thread reads
//! that pipe through [`PipeReader`], which implements Symphonia's
//! [`MediaSource`] and is `Send + Sync` (a live HTTP body is not, so
//! decoupling through the pipe is what makes decoding possible).
//!
//! Metadata is *not* handed straight to the UI. Icecast attaches a block to
//! the source's byte position, so a block sits in the byte stream exactly
//! where the audio it describes does — but that audio then waits in the pipe
//! for several seconds before anyone hears it. Blocks are therefore stamped
//! with the pipe's write cursor and held in [`PipeState::meta`] until the
//! decoder reaches that mark (see [`BytePipe::tick`]). That keeps the display
//! honest, and it is also what lets `sync` measure the listener's true
//! end-to-end lag rather than merely the network's.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use symphonia::core::io::MediaSource;

use crate::icy;

/// Soft cap on buffered audio bytes; writers block for space, giving end-to-end
/// backpressure from the sound card all the way to the TCP socket.
///
/// This is exactly twice Icecast's 128 KB `burst-size`, which is deliberate:
/// the burst must fit with room for jitter on top. It is also why a decode
/// stall is so costly — the pipe fills to ~16s and, since everything
/// downstream runs at 1x, never drains again on its own. Shrinking this is
/// *not* the fix: the backlog would simply relocate into the socket and
/// Icecast's own queue, where we can neither see nor trim it. `sync::Drift`
/// trims it here instead.
const PIPE_CAP: usize = 256 * 1024;

/// A metadata block waiting for the audio it describes to reach the decoder.
struct Pending {
    /// Write-cursor position the block was attached to.
    mark: u64,
    block: icy::Block,
}

struct PipeState {
    buf: VecDeque<u8>,
    closed: bool,
    /// Bytes ever accepted into the pipe — the session write cursor. Only
    /// bytes actually copied count, since `write_all` bails early on stop.
    written: u64,
    /// Bytes ever discarded by a catch-up trim. Held apart from `written`
    /// because the decoder's own byte count never sees them: its position in
    /// write-cursor coordinates is `dropped + demuxed`.
    dropped: u64,
    /// Stamped metadata, oldest first. It lives inside the pipe's mutex on
    /// purpose — that is the only place the write cursor and the block are
    /// guaranteed consistent, since stamping outside the lock would let audio
    /// slip in between reading the cursor and enqueuing.
    meta: VecDeque<Pending>,
}

/// What the decoder learns on each iteration: metadata that has come due, and
/// how much audio is still buffered ahead of the speakers.
pub struct Tick {
    /// Blocks whose audio the decoder has now reached, oldest first.
    pub released: Vec<icy::Block>,
    /// Everything not yet decoded: pipe backlog plus the decoder's read-ahead.
    pub depth_bytes: u64,
    /// The pipe's own backlog, which is the part a trim can actually reach.
    pub pipe_bytes: u64,
}

/// Outcome of a catch-up trim.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Trim {
    pub bytes: u64,
    /// Blocks discarded because they described audio nobody will now hear.
    pub meta_skipped: usize,
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
                written: 0,
                dropped: 0,
                meta: VecDeque::new(),
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
            guard.written += can as u64;
            bytes = &bytes[can..];
            self.data.notify_one();
        }
    }

    /// Hold a metadata block until the decoder reaches the audio it describes.
    /// Called from the network thread the moment the block is demuxed, when
    /// the write cursor is precisely the block's byte position.
    pub fn stamp_meta(&self, block: icy::Block) {
        let mut guard = self.state.lock().unwrap();
        let mark = guard.written;
        guard.meta.push_back(Pending { mark, block });
    }

    /// Collect any metadata that has come due and report the buffer depth.
    /// `demuxed` is the running total of packet bytes the decoder has pulled
    /// out — an exact cursor, since an MP3 packet is header plus frame body
    /// with nothing in between.
    pub fn tick(&self, demuxed: u64) -> Tick {
        let mut guard = self.state.lock().unwrap();
        let cursor = guard.dropped + demuxed;

        let mut released = Vec::new();
        while guard.meta.front().is_some_and(|p| p.mark <= cursor) {
            released.push(guard.meta.pop_front().unwrap().block);
        }

        Tick {
            released,
            depth_bytes: guard.written.saturating_sub(cursor),
            pipe_bytes: guard.buf.len() as u64,
        }
    }

    /// Discard the oldest buffered audio until at most `keep` bytes remain,
    /// cutting at the next MPEG frame header so the demuxer resyncs on a real
    /// frame rather than chewing through the middle of one.
    pub fn trim_to(&self, keep: usize, demuxed: u64) -> Trim {
        let mut guard = self.state.lock().unwrap();
        if guard.buf.len() <= keep {
            return Trim::default();
        }

        let want = guard.buf.len() - keep;
        let cut = {
            // One memmove of at most PIPE_CAP under the lock. Fine for an
            // operation that runs a handful of times an hour at worst — but
            // it must never migrate into a hot path.
            let s = guard.buf.make_contiguous();
            (want + frame_sync_offset(&s[want..])).min(s.len())
        };
        guard.buf.drain(..cut);
        guard.dropped += cut as u64;

        // Blocks we just jumped over describe audio nobody will hear. Keep
        // only the newest of them: it is the best available description of
        // what is about to come out of the speakers.
        let cursor = guard.dropped + demuxed;
        let mut meta_skipped = 0;
        while guard.meta.len() > 1
            && guard.meta[0].mark <= cursor
            && guard.meta[1].mark <= cursor
        {
            guard.meta.pop_front();
            meta_skipped += 1;
        }

        // The writer is very likely parked waiting for room.
        self.space.notify_one();

        Trim {
            bytes: cut as u64,
            meta_skipped,
        }
    }

    pub fn close(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.closed = true;
        guard.meta.clear();
        self.data.notify_all();
        self.space.notify_all();
    }
}

/// Offset of the next MPEG frame sync (11 set bits) at or after the start of
/// `s`. Returns 0 when none is found within a bounded scan — a trim landing
/// mid-frame costs at most one glitched frame, which the decoder skips.
fn frame_sync_offset(s: &[u8]) -> usize {
    const MAX_SCAN: usize = 2048;
    let n = s.len().min(MAX_SCAN);
    for i in 0..n.saturating_sub(1) {
        if s[i] == 0xFF && (s[i + 1] & 0xE0) == 0xE0 {
            return i;
        }
    }
    0
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
        }
    }

    /// Split a chunk into audio (written to the pipe) and metadata (parsed and
    /// stamped). Repeats are deliberately *not* filtered here: a heartbeat
    /// carrying an unchanged title is the only regular sync sample we get when
    /// nothing is changing, and `k=talk` repeats the title by design.
    /// De-duplication belongs at the display, not on the measurement path.
    fn feed(&mut self, chunk: &[u8], pipe: &BytePipe, stop: &AtomicBool) {
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
                        // Icecast pads the block to a 16-byte multiple with NULs.
                        let text = String::from_utf8_lossy(acc);
                        if let Some(block) = icy::parse_block(text.trim_end_matches('\0')) {
                            pipe.stamp_meta(block);
                        }
                        self.seg = Seg::Audio(self.metaint);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fetch loop
// ---------------------------------------------------------------------------

/// Station identity from the connect-time ICY response headers. LRD owns these
/// (the Icecast mount deliberately does not set them), so this is where the
/// real station name comes from — it is not in the metadata payload.
#[derive(Clone, Debug, Default)]
pub struct StationInfo {
    pub name: String,
    pub description: String,
    pub genre: String,
    pub url: String,
    /// `icy-br` in kbps, or 0 when the server does not send it — which the
    /// production mount does not, despite advertising it in
    /// `access-control-expose-headers`. The engine measures it instead.
    pub bitrate_kbps: u32,
    pub metaint: usize,
}

/// Connect and pump the stream into `pipe` until `stop`, EOF, or an error.
/// `on_connect` fires once with the station headers. Metadata is stamped into
/// the pipe rather than reported here, so that it surfaces in step with the
/// audio. Always closes the pipe on exit so the decoder unblocks.
pub fn run<F>(
    url: &str,
    stop: Arc<AtomicBool>,
    pipe: Arc<BytePipe>,
    on_connect: F,
) -> io::Result<()>
where
    F: FnOnce(StationInfo),
{
    let result = pump(url, &stop, &pipe, on_connect);
    pipe.close();
    result
}

fn pump<F>(url: &str, stop: &AtomicBool, pipe: &BytePipe, on_connect: F) -> io::Result<()>
where
    F: FnOnce(StationInfo),
{
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

    // Read headers as lossy UTF-8, not `to_str()`: the station name is
    // genuinely non-ASCII ("LTBR·FM") and `to_str()` rejects that outright.
    let header = |name: &str| -> String {
        resp.headers()
            .get(name)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_string())
            .unwrap_or_default()
    };

    let metaint = header("icy-metaint").parse::<usize>().unwrap_or(0);
    on_connect(StationInfo {
        name: header("icy-name"),
        description: header("icy-description"),
        genre: header("icy-genre"),
        url: header("icy-url"),
        bitrate_kbps: header("icy-br").parse::<u32>().unwrap_or(0),
        metaint,
    });

    let mut demux = IcyDemux::new(metaint);
    let mut buf = [0u8; 8192];

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match resp.read(&mut buf) {
            Ok(0) => return Ok(()), // stream ended
            Ok(n) => demux.feed(&buf[..n], pipe, stop),
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

    /// Build an ICY metadata segment (length byte + NUL-padded payload).
    fn meta_segment(text: &str) -> Vec<u8> {
        let blocks = text.len().div_ceil(16);
        let mut out = vec![blocks as u8];
        out.extend_from_slice(text.as_bytes());
        out.resize(1 + blocks * 16, 0);
        out
    }

    #[test]
    fn icy_demux_splits_audio_and_metadata() {
        // metaint = 4: four audio bytes, then a length byte, then padded meta.
        let pipe = BytePipe::new();
        let stop = AtomicBool::new(false);
        let mut demux = IcyDemux::new(4);

        let mut chunk = vec![1u8, 2, 3, 4]; // audio
        chunk.extend_from_slice(&meta_segment("StreamTitle='X - Y';"));
        chunk.extend_from_slice(&[5, 6, 7, 8]); // more audio

        demux.feed(&chunk, &pipe, &stop);
        let released = pipe.tick(8).released;
        pipe.close();

        // Audio bytes should have passed through, metadata stripped.
        let mut reader = PipeReader::new(pipe);
        let mut audio = Vec::new();
        reader.read_to_end(&mut audio).unwrap();
        assert_eq!(audio, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].title, "X - Y");
    }

    #[test]
    fn blocks_are_stamped_at_the_write_cursor() {
        let pipe = BytePipe::new();
        let stop = AtomicBool::new(false);
        let mut demux = IcyDemux::new(4);

        let mut chunk = vec![0u8; 4];
        chunk.extend_from_slice(&meta_segment("StreamTitle='first';"));
        chunk.extend_from_slice(&[0u8; 4]);
        chunk.extend_from_slice(&meta_segment("StreamTitle='second';"));
        demux.feed(&chunk, &pipe, &stop);

        // Nothing is due before the decoder has consumed the audio ahead of it.
        assert!(pipe.tick(3).released.is_empty(), "released early");
        let first = pipe.tick(4).released;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].title, "first");

        assert!(pipe.tick(7).released.is_empty(), "second released early");
        let second = pipe.tick(8).released;
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].title, "second");
    }

    #[test]
    fn depth_reflects_what_the_decoder_has_not_reached() {
        let pipe = BytePipe::new();
        let stop = AtomicBool::new(false);
        pipe.write_all(&[0u8; 1000], &stop);
        assert_eq!(pipe.tick(0).depth_bytes, 1000);
        assert_eq!(pipe.tick(400).depth_bytes, 600);
    }

    #[test]
    fn trim_cuts_at_a_frame_header_and_accounts_for_it() {
        let pipe = BytePipe::new();
        let stop = AtomicBool::new(false);

        // 600 bytes of junk, then a frame sync, then more.
        let mut data = vec![0x11u8; 600];
        data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        data.extend_from_slice(&[0x22; 396]);
        pipe.write_all(&data, &stop);

        let trim = pipe.trim_to(400, 0);
        // Wanted to drop 600, but slid forward to the sync word at 600.
        assert_eq!(trim.bytes, 600);

        // The trim is invisible to the decoder's own byte count, so depth must
        // fall by exactly what was discarded.
        assert_eq!(pipe.tick(0).depth_bytes, 400);

        let mut reader = PipeReader::new(pipe);
        let mut head = [0u8; 2];
        reader.read_exact(&mut head).unwrap();
        assert_eq!(head, [0xFF, 0xFB], "playback must resume on a frame header");
    }

    #[test]
    fn trim_drops_metadata_it_jumped_over() {
        let pipe = BytePipe::new();
        let stop = AtomicBool::new(false);

        // Three blocks spread through the buffer, all in the region we discard.
        for i in 0..3 {
            pipe.write_all(&[0x11u8; 300], &stop);
            pipe.stamp_meta(icy::Block {
                title: format!("t{i}"),
                url: None,
            });
        }
        pipe.write_all(&[0x22u8; 100], &stop);

        let trim = pipe.trim_to(100, 0);
        assert_eq!(trim.meta_skipped, 2, "only the newest should survive");

        // The survivor is the best description of what is about to be heard.
        let released = pipe.tick(0).released;
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].title, "t2");
    }

    #[test]
    fn trim_below_the_keep_threshold_is_a_no_op() {
        let pipe = BytePipe::new();
        let stop = AtomicBool::new(false);
        pipe.write_all(&[0u8; 100], &stop);
        assert_eq!(pipe.trim_to(400, 0), Trim::default());
        assert_eq!(pipe.tick(0).depth_bytes, 100);
    }
}
