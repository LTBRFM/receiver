//! Engine orchestration: owns a long-lived worker thread that plays a stream
//! session (network -> decode -> DSP -> resample -> output) and reconnects
//! forever until told to stop. Tauri commands mutate [`Controls`] atomics and
//! send [`Cmd`]s here.
//!
//! A session normally runs one [`Deck`] (connection + decoder). A *seamless
//! switch* — moving between the station's DJ and no-DJ mounts without a gap —
//! briefly runs two: the incoming deck is connected, decoded ahead, aligned to
//! the playing one by cross-correlating their audio, and crossfaded in while
//! the outgoing deck is still on air. See [`Incoming`] for the state machine.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tauri::{AppHandle, Emitter};

use crate::dsp::{Controls, Dsp, NUM_BANDS};
use crate::icy;
use crate::output::Output;
use crate::spectrum::Spectrum;
use crate::stream::{self, BytePipe, NetError, PipeReader, StationInfo};
use crate::sync::{self, Drift};

const RESAMPLE_CHUNK: usize = 1024;

/// Frames handed to the DSP per loop iteration — one MP3 frame's worth, so the
/// output pacing is exactly what it was when the loop pushed packet by packet.
const CHUNK: usize = 1152;

pub const PRESETS: &[(&str, [f32; NUM_BANDS])] = &[
    ("flat", [0.0; NUM_BANDS]),
    ("pirate", [4.0, 5.0, 2.0, -1.0, -2.0, 0.0, 2.0, 4.0, 5.0, 3.0]),
    ("bass", [8.0, 7.0, 5.0, 2.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0]),
    ("voice", [-4.0, -3.0, 0.0, 3.0, 5.0, 5.0, 3.0, 1.0, -1.0, -2.0]),
];

enum Cmd {
    Play(String),
    Stop,
}

/// Errors the demuxer may raise on the first frames after a deliberate trim,
/// before it finds its footing again. Bounded so a genuinely broken stream
/// still surfaces as a fault.
const SPLICE_TOLERANCE: u32 = 8;

// ---- seamless switch tuning -------------------------------------------------

/// Bytes the incoming pipe must hold before the format probe is attempted, so
/// the probe (which reads from the pipe) can never block the playing deck.
const PROBE_BYTES: usize = 32 * 1024;
/// Only pull a packet from a *non-playing* deck while its pipe holds at least
/// this much — a frame is ~420 bytes at 128 kbps, so this guarantees the read
/// completes without waiting on the network.
const PULL_MIN_BYTES: usize = 8 * 1024;
/// Decode-ahead budget per loop iteration for the incoming deck (~1.5 ms).
const PRIME_PULLS: usize = 24;
/// Keep decoding the incoming deck past what alignment needs once its pipe
/// holds this much, so a long wait (listener far behind the live edge) never
/// parks the network thread on a full pipe — the live edge must keep moving
/// for the estimate to reach the audio being heard.
const PRIME_DRAIN_ABOVE: usize = 192 * 1024;
/// Audio the playing deck is decoded ahead of itself and matched against the
/// incoming deck. Two seconds of programme is far more than enough for an
/// unambiguous correlation peak between two encoders of the same source.
const REF_S: f64 = 2.0;
/// Half-width of the search around the byte-depth estimate. The estimate was
/// measured at ~70 ms off against the real mounts; this is a wide margin.
const WINDOW_S: f64 = 1.0;
/// The estimate must sit at least this far into the incoming burst before the
/// search runs, so the whole window is real audio and not the pre-burst void.
const LEAD_S: f64 = 0.5;
/// Crossfade length. Linear, because the two streams are the *same* programme
/// sample-aligned — a constant-power curve would bump the level mid-fade.
const FADE_S: f64 = 0.5;
/// Below this normalised correlation the fine alignment is not trusted and
/// the byte-depth estimate is used as-is. Still a crossfade, never a gap.
const MIN_CONFIDENCE: f32 = 0.5;
/// A weak match usually means the presenter is talking — voice on one mount,
/// music only on the other — so the search is repeated this many times, this
/// far apart, before the byte estimate is accepted. Long enough for a link to
/// end, short enough that the voice still goes away promptly.
const ALIGN_RETRIES: u32 = 2;
const ALIGN_RETRY_GAP_MS: u64 = 1_500;
/// Give up on a switch whose connection never delivers.
const LINK_TIMEOUT_MS: u64 = 15_000;
/// Give up on a switch that never reaches alignment. Sized past the drift
/// ceiling: a listener 15 s behind needs that long for the incoming burst to
/// reach the audio they are hearing, and the switch waits that out rather
/// than jump.
const PRIME_TIMEOUT_MS: u64 = 20_000;

