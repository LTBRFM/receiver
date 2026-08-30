//! Engine orchestration: owns a long-lived worker thread that plays a stream
//! session (network -> decode -> DSP -> resample -> output) and reconnects
//! forever until told to stop. Tauri commands mutate [`Controls`] atomics and
//! send [`Cmd`]s here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tauri::{AppHandle, Emitter};

use crate::dsp::{Controls, Dsp, NUM_BANDS};
use crate::output::Output;
use crate::spectrum::Spectrum;
use crate::icy;
use crate::stream::{self, BytePipe, PipeReader, StationInfo};
use crate::sync::{self, Drift};

const RESAMPLE_CHUNK: usize = 1024;

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

#[derive(serde::Serialize, Clone)]
struct StatePayload {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn emit_state(app: &AppHandle, state: &'static str, message: Option<&str>) {
    let _ = app.emit(
        "state",
        StatePayload {
            state,
            message: message.map(|s| s.to_string()),
        },
    );
}

pub struct Engine {
    controls: Arc<Controls>,
    cmd_tx: Sender<Cmd>,
    session: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

impl Engine {
    pub fn new(app: AppHandle) -> Engine {
        let controls = Arc::new(Controls::default());
        let session: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::new(Mutex::new(None));
        let (cmd_tx, cmd_rx) = channel::<Cmd>();

        {
            let controls = controls.clone();
            let session = session.clone();
            thread::Builder::new()
                .name("ltbr-audio".into())
                .spawn(move || worker(cmd_rx, controls, session, app))
                .expect("failed to spawn audio worker");
        }

        Engine {
            controls,
            cmd_tx,
            session,
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
        self.cancel_current();
        let _ = self.cmd_tx.send(Cmd::Play(url));
    }

    pub fn stop(&self) {
        self.cancel_current();
        let _ = self.cmd_tx.send(Cmd::Stop);
    }
}

fn worker(
    rx: Receiver<Cmd>,
    controls: Arc<Controls>,
    session: Arc<Mutex<Option<Arc<AtomicBool>>>>,
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
                run_session(&url, stop, &controls, &app);
            }
        }
    }
}

