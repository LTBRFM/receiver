//! End-to-end audio probe (no GUI). Plays the live stream through the real
//! production audio path — network -> ICY demux -> symphonia -> DSP -> resample
//! -> cpal — while continuously sweeping the EQ and volume, so you can *hear*
//! whether control changes are artifact-free.
//!
//!   cargo run --example audio_probe                       # default stream, 6s
//!   cargo run --example audio_probe <url> <seconds>

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ltbrfm_player_lib::dsp::Controls;
use ltbrfm_player_lib::engine::SessionState;
use ltbrfm_player_lib::output::Output;
use ltbrfm_player_lib::stream::{self, BytePipe, PipeReader};
use ltbrfm_player_lib::sync;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "https://stream.ltbr.fm/live".to_string());
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(6);

    let controls = Arc::new(Controls::default());
    controls.set_volume(0.85);

    let mut out = match Output::new() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("no audio output: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("output device: {} ch @ {} Hz", out.channels, out.sample_rate);

    let stop = Arc::new(AtomicBool::new(false));
    let pipe = BytePipe::new();

    // Network ingest.
    {
        let url = url.clone();
        let stop = stop.clone();
        let pipe = pipe.clone();
        thread::spawn(move || {
            let _ = stream::run(&url, stop, pipe, |info| {
                eprintln!(
                    "station: {} ({} kbps, metaint {})",
                    info.name, info.bitrate_kbps, info.metaint
                );
            });
        });
    }

    // Continuously wobble the EQ + volume — the audible artifact test.
    {
        let controls = controls.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut t = 0.0f32;
            while !stop.load(Ordering::Relaxed) {
                let g = t.sin() * 12.0;
                controls.set_band(0, g); // bass shelf
                controls.set_band(8, -g); // 8 kHz
                controls.set_preamp((t * 0.7).sin() * 4.0);
                controls.set_volume(0.6 + 0.25 * (t * 1.7).sin().abs());
                t += 0.15;
                thread::sleep(Duration::from_millis(60));
            }
        });
    }

    // Decode + play.
    let mss = MediaSourceStream::new(Box::new(PipeReader::new(pipe.clone())), Default::default());
    let mut hint = Hint::new();
    hint.mime_type("audio/mpeg");
    hint.with_extension("mp3");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("probe failed");
    let mut format = probed.format;
    let track = format.default_track().expect("no track").clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .expect("no codec");
    let track_id = track.id;

    let mut st: Option<SessionState> = None;
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    let mut frames_total = 0u64;
    let mut non_finite = 0u64;
    let mut in_rate = 0u32;
    // Byte cursor for the demux point, so metadata surfaces in step with the
    // audio rather than seconds ahead of it — the same mechanism the engine
    // uses, which makes this probe a live demonstration of it.
    let mut demuxed = 0u64;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        let packet = match format.next_packet() {
            Ok(p) => {
                demuxed += p.data.len() as u64;
                let tick = pipe.tick(demuxed);
                for block in tick.released {
                    // Blocks are released when their audio reaches the
                    // speakers, so this lag is what the listener actually
                    // experiences — not merely the network's.
                    let lag = block
                        .url
                        .as_ref()
                        .map(|u| sync::wall_ms() as i64 - u.wall_ms);
                    eprintln!(
                        "[{:>6}] NOW PLAYING: {}  (lag {}, buffered {} ms)",
                        tick.depth_bytes,
                        block.title,
                        lag.map_or("n/a".into(), |l| format!("{l} ms")),
                        sync::bytes_to_ms(tick.depth_bytes, 128),
                    );
                }
                p
            }
            Err(e) => {
                eprintln!("stream read ended: {e}");
                break;
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => {
                eprintln!("decode error: {e}");
                break;
            }
        };
        let spec = *decoded.spec();
        in_rate = spec.rate;
        let cap = decoded.capacity() as u64;
        let b = sbuf.get_or_insert_with(|| SampleBuffer::<f32>::new(cap, spec));
        b.copy_interleaved_ref(decoded);
        let inter = b.samples();
        let in_ch = spec.channels.count().max(1);

        let s = st.get_or_insert_with(|| {
            SessionState::new(
                spec.rate,
                out.sample_rate,
                out.channels,
                controls.clone(),
                Box::new(|_bars| {}),
            )
        });

        for &x in inter {
            if !x.is_finite() {
                non_finite += 1;
            }
        }
        frames_total += (inter.len() / in_ch) as u64;
        s.process(inter, in_ch, &mut out, &stop, &stop);
    }

    stop.store(true, Ordering::SeqCst);
    pipe.close();

    let played = if in_rate > 0 {
        frames_total as f64 / in_rate as f64
    } else {
        0.0
    };
    eprintln!("---");
    eprintln!("stream sample rate: {in_rate} Hz");
    eprintln!("frames decoded: {frames_total} (~{played:.1}s)");
    eprintln!("non-finite input samples: {non_finite}");
    thread::sleep(Duration::from_millis(200));
}