#[derive(serde::Serialize, Clone)]
struct NowPlaying {
    title: String,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StationPayload {
    name: String,
    description: String,
    genre: String,
    url: String,
    bitrate_kbps: u32,
}

/// The full metadata set, emitted when a block's audio reaches the speakers.
///
/// `playhead_ms` is the station's own timeline position for the audio being
/// heard *right now* — because release is playback-aligned, the frontend can
/// anchor on it and interpolate forward to run a live countdown to the next
/// track without any clock synchronisation or extra request.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct MetadataPayload {
    title: String,
    /// track | jingle | talk | off | hb
    kind: String,
    seq: u64,
    playhead_ms: i64,
    item_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    station: Option<StationPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    programme: Option<icy::Daypart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    now: Option<icy::Segment>,
    next: Vec<icy::Segment>,
    talk: Vec<icy::Talk>,
    jingles: Vec<icy::Jingle>,
    /// True when the schedule was dropped to fit the block's byte cap. The
    /// lists are then empty because we were not told, NOT because nothing is
    /// coming up — a display must not render those the same way.
    schedule_truncated: bool,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SyncPayload {
    /// End-to-end listener lag. `None` until the connection settles; may be
    /// negative if the listener's clock runs behind the station's.
    lag_ms: Option<i64>,
    /// Lag growth since this connection settled — constant clock skew cancels
    /// out of this, which is why the controller reasons about it.
    excess_ms: Option<i64>,
    /// Our own buffers only. Always available, even with metadata off.
    buffer_ms: u64,
    buffer_bytes: u64,
    bitrate_kbps: u32,
    target_ms: u64,
    ceiling_ms: u64,
    state: String,
    /// none | catchup | reconnect
    action: String,
    dropped_ms: u64,
    catchups: u32,
    reconnects: u32,
}

fn emit_sync(app: &AppHandle, drift: &Drift, action: &str) {
    let r = drift.report();
    let _ = app.emit(
        "sync",
        SyncPayload {
            lag_ms: r.lag_ms,
            excess_ms: r.excess_ms,
            buffer_ms: r.buffer_ms,
            buffer_bytes: r.buffer_bytes,
            bitrate_kbps: r.bitrate_kbps,
            target_ms: sync::TARGET_LAG_MS,
            ceiling_ms: sync::CEILING_MS,
            state: r.state.as_str().to_string(),
            action: action.to_string(),
            dropped_ms: r.dropped_ms,
            catchups: r.catchups,
            reconnects: r.reconnects,
        },
    );
}

fn station_payload(info: &StationInfo, bitrate_kbps: u32) -> StationPayload {
    StationPayload {
        name: info.name.clone(),
        description: info.description.clone(),
        genre: info.genre.clone(),
        url: info.url.clone(),
        bitrate_kbps,
    }
}

/// Publish one released block. Returns the title if it should become the new
/// "now playing" string.
fn emit_block(
    app: &AppHandle,
    block: &icy::Block,
    drift: &mut Drift,
    last_title: &mut String,
    station: Option<StationPayload>,
    now_ms: u64,
) {
    if let Some(url) = &block.url {
        if drift.on_block(url, now_ms) == sync::BlockVerdict::StaleSeq {
            return;
        }
    }

    // De-duplicate the title here rather than in the demuxer: heartbeats and
    // DJ-talk blocks repeat it deliberately, and the drift measurement needs
    // every single one of them.
    if !block.title.is_empty() && block.title != *last_title {
        *last_title = block.title.clone();
        let _ = app.emit(
            "nowplaying",
            NowPlaying {
                title: block.title.clone(),
            },
        );
    }

    let url = block.url.as_ref();
    let payload = url.map(|u| u.payload.as_ref());
    let _ = app.emit(
        "metadata",
        MetadataPayload {
            title: block.title.clone(),
            kind: url.map_or_else(String::new, |u| u.kind.clone()),
            seq: url.map_or(0, |u| u.seq),
            playhead_ms: url.map_or(0, |u| u.playhead_ms),
            item_id: url.map_or_else(String::new, |u| u.item_id.clone()),
            station,
            programme: payload.flatten().and_then(|p| p.daypart.clone()),
            now: payload.flatten().and_then(|p| p.now.clone()),
            next: payload.flatten().map(|p| p.next.clone()).unwrap_or_default(),
            talk: payload.flatten().map(|p| p.talk.clone()).unwrap_or_default(),
            jingles: payload
                .flatten()
                .map(|p| p.jingles.clone())
                .unwrap_or_default(),
            schedule_truncated: url.is_some() && payload.flatten().is_none(),
        },
    );
}

/// A failure, classified for the display. `code` picks the short indicator
/// text (see `stream::NetError` for the wire codes; the engine adds `device`
/// and `decode`), `message` is the full story for a tooltip.
#[derive(Debug, Clone)]
pub struct Fault {
    pub code: &'static str,
    pub message: String,
}

impl From<NetError> for Fault {
    fn from(e: NetError) -> Self {
        Fault {
            code: e.code,
            message: e.message,
        }
    }
}

#[derive(serde::Serialize, Clone)]
struct StatePayload {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
}

fn emit_state(app: &AppHandle, state: &'static str, message: Option<&str>) {
    let _ = app.emit(
        "state",
        StatePayload {
            state,
            message: message.map(|s| s.to_string()),
            code: None,
        },
    );
}

fn emit_error_state(app: &AppHandle, fault: &Fault) {
    let _ = app.emit(
        "state",
        StatePayload {
            state: "error",
            message: Some(fault.message.clone()),
            code: Some(fault.code),
        },
    );
}

fn emit_fault(app: &AppHandle, fault: &Fault) {
    let _ = app.emit(
        "fault",
        serde_json::json!({ "message": fault.message, "code": fault.code }),
    );
}

/// Which mount is (or is about to be) on air. `switching` when an incoming
/// deck has been opened, `live` once a mount is what the speakers carry, and
/// `failed` when a switch was abandoned — `url` is then the mount that kept
/// playing.
#[derive(serde::Serialize, Clone)]
struct SourcePayload<'a> {
    url: &'a str,
    phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    /// Whether the crossfade was sample-aligned by correlation (true) or fell
    /// back to the byte-depth estimate (false), and the peak normalised
    /// correlation the search found. Diagnostics only.
    #[serde(skip_serializing_if = "Option::is_none")]
    aligned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f32>,
    /// The stream's sample rate, on `live` — for a readout.
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_rate: Option<u32>,
}

fn emit_source(
    app: &AppHandle,
    url: &str,
    phase: &'static str,
    reason: Option<&str>,
    aligned: Option<(bool, f32)>,
    sample_rate: Option<u32>,
) {
    let _ = app.emit(
        "source",
        SourcePayload {
            url,
            phase,
            reason,
            aligned: aligned.map(|a| a.0),
            confidence: aligned.map(|a| a.1),
            sample_rate,
        },
    );
}

pub struct Engine {
    controls: Arc<Controls>,
    cmd_tx: Sender<Cmd>,
    session: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// A pending seamless-switch request. A slot rather than a command: the
    /// worker is inside `run_session` while anything plays, so the channel is
    /// not read until the session ends.
    switch_to: Arc<Mutex<Option<String>>>,
}

impl Engine {
    pub fn new(app: AppHandle) -> Engine {
        let controls = Arc::new(Controls::default());
        let session: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::new(Mutex::new(None));
        let switch_to: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let (cmd_tx, cmd_rx) = channel::<Cmd>();

        {
            let controls = controls.clone();
            let session = session.clone();
            let switch_to = switch_to.clone();
            thread::Builder::new()
                .name("ltbr-audio".into())
                .spawn(move || worker(cmd_rx, controls, session, switch_to, app))
                .expect("failed to spawn audio worker");
        }

        Engine {
            controls,
            cmd_tx,
            session,
            switch_to,
        }
    }

    pub fn controls(&self) -> &Arc<Controls> {
        &self.controls
    }

    fn cancel_current(&self) {
        if let Some(flag) = self.session.lock().unwrap().take() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn play(&self, url: String) {
        // A fresh session names its own mount; an older switch request must
        // not override it.
        *self.switch_to.lock().unwrap() = None;
        self.cancel_current();
        let _ = self.cmd_tx.send(Cmd::Play(url));
    }

    pub fn stop(&self) {
        *self.switch_to.lock().unwrap() = None;
        self.cancel_current();
        let _ = self.cmd_tx.send(Cmd::Stop);
    }

    /// Move the running session to another mount without a gap. A no-op when
    /// nothing is playing — the frontend starts playback with the mount it
    /// wants in that case.
    pub fn switch(&self, url: String) {
        if self.session.lock().unwrap().is_some() {
            *self.switch_to.lock().unwrap() = Some(url);
        }
    }
}

fn worker(
    rx: Receiver<Cmd>,
    controls: Arc<Controls>,
    session: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    switch_to: Arc<Mutex<Option<String>>>,
    app: AppHandle,
) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Stop => {
                emit_state(&app, "standby", None);
            }
            Cmd::Play(url) => {
                let stop = Arc::new(AtomicBool::new(false));
                *session.lock().unwrap() = Some(stop.clone());
                run_session(url, stop, &controls, &app, &switch_to);
            }
        }
    }
}

/// Play one URL, reconnecting until `stop`. The URL can change underneath a
/// running session through a seamless switch, which is why it is owned here.
fn run_session(
    mut url: String,
    stop: Arc<AtomicBool>,
    controls: &Arc<Controls>,
    app: &AppHandle,
    switch_to: &Mutex<Option<String>>,
) {
    let mut out = match Output::new() {
        Ok(o) => o,
        Err(e) => {
            emit_error_state(
                app,
                &Fault {
                    code: "device",
                    message: format!("Audio device error: {e}"),
                },
            );
            return;
        }
    };

    let mut backoff_ms = 500u64;
    while !stop.load(Ordering::Relaxed) {
        // A switch asked for between attempts is simply where we retune to.
        if let Some(next) = switch_to.lock().unwrap().take() {
            url = next;
        }
        emit_state(app, "tuning", Some("acquiring…"));

        let link = Link::open(&url);
        let attempt_stop = Arc::new(AtomicBool::new(false));

        let outcome = decode_loop(
            &mut url,
            link,
            &stop,
            &attempt_stop,
            controls,
            &mut out,
            app,
            switch_to,
        );

        // Any deck still alive was shut down by the loop on its way out.
        attempt_stop.store(true, Ordering::SeqCst);

        if stop.load(Ordering::Relaxed) {
            break;
        }

        match outcome {
            DecodeOutcome::Ended => backoff_ms = 500,
            // A resync we chose, not a stream that broke: no fault banner, and
            // no backoff growth — the whole point is to land back on the burst
            // as quickly as possible.
            DecodeOutcome::Resync => {
                backoff_ms = 500;
                emit_state(app, "tuning", Some("re-syncing…"));
            }
            // A switch that could not be done seamlessly (nothing was on air
            // yet, or the mounts differ in sample rate): straight to the new
            // mount, no wait.
            DecodeOutcome::Retune => {
                backoff_ms = 500;
                continue;
            }
            DecodeOutcome::Failed(fault) => {
                emit_state(app, "tuning", Some("reconnecting…"));
                emit_fault(app, &fault);
            }
        }

        // Backoff, staying responsive to stop.
        let mut waited = 0;
        while waited < backoff_ms && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
            waited += 50;
        }
        backoff_ms = (backoff_ms * 2).min(8000);
    }
}