/// Play one URL, reconnecting until `stop`.
fn run_session(url: &str, stop: Arc<AtomicBool>, controls: &Arc<Controls>, app: &AppHandle) {
    let mut out = match Output::new() {
        Ok(o) => o,
        Err(e) => {
            emit_state(app, "error", Some(&format!("Audio device error: {e}")));
            return;
        }
    };

    let mut backoff_ms = 500u64;
    while !stop.load(Ordering::Relaxed) {
        emit_state(app, "tuning", Some("acquiring…"));

        let pipe = BytePipe::new();
        let attempt_stop = Arc::new(AtomicBool::new(false));
        // Filled in by the network thread the moment the headers land; the
        // decode loop reads it to name the station and seed the bitrate.
        let station: Arc<Mutex<Option<StationInfo>>> = Arc::new(Mutex::new(None));

        // combined stop for the network thread
        let net_stop = Arc::new(AtomicBool::new(false));
        let net_handle = {
            let url = url.to_string();
            let pipe = pipe.clone();
            let net_stop = net_stop.clone();
            let station = station.clone();
            thread::Builder::new()
                .name("ltbr-net".into())
                .spawn(move || {
                    let _ = stream::run(&url, net_stop, pipe, |info| {
                        *station.lock().unwrap() = Some(info);
                    });
                })
                .ok()
        };

        let outcome = decode_loop(&pipe, &station, &stop, &attempt_stop, controls, &mut out, app);

        // Tear the attempt down.
        attempt_stop.store(true, Ordering::SeqCst);
        net_stop.store(true, Ordering::SeqCst);
        pipe.close();
        if let Some(h) = net_handle {
            let _ = h.join();
        }

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
            DecodeOutcome::Failed(msg) => {
                emit_state(app, "tuning", Some(&format!("reconnecting… ({msg})")));
                let _ = app.emit(
                    "fault",
                    serde_json::json!({ "message": format!("Stream dropped: {msg}. Reconnecting…") }),
                );
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
    Failed(String),
}

fn decode_loop(
    pipe: &Arc<BytePipe>,
    station: &Arc<Mutex<Option<StationInfo>>>,
    stop: &AtomicBool,
    attempt_stop: &AtomicBool,
    controls: &Arc<Controls>,
    out: &mut Output,
    app: &AppHandle,
) -> DecodeOutcome {
    let mss = MediaSourceStream::new(Box::new(PipeReader::new(pipe.clone())), Default::default());

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
        Err(e) => return DecodeOutcome::Failed(format!("probe: {e}")),
    };
    let mut format = probed.format;

    let track = match format.default_track() {
        Some(t) => t.clone(),
        None => return DecodeOutcome::Failed("no audio track".into()),
    };
    let mut decoder = match symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
    {
        Ok(d) => d,
        Err(e) => return DecodeOutcome::Failed(format!("codec: {e}")),
    };
    let track_id = track.id;

    let mut state: Option<SessionState> = None;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut live = false;

    let in_rate = track.codec_params.sample_rate.unwrap_or(44_100).max(1) as u64;

    // Exact byte cursor for the demux point. An MP3 packet is its header plus
    // frame body with nothing in between, so summing packet lengths tracks the
    // demuxer precisely. The pipe's own read position cannot be used: symphonia
    // keeps an internal read-ahead of up to 32 KB (~2s) that is neither
    // visible nor shrinkable — `buffer_len` is asserted to be a power of two
    // AND larger than the block size.
    let mut demuxed: u64 = 0;

    // Drift lives per attempt, so every reconnect re-settles and re-baselines
    // from scratch — which is exactly right after a suspend/resume, where the
    // clock jumped and the socket usually died anyway.
    let seed_bitrate = station.lock().unwrap().as_ref().map_or(0, |s| s.bitrate_kbps);
    let mut drift = Drift::new(seed_bitrate);
    let mut last_title = String::new();
    let mut station_sent = false;
    let mut splice_errors_left: u32 = 0;
    let mut last_sync_emit: u64 = 0;
    // Rolling window for measuring the real byte rate; the production mount
    // does not send `icy-br` despite advertising it, and this is exact anyway.
    let (mut rate_bytes, mut rate_frames) = (0u64, 0u64);

    loop {
        if stop.load(Ordering::Relaxed) || attempt_stop.load(Ordering::Relaxed) {
            return DecodeOutcome::Ended;
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(_)) => return DecodeOutcome::Ended,
            // A catch-up splices the byte stream, so the demuxer stumbles on
            // the first frame or two afterwards. That is our own doing, not a
            // broken stream — never surface it as a fault.
            Err(SymError::DecodeError(_) | SymError::ResetRequired)
                if splice_errors_left > 0 =>
            {
                splice_errors_left -= 1;
                continue;
            }
            Err(e) => return DecodeOutcome::Failed(format!("read: {e}")),
        };
        demuxed += packet.data.len() as u64;
        rate_bytes += packet.data.len() as u64;
        rate_frames += packet.dur();
        if rate_frames >= in_rate * 2 {
            let ms = rate_frames * 1000 / in_rate;
            if ms > 0 {
                drift.set_bitrate((rate_bytes * 8 / ms) as u32);
            }
            rate_bytes = 0;
            rate_frames = 0;
        }
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue, // skip a bad frame
            Err(SymError::IoError(_)) => return DecodeOutcome::Ended,
            Err(e) => return DecodeOutcome::Failed(format!("decode: {e}")),
        };

        let spec = *decoded.spec();
        let frames = decoded.capacity() as u64;
        let sbuf = sample_buf.get_or_insert_with(|| SampleBuffer::<f32>::new(frames, spec));
        sbuf.copy_interleaved_ref(decoded);
        let interleaved = sbuf.samples();
        let in_ch = spec.channels.count().max(1);

        let st = state.get_or_insert_with(|| {
            let app = app.clone();
            SessionState::new(
                spec.rate,
                out.sample_rate,
                out.channels,
                controls.clone(),
                Box::new(move |bars| {
                    let _ = app.emit("spectrum", bars.to_vec());
                }),
            )
        });

        st.process(interleaved, in_ch, out, stop, attempt_stop);

        if !live {
            live = true;
            emit_state(app, "live", None);
            drift.first_packet(sync::wall_ms());
        }

        // Metadata and drift are handled AFTER the (possibly blocking) push to
        // the output, so a release is never held up by a full ring and the
        // poll cadence is paced by real playback time.
        let now = sync::wall_ms();
        let tick = pipe.tick(demuxed);
        if !tick.released.is_empty() {
            let info = station
                .lock()
                .unwrap()
                .as_ref()
                .map(|i| station_payload(i, drift.bitrate_kbps()));
            for block in &tick.released {
                let with_station = if station_sent { None } else { info.clone() };
                station_sent = true;
                emit_block(app, block, &mut drift, &mut last_title, with_station, now);
            }
        }

        match drift.poll(tick.depth_bytes, out.queued_ms(), now) {
            sync::Action::None => {}
            sync::Action::CatchUp { keep_bytes } => {
                let trimmed = pipe.trim_to(keep_bytes, demuxed);
                if trimmed.bytes > 0 {
                    drift.on_trimmed(trimmed.bytes);
                    splice_errors_left = SPLICE_TOLERANCE;
                    last_sync_emit = now;
                    emit_sync(app, &drift, "catchup");
                }
            }
            sync::Action::Reconnect => {
                emit_sync(app, &drift, "reconnect");
                return DecodeOutcome::Resync;
            }
        }

        // Keep the lag readout live without flooding the IPC.
        if now.saturating_sub(last_sync_emit) >= 1000 {
            last_sync_emit = now;
            emit_sync(app, &drift, "none");
        }
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
}