enum DecodeOutcome {
    Ended,
    /// Drift could not be trimmed away; reconnect deliberately.
    Resync,
    /// The session URL has changed and must be reconnected the hard way.
    Retune,
    Failed(Fault),
}

// ---- Link: a connection before it has a decoder ----------------------------

/// The network half of a deck: pipe, pump thread and what it reports.
struct Link {
    url: String,
    pipe: Arc<BytePipe>,
    /// Filled in by the network thread the moment the headers land.
    station: Arc<Mutex<Option<StationInfo>>>,
    net_stop: Arc<AtomicBool>,
    net_handle: Option<JoinHandle<()>>,
    /// Why the pump gave up, if it did. Read when the pipe reaches EOF to tell
    /// a broken connection from a clean one.
    net_error: Arc<Mutex<Option<NetError>>>,
}

impl Link {
    fn open(url: &str) -> Link {
        let pipe = BytePipe::new();
        let station: Arc<Mutex<Option<StationInfo>>> = Arc::new(Mutex::new(None));
        let net_stop = Arc::new(AtomicBool::new(false));
        let net_error: Arc<Mutex<Option<NetError>>> = Arc::new(Mutex::new(None));

        let net_handle = {
            let url = url.to_string();
            let pipe = pipe.clone();
            let net_stop = net_stop.clone();
            let station = station.clone();
            let net_error = net_error.clone();
            thread::Builder::new()
                .name("ltbr-net".into())
                .spawn(move || {
                    if let Err(e) = stream::run(&url, net_stop, pipe, |info| {
                        *station.lock().unwrap() = Some(info);
                    }) {
                        *net_error.lock().unwrap() = Some(e);
                    }
                })
                .ok()
        };

        Link {
            url: url.to_string(),
            pipe,
            station,
            net_stop,
            net_handle,
            net_error,
        }
    }

    fn net_fault(&self) -> Option<Fault> {
        self.net_error.lock().unwrap().clone().map(Fault::from)
    }

    /// Stop the pump and reap its thread off the audio path — a blocking read
    /// on a dead peer can take a while to notice, and the decode loop must
    /// never wait on that.
    fn shutdown(mut self) {
        self.net_stop.store(true, Ordering::SeqCst);
        self.pipe.close();
        if let Some(h) = self.net_handle.take() {
            let _ = thread::Builder::new()
                .name("ltbr-reap".into())
                .spawn(move || {
                    let _ = h.join();
                });
        }
    }
}

// ---- Deck: a connection with its decoder ------------------------------------

enum DeckEnd {
    /// Clean EOF on the pipe.
    Ended,
    Failed(Fault),
}

impl DeckEnd {
    fn into_outcome(self) -> DecodeOutcome {
        match self {
            DeckEnd::Ended => DecodeOutcome::Ended,
            DeckEnd::Failed(f) => DecodeOutcome::Failed(f),
        }
    }
}

/// One stream connection plus its demuxer/decoder and a small PCM FIFO.
///
/// Decoding goes packet -> `fifo`; playback takes fixed [`CHUNK`]s out of the
/// FIFO. Normally the FIFO holds under one packet, but during a switch the
/// incoming deck decodes several seconds ahead so the audio can be searched
/// before it is played. `marks` remembers the byte cursor after each packet
/// so metadata is still released when its audio is *played*, not decoded.
struct Deck {
    link: Link,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    rate: u32,
    /// Exact byte cursor for the demux point. An MP3 packet is its header
    /// plus frame body with nothing in between, so summing packet lengths
    /// tracks the demuxer precisely. The pipe's own read position cannot be
    /// used: symphonia keeps an internal read-ahead of up to 32 KB (~2s).
    demuxed: u64,
    sample_buf: Option<SampleBuffer<f32>>,
    /// Decoded stereo-interleaved audio not yet handed to the DSP.
    fifo: VecDeque<f32>,
    /// `(frames decoded so far, demuxed bytes so far)` after each packet.
    marks: VecDeque<(u64, u64)>,
    decoded_frames: u64,
    played_frames: u64,
    /// `demuxed` as of the last fully played packet.
    played_demuxed: u64,
    // Rolling window for measuring the real byte rate; the production mount
    // does not send `icy-br` despite advertising it, and this is exact anyway.
    rate_bytes: u64,
    rate_frames: u64,
    bitrate_update: Option<u32>,
    splice_errors_left: u32,
}

impl Deck {
    /// Probe the format and open a decoder. Blocks until the pipe delivers
    /// enough to probe — fine for the playing deck, which has nothing else to
    /// do; the incoming deck waits for [`PROBE_BYTES`] first.
    fn probe(link: Link) -> Result<Deck, (Fault, Link)> {
        let mss = MediaSourceStream::new(
            Box::new(PipeReader::new(link.pipe.clone())),
            Default::default(),
        );

        let mut hint = Hint::new();
        hint.mime_type("audio/mpeg");
        hint.with_extension("mp3");

        let probed = match symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        ) {
            Ok(p) => p,
            Err(e) => {
                // EOF during the probe almost always means the connection
                // itself failed; say that rather than "end of stream".
                let fault = link.net_fault().unwrap_or(Fault {
                    code: "decode",
                    message: format!("Stream is not decodable ({e})"),
                });
                return Err((fault, link));
            }
        };
        let format = probed.format;

        let track = match format.default_track() {
            Some(t) => t.clone(),
            None => {
                return Err((
                    Fault {
                        code: "decode",
                        message: "Stream carries no audio track".into(),
                    },
                    link,
                ))
            }
        };
        let decoder = match symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
        {
            Ok(d) => d,
            Err(e) => {
                return Err((
                    Fault {
                        code: "decode",
                        message: format!("Unsupported codec ({e})"),
                    },
                    link,
                ))
            }
        };

        Ok(Deck {
            link,
            format,
            decoder,
            track_id: track.id,
            rate: track.codec_params.sample_rate.unwrap_or(44_100).max(1),
            demuxed: 0,
            sample_buf: None,
            fifo: VecDeque::new(),
            marks: VecDeque::new(),
            decoded_frames: 0,
            played_frames: 0,
            played_demuxed: 0,
            rate_bytes: 0,
            rate_frames: 0,
            bitrate_update: None,
            splice_errors_left: 0,
        })
    }

    fn url(&self) -> &str {
        &self.link.url
    }

    fn pipe(&self) -> &Arc<BytePipe> {
        &self.link.pipe
    }

    fn station(&self) -> Option<StationInfo> {
        self.link.station.lock().unwrap().clone()
    }

    fn shutdown(self) {
        self.link.shutdown();
    }

    /// Decoded frames waiting in the FIFO.
    fn available(&self) -> usize {
        self.fifo.len() / 2
    }

    /// Bytes decoded but not yet played — the FIFO in pipe units.
    fn fifo_bytes(&self) -> u64 {
        self.demuxed.saturating_sub(self.played_demuxed)
    }

    /// Bytes between what is being played and this connection's live edge.
    fn depth_bytes(&self) -> u64 {
        self.pipe().depth(self.played_demuxed)
    }

    /// Decoded frames per demuxed byte, from this deck's own packets.
    fn frames_per_byte(&self) -> Option<f64> {
        (self.demuxed > 0).then(|| self.decoded_frames as f64 / self.demuxed as f64)
    }

    /// Demux and decode one packet into the FIFO. Blocks on the pipe if it is
    /// empty — callers that must not block check `pipe().buffered()` first.
    fn pull(&mut self) -> Result<(), DeckEnd> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(_)) => return Err(self.ended()),
                // A catch-up splices the byte stream, so the demuxer stumbles
                // on the first frame or two afterwards. That is our own doing,
                // not a broken stream — never surface it as a fault.
                Err(SymError::DecodeError(_) | SymError::ResetRequired)
                    if self.splice_errors_left > 0 =>
                {
                    self.splice_errors_left -= 1;
                    continue;
                }
                Err(e) => return Err(self.failed(format!("read: {e}"))),
            };
            self.demuxed += packet.data.len() as u64;
            self.rate_bytes += packet.data.len() as u64;
            self.rate_frames += packet.dur();
            if self.rate_frames >= self.rate as u64 * 2 {
                let ms = self.rate_frames * 1000 / self.rate as u64;
                if ms > 0 {
                    self.bitrate_update = Some((self.rate_bytes * 8 / ms) as u32);
                }
                self.rate_bytes = 0;
                self.rate_frames = 0;
            }
            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymError::DecodeError(_)) => continue, // skip a bad frame
                Err(SymError::IoError(_)) => return Err(self.ended()),
                Err(e) => return Err(self.failed(format!("decode: {e}"))),
            };

            let spec = *decoded.spec();
            let frames = decoded.capacity() as u64;
            let need = frames as usize * spec.channels.count();
            if self.sample_buf.as_ref().map_or(true, |b| b.capacity() < need) {
                self.sample_buf = Some(SampleBuffer::<f32>::new(frames, spec));
            }
            let sbuf = self.sample_buf.as_mut().unwrap();
            sbuf.copy_interleaved_ref(decoded);
            let interleaved = sbuf.samples();
            let in_ch = spec.channels.count().max(1);
            let n = interleaved.len() / in_ch;

            // Everything downstream is stereo; fold or duplicate here.
            self.fifo.reserve(n * 2);
            for f in 0..n {
                let base = f * in_ch;
                if in_ch >= 2 {
                    self.fifo.push_back(interleaved[base]);
                    self.fifo.push_back(interleaved[base + 1]);
                } else {
                    let m = interleaved[base];
                    self.fifo.push_back(m);
                    self.fifo.push_back(m);
                }
            }
            self.decoded_frames += n as u64;
            self.marks.push_back((self.decoded_frames, self.demuxed));
            return Ok(());
        }
    }

    fn ended(&self) -> DeckEnd {
        // EOF with a recorded pump error is a broken connection, not a clean
        // end — that distinction is what the display reports.
        match self.link.net_fault() {
            Some(f) => DeckEnd::Failed(f),
            None => DeckEnd::Ended,
        }
    }

    fn failed(&self, detail: String) -> DeckEnd {
        DeckEnd::Failed(self.link.net_fault().unwrap_or(Fault {
            code: "decode",
            message: format!("Stream is not decodable ({detail})"),
        }))
    }

    /// Decode until at least `frames` are waiting.
    fn fill(&mut self, frames: usize) -> Result<(), DeckEnd> {
        while self.available() < frames {
            self.pull()?;
        }
        Ok(())
    }

    fn advance_marks(&mut self) {
        while self.marks.front().is_some_and(|m| m.0 <= self.played_frames) {
            self.played_demuxed = self.marks.pop_front().unwrap().1;
        }
    }

    /// Hand out the next `frames` (stereo interleaved) into `out`.
    fn take(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<(), DeckEnd> {
        self.fill(frames)?;
        out.clear();
        out.extend(self.fifo.drain(..frames * 2));
        self.played_frames += frames as u64;
        self.advance_marks();
        Ok(())
    }

    /// Throw away the next `frames` unheard.
    fn discard(&mut self, mut frames: u64) -> Result<(), DeckEnd> {
        while frames > 0 {
            let n = frames.min(CHUNK as u64 * 8) as usize;
            self.fill(n)?;
            self.fifo.drain(..n * 2);
            self.played_frames += n as u64;
            frames -= n as u64;
        }
        self.advance_marks();
        Ok(())
    }

    /// Mono copy of decoded frames `[from, to)` in this deck's frame
    /// coordinates, clamped to what the FIFO holds.
    fn mono_range(&self, from: u64, to: u64) -> Vec<f32> {
        let head = self.played_frames;
        let tail = self.decoded_frames;
        let from = from.clamp(head, tail);
        let to = to.clamp(from, tail);
        ((from - head) as usize..(to - head) as usize)
            .map(|f| 0.5 * (self.fifo[f * 2] + self.fifo[f * 2 + 1]))
            .collect()
    }

    fn take_bitrate_update(&mut self) -> Option<u32> {
        self.bitrate_update.take()
    }
}

// ---- Incoming: the seamless-switch state machine ------------------------------

/// Result of matching the incoming deck's audio against the playing one.
struct Alignment {
    /// Incoming frame index that plays at the same instant as playing-deck
    /// frame `anchor`.
    at: i64,
    anchor: u64,
    /// Whether the fine search was trusted, or `at` is the byte estimate.
    fine: bool,
    /// Peak normalised correlation the search found (diagnostics).
    confidence: f32,
}

enum Incoming {
    /// Connected; waiting for enough bytes to probe.
    Linking { link: Link, since: u64 },
    /// Decoding ahead until the audio around the estimate is on hand.
    Priming {
        deck: Deck,
        since: u64,
        retries: u32,
        not_before: u64,
    },
    /// Correlation running on a helper thread.
    Aligning {
        deck: Deck,
        rx: Receiver<Alignment>,
        since: u64,
        retries: u32,
    },
    /// Positioned; crossfading over `total` frames.
    Fading {
        deck: Deck,
        done: usize,
        total: usize,
        fine: bool,
        confidence: f32,
    },
}

impl Incoming {
    fn shutdown(self) {
        match self {
            Incoming::Linking { link, .. } => link.shutdown(),
            Incoming::Priming { deck, .. }
            | Incoming::Aligning { deck, .. }
            | Incoming::Fading { deck, .. } => deck.shutdown(),
        }
    }
}

enum Step {
    Continue(Incoming),
    /// Crossfade complete: the incoming deck is what the speakers carry now,
    /// with whether the alignment was trusted and its correlation.
    Done(Deck, bool, f32),
    /// The switch was abandoned; the playing deck was never touched.
    Failed(Fault),
    /// The mounts cannot be blended (sample rate differs); reconnect hard.
    Retune(String),
}

/// Frame index in the incoming deck that the byte depths say is playing right
/// now on `a`: both mounts carry the same programme and burst the same
/// trailing window on connect, so their live edges coincide and "how far
/// behind the edge" translates straight across.
fn coarse_estimate(a: &Deck, b: &Deck) -> i64 {
    let fpb = b
        .frames_per_byte()
        .or_else(|| a.frames_per_byte())
        .unwrap_or(1152.0 / 418.0);
    let target_bytes = b.pipe().written() as i64 - a.depth_bytes() as i64;
    (target_bytes as f64 * fpb).round() as i64
}

/// One step of the switch, run once per loop iteration right after `chunk`
/// was taken from the playing deck `a`. During the fade `chunk` is mixed in
/// place.
fn advance_switch(
    inc: Incoming,
    a: &mut Deck,
    chunk: &mut [f32],
    b_chunk: &mut Vec<f32>,
    now: u64,
) -> Step {
    let rate = a.rate as f64;
    match inc {
        Incoming::Linking { link, since } => {
            if let Some(f) = link.net_fault() {
                link.shutdown();
                return Step::Failed(f);
            }
            if link.pipe.buffered() >= PROBE_BYTES {
                return match Deck::probe(link) {
                    Ok(deck) => Step::Continue(Incoming::Priming {
                        deck,
                        since: now,
                        retries: 0,
                        not_before: 0,
                    }),
                    Err((f, link)) => {
                        link.shutdown();
                        Step::Failed(f)
                    }
                };
            }
            if link.pipe.is_closed() {
                let f = link.net_fault().unwrap_or(Fault {
                    code: "dropped",
                    message: "Stream ended before it started".into(),
                });
                link.shutdown();
                return Step::Failed(f);
            }
            if now.saturating_sub(since) > LINK_TIMEOUT_MS {
                link.shutdown();
                return Step::Failed(Fault {
                    code: "timeout",
                    message: "Stream did not answer in time".into(),
                });
            }
            Step::Continue(Incoming::Linking { link, since })
        }

        Incoming::Priming {
            mut deck,
            since,
            retries,
            not_before,
        } => {
            if deck.rate != a.rate {
                let url = deck.url().to_string();
                deck.shutdown();
                return Step::Retune(url);
            }

            let ref_frames = (REF_S * rate) as u64;
            let window = (WINDOW_S * rate) as i64;
            let lead = (LEAD_S * rate) as i64;

            let coarse = coarse_estimate(a, &deck);
            // The window must end inside what the incoming deck has decoded.
            let needed = (coarse + window + ref_frames as i64 + CHUNK as i64).max(0) as u64;

            // Decode ahead on the incoming deck — only while its pipe can
            // satisfy each read without touching the network.
            let mut pulls = 0;
            while pulls < PRIME_PULLS
                && (deck.decoded_frames < needed
                    || deck.pipe().buffered() >= PRIME_DRAIN_ABOVE)
                && deck.pipe().buffered() >= PULL_MIN_BYTES
            {
                if let Err(end) = deck.pull() {
                    let f = fault_of(end, "Incoming stream ended");
                    deck.shutdown();
                    return Step::Failed(f);
                }
                pulls += 1;
            }
            // And on the playing deck, so the reference is audio that has
            // not been heard yet (the incoming burst starts roughly where the
            // listener is, so the past would not be on it).
            if coarse >= lead {
                let want = ref_frames as usize + CHUNK * 2;
                let mut pulls = 0;
                while pulls < PRIME_PULLS
                    && a.available() < want
                    && a.pipe().buffered() >= PULL_MIN_BYTES
                {
                    // If the playing deck is dying, the main loop deals with
                    // it on its next take.
                    if a.pull().is_err() {
                        break;
                    }
                    pulls += 1;
                }
            }

            let a_ready = a.available() as u64 >= ref_frames + CHUNK as u64;
            let b_ready = deck.decoded_frames >= needed;
            let timed_out = now.saturating_sub(since) > PRIME_TIMEOUT_MS;

            if (coarse >= lead && a_ready && b_ready && now >= not_before) || timed_out {
                let anchor = a.played_frames;
                let reference = a.mono_range(anchor, anchor + ref_frames);
                let lo = (coarse - window).max(deck.played_frames as i64) as u64;
                let hi = (coarse + window + ref_frames as i64).max(lo as i64) as u64;
                let hay = deck.mono_range(lo, hi);
                let rx = spawn_align(reference, hay, lo, coarse, anchor);
                return Step::Continue(Incoming::Aligning {
                    deck,
                    rx,
                    since,
                    retries,
                });
            }
            Step::Continue(Incoming::Priming {
                deck,
                since,
                retries,
                not_before,
            })
        }

        Incoming::Aligning {
            mut deck,
            rx,
            since,
            retries,
        } => match rx.try_recv() {
            Ok(al) if !al.fine
                && retries < ALIGN_RETRIES
                && now.saturating_sub(since) <= PRIME_TIMEOUT_MS =>
            {
                // Probably a voice link on one mount only; try again shortly
                // with fresher audio rather than settle for the estimate.
                Step::Continue(Incoming::Priming {
                    deck,
                    since,
                    retries: retries + 1,
                    not_before: now + ALIGN_RETRY_GAP_MS,
                })
            }
            Ok(al) => {
                // Position the incoming deck so its next frame is the one
                // that plays at the same instant as the playing deck's next
                // frame. `a` has already moved on by the frames taken since
                // the snapshot; the offset carries across unchanged.
                let delta = al.at - al.anchor as i64;
                let target = a.played_frames as i64 + delta;
                let positioned = if target >= deck.played_frames as i64 {
                    deck.discard((target - deck.played_frames as i64) as u64)
                } else {
                    // The matching audio predates what the incoming burst
                    // holds: the only gap-free option is to step the playing
                    // deck forward to meet it.
                    a.discard((deck.played_frames as i64 - target) as u64)
                };
                if let Err(end) = positioned {
                    let f = fault_of(end, "Incoming stream ended");
                    deck.shutdown();
                    return Step::Failed(f);
                }
                Step::Continue(Incoming::Fading {
                    deck,
                    done: 0,
                    total: (FADE_S * rate) as usize,
                    fine: al.fine,
                    confidence: al.confidence,
                })
            }
            Err(TryRecvError::Empty) => Step::Continue(Incoming::Aligning {
                deck,
                rx,
                since,
                retries,
            }),
            Err(TryRecvError::Disconnected) => {
                deck.shutdown();
                Step::Failed(Fault {
                    code: "decode",
                    message: "Alignment failed".into(),
                })
            }
        },

        Incoming::Fading {
            mut deck,
            done,
            total,
            fine,
            confidence,
        } => {
            if let Err(end) = deck.take(chunk.len() / 2, b_chunk) {
                // The incoming side died mid-fade. `chunk` is still the
                // playing deck's own audio, so nothing is lost by staying.
                let f = fault_of(end, "Incoming stream ended");
                deck.shutdown();
                return Step::Failed(f);
            }
            let frames = chunk.len() / 2;
            for i in 0..frames {
                let t = ((done + i) as f64 / total as f64).min(1.0) as f32;
                let ga = 1.0 - t;
                chunk[i * 2] = chunk[i * 2] * ga + b_chunk[i * 2] * t;
                chunk[i * 2 + 1] = chunk[i * 2 + 1] * ga + b_chunk[i * 2 + 1] * t;
            }
            let done = done + frames;
            if done >= total {
                Step::Done(deck, fine, confidence)
            } else {
                Step::Continue(Incoming::Fading {
                    deck,
                    done,
                    total,
                    fine,
                    confidence,
                })
            }
        }
    }
}

fn fault_of(end: DeckEnd, clean_eof: &str) -> Fault {
    match end {
        DeckEnd::Failed(f) => f,
        DeckEnd::Ended => Fault {
            code: "dropped",
            message: clean_eof.into(),
        },
    }
}

/// Run the correlation off the audio thread. `reference` is playing-deck
/// audio starting at frame `anchor`; `hay` is incoming-deck audio starting at
/// frame `lo`; `coarse` is the byte-depth estimate for `anchor` in incoming
/// coordinates, used when the search is not trusted.
fn spawn_align(
    reference: Vec<f32>,
    hay: Vec<f32>,
    lo: u64,
    coarse: i64,
    anchor: u64,
) -> Receiver<Alignment> {
    let (tx, rx) = channel();
    let _ = thread::Builder::new()
        .name("ltbr-align".into())
        .spawn(move || {
            let found = align::locate(&reference, &hay);
            #[cfg(test)]
            eprintln!(
                "align: ref {} frames @A{anchor}, hay {} frames @B{lo}, coarse B{coarse} -> {:?}",
                reference.len(),
                hay.len(),
                found
            );
            let result = match found {
                Some((idx, ncc)) if ncc >= MIN_CONFIDENCE => Alignment {
                    at: lo as i64 + idx as i64,
                    anchor,
                    fine: true,
                    confidence: ncc,
                },
                other => Alignment {
                    at: coarse.max(lo as i64),
                    anchor,
                    fine: false,
                    confidence: other.map_or(0.0, |(_, ncc)| ncc),
                },
            };
            let _ = tx.send(result);
        });
    rx
}

// ---- the decode loop ----------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    url: &mut String,
    link: Link,
    stop: &AtomicBool,
    attempt_stop: &AtomicBool,
    controls: &Arc<Controls>,
    out: &mut Output,
    app: &AppHandle,
    switch_to: &Mutex<Option<String>>,
) -> DecodeOutcome {
    let mut active = match Deck::probe(link) {
        Ok(d) => d,
        Err((fault, link)) => {
            link.shutdown();
            return DecodeOutcome::Failed(fault);
        }
    };

    let mut state: Option<SessionState> = None;
    let mut live = false;

    // Drift lives per attempt, so every reconnect re-settles and re-baselines
    // from scratch — which is exactly right after a suspend/resume, where the
    // clock jumped and the socket usually died anyway.
    let seed_bitrate = active.station().map_or(0, |s| s.bitrate_kbps);
    let mut drift = Drift::new(seed_bitrate);
    let mut last_title = String::new();
    let mut station_sent = false;
    let mut last_sync_emit: u64 = 0;

    let mut incoming: Option<Incoming> = None;
    let mut chunk: Vec<f32> = Vec::with_capacity(CHUNK * 2);
    let mut b_chunk: Vec<f32> = Vec::with_capacity(CHUNK * 2);

    let outcome = loop {
        if stop.load(Ordering::Relaxed) || attempt_stop.load(Ordering::Relaxed) {
            break DecodeOutcome::Ended;
        }

        // A switch request. Before anything is on air there is nothing to
        // blend, so simply retune; otherwise open the incoming deck alongside.
        if incoming.is_none() {
            if let Some(next) = switch_to.lock().unwrap().take() {
                if next != *url {
                    if !live {
                        *url = next;
                        break DecodeOutcome::Retune;
                    }
                    emit_source(app, &next, "switching", None, None, None);
                    incoming = Some(Incoming::Linking {
                        link: Link::open(&next),
                        since: sync::wall_ms(),
                    });
                }
            }
        }

        if let Err(end) = active.take(CHUNK, &mut chunk) {
            // The playing deck died. If the replacement is already aligned
            // and fading in, finish the swap on the spot rather than drop to
            // a reconnect — a hard cut beats silence.
            match incoming.take() {
                Some(Incoming::Fading { deck, fine, confidence, .. }) => {
                    swap_in(
                        &mut active, deck, url, app, &mut drift, &mut last_title,
                        &mut station_sent, (fine, confidence),
                    );
                    if active.take(CHUNK, &mut chunk).is_err() {
                        break end.into_outcome();
                    }
                }
                other => {
                    if let Some(inc) = other {
                        inc.shutdown();
                    }
                    break end.into_outcome();
                }
            }
        }

        let now = sync::wall_ms();

        if let Some(inc) = incoming.take() {
            match advance_switch(inc, &mut active, &mut chunk, &mut b_chunk, now) {
                Step::Continue(inc) => incoming = Some(inc),
                Step::Done(deck, fine, confidence) => {
                    swap_in(
                        &mut active, deck, url, app, &mut drift, &mut last_title,
                        &mut station_sent, (fine, confidence),
                    );
                }
                Step::Failed(fault) => {
                    emit_source(app, url, "failed", Some(&fault.message), None, None);
                }
                Step::Retune(next) => {
                    *url = next;
                    break DecodeOutcome::Retune;
                }
            }
        }

        let st = state.get_or_insert_with(|| {
            let app = app.clone();
            SessionState::new(
                active.rate,
                out.sample_rate,
                out.channels,
                controls.clone(),
                Box::new(move |bars| {
                    let _ = app.emit("spectrum", bars.to_vec());
                }),
            )
        });

        st.process(&chunk, 2, out, stop, attempt_stop);

        if !live {
            live = true;
            emit_state(app, "live", None);
            emit_source(app, url, "live", None, None, Some(active.rate));
            drift.first_packet(now);
        }

        if let Some(kbps) = active.take_bitrate_update() {
            drift.set_bitrate(kbps);
        }

        // Metadata and drift are handled AFTER the (possibly blocking) push to
        // the output, so a release is never held up by a full ring and the
        // poll cadence is paced by real playback time.
        let tick = active.pipe().tick(active.played_demuxed);
        if !tick.released.is_empty() {
            let info = active
                .station()
                .map(|i| station_payload(&i, drift.bitrate_kbps()));
            for block in &tick.released {
                let with_station = if station_sent { None } else { info.clone() };
                station_sent = true;
                emit_block(app, block, &mut drift, &mut last_title, with_station, now);
            }
        }

        match drift.poll(tick.depth_bytes, out.queued_ms(), now) {
            sync::Action::None => {}
            sync::Action::CatchUp { keep_bytes } => {
                // Audio already decoded into the FIFO is beyond a trim's
                // reach, so it counts against the keep budget.
                let keep = keep_bytes.saturating_sub(active.fifo_bytes() as usize);
                let trimmed = active.pipe().trim_to(keep, active.demuxed);
                if trimmed.bytes > 0 {
                    drift.on_trimmed(trimmed.bytes);
                    active.splice_errors_left = SPLICE_TOLERANCE;
                    last_sync_emit = now;
                    emit_sync(app, &drift, "catchup");
                }
            }
            sync::Action::Reconnect => {
                emit_sync(app, &drift, "reconnect");
                break DecodeOutcome::Resync;
            }
        }

        // Keep the lag readout live without flooding the IPC.
        if now.saturating_sub(last_sync_emit) >= 1000 {
            last_sync_emit = now;
            emit_sync(app, &drift, "none");
        }
    };

    if let Some(inc) = incoming.take() {
        inc.shutdown();
    }
    active.shutdown();
    outcome
}

/// Make the incoming deck the playing one. The old deck is shut down, the
/// session URL follows, drift re-settles on the new connection, and the
/// newest metadata block the incoming audio has passed is published so the
/// display describes what is now heard.
#[allow(clippy::too_many_arguments)]
fn swap_in(
    active: &mut Deck,
    incoming: Deck,
    url: &mut String,
    app: &AppHandle,
    drift: &mut Drift,
    last_title: &mut String,
    station_sent: &mut bool,
    aligned: (bool, f32),
) {
    let old = std::mem::replace(active, incoming);
    old.shutdown();
    *url = active.url().to_string();

    let now = sync::wall_ms();
    let seed = active.station().map_or(drift.bitrate_kbps(), |s| {
        if s.bitrate_kbps > 0 { s.bitrate_kbps } else { drift.bitrate_kbps() }
    });
    *drift = Drift::new(seed);
    drift.first_packet(now);
    *station_sent = false;

    let tick = active.pipe().tick(active.played_demuxed);
    if let Some(block) = tick.released.last() {
        let info = active
            .station()
            .map(|i| station_payload(&i, drift.bitrate_kbps()));
        *station_sent = true;
        emit_block(app, block, drift, last_title, info, now);
    }

    emit_source(app, url, "live", None, Some(aligned), Some(active.rate));
}

// ---- audio alignment ------------------------------------------------------------

/// Locating one stream inside another by normalised cross-correlation.
///
/// Both mounts are the same programme through two encoders, so the match is
/// strong (measured ~0.99 at full rate against the real streams). A coarse
/// pass on 8x-decimated audio finds the neighbourhood; a full-rate pass over
/// a few samples either side pins it down.
pub mod align {
    const DECIMATE: usize = 8;
    const REFINE: isize = 12;

    fn decimate(x: &[f32]) -> Vec<f32> {
        x.chunks_exact(DECIMATE)
            .map(|c| c.iter().sum::<f32>() / DECIMATE as f32)
            .collect()
    }

    /// Normalised correlation of `r` against `h[at..at+r.len()]`.
    fn ncc(r: &[f32], h: &[f32], at: usize, e_ref: f64) -> f32 {
        let seg = &h[at..at + r.len()];
        let mut dot = 0.0f64;
        let mut e = 0.0f64;
        for (a, b) in r.iter().zip(seg) {
            dot += (*a as f64) * (*b as f64);
            e += (*b as f64) * (*b as f64);
        }
        if e < 1e-9 || e_ref < 1e-9 {
            return 0.0;
        }
        (dot / (e * e_ref).sqrt()) as f32
    }

    /// Index into `hay` where `reference` begins, with the normalised
    /// correlation there (1.0 = identical). `None` when `hay` is shorter than
    /// `reference` or either is silent.
    pub fn locate(reference: &[f32], hay: &[f32]) -> Option<(usize, f32)> {
        if reference.len() < DECIMATE * 4 || hay.len() < reference.len() {
            return None;
        }
        let rd = decimate(reference);
        let hd = decimate(hay);
        if hd.len() < rd.len() {
            return None;
        }

        let e_ref: f64 = rd.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        if e_ref < 1e-9 {
            return None;
        }
        // Running window energy over the haystack via a prefix sum.
        let mut prefix = vec![0.0f64; hd.len() + 1];
        for (i, v) in hd.iter().enumerate() {
            prefix[i + 1] = prefix[i] + (*v as f64) * (*v as f64);
        }

        let mut best = (0usize, f32::MIN);
        for lag in 0..=(hd.len() - rd.len()) {
            let e = prefix[lag + rd.len()] - prefix[lag];
            if e < 1e-9 {
                continue;
            }
            let mut dot = 0.0f64;
            for (a, b) in rd.iter().zip(&hd[lag..]) {
                dot += (*a as f64) * (*b as f64);
            }
            let v = (dot / (e * e_ref).sqrt()) as f32;
            if v > best.1 {
                best = (lag, v);
            }
        }
        if best.1 == f32::MIN {
            return None;
        }

        // Full-rate refinement around the decimated peak.
        let e_full: f64 = reference.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        let centre = (best.0 * DECIMATE) as isize;
        let max_at = (hay.len() - reference.len()) as isize;
        let mut fine = (centre.clamp(0, max_at) as usize, f32::MIN);
        for j in (centre - REFINE)..=(centre + REFINE) {
            if j < 0 || j > max_at {
                continue;
            }
            let v = ncc(reference, hay, j as usize, e_full);
            if v > fine.1 {
                fine = (j as usize, v);
            }
        }
        Some(fine)
    }
}

/// Per-session mutable audio state: DSP, spectrum, optional resampler.
/// AppHandle-free so it can be driven both by the Tauri engine and by the
/// standalone `audio_probe` example / tests.
pub struct SessionState {
    dsp: Dsp,
    spectrum: Spectrum,
    app_channels: usize,
    resampler: Option<SincFixedIn<f32>>,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    scratch: Vec<f32>,
    on_spectrum: Box<dyn FnMut([f32; crate::spectrum::BARS]) + Send>,
}

impl SessionState {
    pub fn new(
        in_rate: u32,
        out_rate: u32,
        out_channels: usize,
        controls: Arc<Controls>,
        on_spectrum: Box<dyn FnMut([f32; crate::spectrum::BARS]) + Send>,
    ) -> Self {
        let resampler = if in_rate != out_rate {
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            SincFixedIn::<f32>::new(
                out_rate as f64 / in_rate as f64,
                2.0,
                params,
                RESAMPLE_CHUNK,
                2,
            )
            .ok()
        } else {
            None
        };

        SessionState {
            dsp: Dsp::new(in_rate as f32, controls),
            spectrum: Spectrum::new(in_rate as f32),
            app_channels: out_channels,
            resampler,
            in_l: Vec::with_capacity(RESAMPLE_CHUNK * 2),
            in_r: Vec::with_capacity(RESAMPLE_CHUNK * 2),
            scratch: Vec::new(),
            on_spectrum,
        }
    }

    pub fn process(
        &mut self,
        interleaved: &[f32],
        in_ch: usize,
        out: &mut Output,
        stop: &AtomicBool,
        attempt_stop: &AtomicBool,
    ) {
        self.render(interleaved, in_ch);
        push_all(out, &self.scratch, stop, attempt_stop);
    }

    /// DSP + resample one decoded packet into `self.scratch` (device-layout
    /// interleaved). Separated from the output push so it can be unit-tested.
    fn render(&mut self, interleaved: &[f32], in_ch: usize) {
        let frames = interleaved.len() / in_ch;
        self.scratch.clear();

        for f in 0..frames {
            let base = f * in_ch;
            let (l, r) = if in_ch >= 2 {
                (interleaved[base], interleaved[base + 1])
            } else {
                let m = interleaved[base];
                (m, m)
            };

            let (ol, or, tap) = self.dsp.process_frame(l, r);

            if let Some(bars) = self.spectrum.push(tap) {
                (self.on_spectrum)(bars);
            }

            match &mut self.resampler {
                Some(_) => {
                    self.in_l.push(ol);
                    self.in_r.push(or);
                }
                None => interleave_into(&mut self.scratch, ol, or, self.app_channels),
            }
        }

        // Drain any full resampler chunks.
        if self.resampler.is_some() {
            self.drain_resampler();
        }
    }

    fn drain_resampler(&mut self) {
        let ch = self.app_channels;
        let resampler = self.resampler.as_mut().unwrap();
        while self.in_l.len() >= RESAMPLE_CHUNK {
            let l: Vec<f32> = self.in_l.drain(..RESAMPLE_CHUNK).collect();
            let r: Vec<f32> = self.in_r.drain(..RESAMPLE_CHUNK).collect();
            if let Ok(outbuf) = resampler.process(&[l, r], None) {
                let n = outbuf[0].len();
                for i in 0..n {
                    interleave_into(&mut self.scratch, outbuf[0][i], outbuf[1][i], ch);
                }
            }
        }
    }
}

#[inline]
fn interleave_into(buf: &mut Vec<f32>, l: f32, r: f32, channels: usize) {
    match channels {
        0 => {}
        1 => buf.push(0.5 * (l + r)),
        _ => {
            buf.push(l);
            buf.push(r);
            for _ in 2..channels {
                buf.push(0.0);
            }
        }
    }
}

/// Push every sample, spinning briefly when the ring is full. Bails on stop.
fn push_all(out: &mut Output, data: &[f32], stop: &AtomicBool, attempt_stop: &AtomicBool) {
    let mut off = 0;
    while off < data.len() {
        off += out.push(&data[off..]);
        if off < data.len() {
            if stop.load(Ordering::Relaxed) || attempt_stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise the resampler path (48 kHz stream -> 44.1 kHz device) that this
    /// dev machine's matched-rate device does not hit at runtime.
    #[test]
    fn resamples_48k_to_44k_cleanly() {
        let controls = Arc::new(Controls::default());
        controls.set_volume(1.0);
        let mut st = SessionState::new(48_000, 44_100, 2, controls, Box::new(|_| {}));
        assert!(st.resampler.is_some(), "resampler should be active");

        // Feed 1 second of a 440 Hz stereo sine at 48 kHz.
        let n = 48_000;
        let mut interleaved = Vec::with_capacity(n * 2);
        for i in 0..n {
            let x = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin() * 0.5;
            interleaved.push(x);
            interleaved.push(x);
        }
        st.render(&interleaved, 2);

        // Output is stereo-interleaved; expect ~44.1k frames (down from 48k),
        // all finite and within range.
        let frames = st.scratch.len() / 2;
        assert!(
            (40_000..=44_100).contains(&frames),
            "unexpected resampled frame count: {frames}"
        );
        assert!(
            st.scratch.iter().all(|s| s.is_finite() && s.abs() <= 1.5),
            "resampled output has bad samples"
        );
    }

    #[test]
    fn matched_rate_has_no_resampler() {
        let controls = Arc::new(Controls::default());
        let st = SessionState::new(44_100, 44_100, 2, controls, Box::new(|_| {}));
        assert!(st.resampler.is_none());
    }

    /// Deterministic "programme": a sum of drifting tones with a little
    /// noise, so autocorrelation has one clear peak.
    fn programme(n: usize, seed: u32, f0: f32) -> Vec<f32> {
        let mut s = seed;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        (0..n)
            .map(|i| {
                let t = i as f32 / 44_100.0;
                0.3 * (2.0 * std::f32::consts::PI * f0 * t).sin()
                    + 0.2 * (2.0 * std::f32::consts::PI * (1.5 * f0 + 40.0 * t) * t).sin()
                    + 0.15 * rnd()
            })
            .collect()
    }

    #[test]
    fn align_finds_the_exact_offset() {
        let src = programme(44_100 * 5, 7, 220.0);
        let offset = 44_100 * 2 + 517; // reference starts here inside the source
        let reference = src[offset..offset + 44_100 * 3 / 2].to_vec();
        // Haystack: 3.5 s window that contains it, through a "second encoder"
        // (mild gain + independent noise).
        let hay: Vec<f32> = src[44_100..44_100 * 9 / 2]
            .iter()
            .enumerate()
            .map(|(i, v)| v * 0.97 + 0.01 * ((i * 7919) % 13) as f32 / 13.0)
            .collect();
        let (at, ncc) = align::locate(&reference, &hay).expect("should locate");
        assert_eq!(at, offset - 44_100, "sample-exact alignment expected");
        assert!(ncc > 0.9, "weak correlation {ncc}");
    }

    #[test]
    fn align_rejects_unrelated_audio() {
        let reference = programme(44_100, 3, 220.0);
        let hay = programme(44_100 * 2, 99, 587.0);
        let (_, ncc) = align::locate(&reference, &hay).expect("should still search");
        assert!(ncc < MIN_CONFIDENCE, "unrelated audio scored {ncc}");
    }

    /// Drive the real switch machinery against the station's two live mounts,
    /// with no sound card: chunks are paced by sleeping. Prints the alignment
    /// it found and how closely the two decks agree at the first fade step.
    /// Needs the network, so it is opt-in: `cargo test -- --ignored switch_live`.
    #[test]
    #[ignore]
    fn switch_live_mounts_align() {
        let a_url = "https://stream.ltbr.fm/live";
        let b_url = "https://stream.ltbr.fm/live-nodj";

        let mut a = Deck::probe(Link::open(a_url)).map_err(|(f, _)| f.message).unwrap();
        let period = Duration::from_micros(1_000_000 * CHUNK as u64 / a.rate as u64);
        let mut chunk = Vec::with_capacity(CHUNK * 2);
        let mut b_chunk = Vec::with_capacity(CHUNK * 2);

        // Let the playing deck settle into the burst like a real listener.
        for _ in 0..(3.0 * a.rate as f64 / CHUNK as f64) as usize {
            a.take(CHUNK, &mut chunk).map_err(|_| "A ended").unwrap();
            thread::sleep(period);
        }
        eprintln!("A depth {} ms behind edge", sync::bytes_to_ms(a.depth_bytes(), 128));

        let t0 = std::time::Instant::now();
        let mut inc = Some(Incoming::Linking { link: Link::open(b_url), since: sync::wall_ms() });
        let mut phase = "linking";
        let mut residual: Option<f32> = None;
        let mut result = None;

        while result.is_none() {
            a.take(CHUNK, &mut chunk).map_err(|_| "A ended").unwrap();
            let before = chunk.clone();
            match advance_switch(inc.take().unwrap(), &mut a, &mut chunk, &mut b_chunk, sync::wall_ms()) {
                Step::Continue(next) => {
                    let now = match &next {
                        Incoming::Linking { .. } => "linking",
                        Incoming::Priming { .. } => "priming",
                        Incoming::Aligning { .. } => "aligning",
                        Incoming::Fading { .. } => "fading",
                    };
                    if now != phase {
                        eprintln!("{:>6} ms  {phase} -> {now}", t0.elapsed().as_millis());
                        phase = now;
                    }
                    if let Incoming::Fading { done, .. } = &next {
                        if residual.is_none() && *done > 0 {
                            // First fade step: `before` is pure A, `b_chunk` pure B.
                            let (mut num, mut den) = (0.0f64, 0.0f64);
                            for (x, y) in before.iter().zip(&b_chunk) {
                                num += ((x - y) * (x - y)) as f64;
                                den += (x * x) as f64;
                            }
                            residual = Some((num / den.max(1e-12)).sqrt() as f32);
                        }
                    }
                    inc = Some(next);
                }
                Step::Done(deck, fine, _) => result = Some(Ok((deck, fine))),
                Step::Failed(f) => result = Some(Err(f.message)),
                Step::Retune(u) => result = Some(Err(format!("retune to {u}"))),
            }
            thread::sleep(period);
        }

        let (b, fine) = result.unwrap().expect("switch should complete");
        let residual = residual.expect("fade never ran");
        eprintln!(
            "{:>6} ms  done: fine={fine} residual={residual:.3} (rms(A-B)/rms(A) at fade start)",
            t0.elapsed().as_millis()
        );
        a.shutdown();
        b.shutdown();
        assert!(fine, "expected a trusted correlation against the live mounts");
        // Two encoders of one programme, sample-aligned, differ by a few percent.
        assert!(residual < 0.35, "decks disagree too much at the fade: {residual}");
    }

    #[test]
    fn align_needs_a_haystack_at_least_as_long() {
        let reference = programme(44_100, 1, 220.0);
        let hay = programme(22_050, 1, 220.0);
        assert!(align::locate(&reference, &hay).is_none());
    }
}
