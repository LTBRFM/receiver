//! Digital signal processing: a 10-band graphic equaliser + preamp + master
//! volume, all with parameter smoothing so nothing clicks or zippers when a
//! slider moves.
//!
//! Signal path per frame (matching the reference design):
//!   input -> [10 biquad bands] -> preamp -> (spectrum tap) -> volume -> output
//!
//! * EQ gains are smoothed at block rate (32 samples) and biquad coefficients
//!   are recomputed only when the smoothed dB actually moves — gains glide.
//! * Preamp and master volume are smoothed **per sample** (one-pole), so even a
//!   hard mute is a short ramp to zero rather than a discontinuity.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// ISO-ish octave centre frequencies for the ten bands.
pub const FREQS: [f32; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
pub const NUM_BANDS: usize = 10;
pub const MAX_DB: f32 = 12.0;

/// EQ gains are re-evaluated every this many samples.
const BLOCK: u32 = 32;
/// Smoothing time constants.
const EQ_TAU_S: f32 = 0.020;
const GAIN_TAU_S: f32 = 0.020;

// ---------------------------------------------------------------------------
// Shared control state — written by Tauri commands, read by the audio thread.
// f32 values are stored as bit patterns in atomics so no lock is ever taken on
// the real-time path.
// ---------------------------------------------------------------------------

pub struct Controls {
    band_db: [AtomicU32; NUM_BANDS],
    preamp_db: AtomicU32,
    volume: AtomicU32,
    muted: AtomicBool,
    /// EQ bypass: when false every band glides to 0 dB while the slider
    /// positions are kept, so switching back restores the curve.
    eq_enabled: AtomicBool,
}

impl Default for Controls {
    fn default() -> Self {
        Controls {
            band_db: Default::default(),
            preamp_db: AtomicU32::new(0f32.to_bits()),
            volume: AtomicU32::new(0.8f32.to_bits()),
            muted: AtomicBool::new(false),
            eq_enabled: AtomicBool::new(true),
        }
    }
}

impl Controls {
    pub fn set_band(&self, i: usize, db: f32) {
        if i < NUM_BANDS {
            self.band_db[i].store(db.clamp(-MAX_DB, MAX_DB).to_bits(), Ordering::Relaxed);
        }
    }
    pub fn set_preamp(&self, db: f32) {
        self.preamp_db
            .store(db.clamp(-MAX_DB, MAX_DB).to_bits(), Ordering::Relaxed);
    }
    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }
    pub fn set_eq_enabled(&self, on: bool) {
        self.eq_enabled.store(on, Ordering::Relaxed);
    }

    fn band(&self, i: usize) -> f32 {
        if !self.eq_enabled.load(Ordering::Relaxed) {
            return 0.0;
        }
        f32::from_bits(self.band_db[i].load(Ordering::Relaxed))
    }
    fn preamp(&self) -> f32 {
        f32::from_bits(self.preamp_db.load(Ordering::Relaxed))
    }
    fn volume_target(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        }
    }
}

// ---------------------------------------------------------------------------
// One-pole smoother
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct OnePole {
    cur: f32,
    coeff: f32,
}

impl OnePole {
    fn new(initial: f32, tau_s: f32, rate: f32) -> Self {
        OnePole {
            cur: initial,
            coeff: (-1.0 / (tau_s * rate)).exp(),
        }
    }
    #[inline]
    fn process(&mut self, target: f32) -> f32 {
        self.cur = target + (self.cur - target) * self.coeff;
        self.cur
    }
}

// ---------------------------------------------------------------------------
// Biquad (RBJ cookbook), transposed direct-form II
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    LowShelf,
    Peaking,
    HighShelf,
}

#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn identity() -> Self {
        Biquad {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Recompute coefficients; audio state (z1/z2) is preserved so live tweaks
    /// don't glitch.
    fn set(&mut self, kind: Kind, freq: f32, q: f32, db: f32, fs: f32) {
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * (freq / fs).min(0.4999);
        let cw = w0.cos();
        let sw = w0.sin();
        let alpha = sw / (2.0 * q);

        let (b0, b1, b2, a0, a1, a2) = match kind {
            Kind::Peaking => (
                1.0 + alpha * a,
                -2.0 * cw,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cw,
                1.0 - alpha / a,
            ),
            Kind::LowShelf => {
                let sq = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cw + sq),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                    a * ((a + 1.0) - (a - 1.0) * cw - sq),
                    (a + 1.0) + (a - 1.0) * cw + sq,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                    (a + 1.0) + (a - 1.0) * cw - sq,
                )
            }
            Kind::HighShelf => {
                let sq = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cw + sq),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                    a * ((a + 1.0) + (a - 1.0) * cw - sq),
                    (a + 1.0) - (a - 1.0) * cw + sq,
                    2.0 * ((a - 1.0) - (a + 1.0) * cw),
                    (a + 1.0) - (a - 1.0) * cw - sq,
                )
            }
        };

        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

// ---------------------------------------------------------------------------
// The full stereo DSP block
// ---------------------------------------------------------------------------

pub struct Dsp {
    controls: Arc<Controls>,
    fs: f32,
    kinds: [Kind; NUM_BANDS],
    qs: [f32; NUM_BANDS],
    bands_l: [Biquad; NUM_BANDS],
    bands_r: [Biquad; NUM_BANDS],
    db_smooth: [OnePole; NUM_BANDS],
    last_db: [f32; NUM_BANDS],
    preamp_lin: OnePole,
    volume_lin: OnePole,
    counter: u32,
}

impl Dsp {
    pub fn new(fs: f32, controls: Arc<Controls>) -> Self {
        let mut kinds = [Kind::Peaking; NUM_BANDS];
        kinds[0] = Kind::LowShelf;
        kinds[NUM_BANDS - 1] = Kind::HighShelf;

        let mut qs = [1.1f32; NUM_BANDS];
        qs[0] = 0.7;
        qs[NUM_BANDS - 1] = 0.7;

        let block_rate = fs / BLOCK as f32;
        Dsp {
            fs,
            kinds,
            qs,
            bands_l: [Biquad::identity(); NUM_BANDS],
            bands_r: [Biquad::identity(); NUM_BANDS],
            db_smooth: [OnePole::new(0.0, EQ_TAU_S, block_rate); NUM_BANDS],
            last_db: [f32::NAN; NUM_BANDS],
            preamp_lin: OnePole::new(1.0, GAIN_TAU_S, fs),
            volume_lin: OnePole::new(controls.volume_target(), GAIN_TAU_S, fs),
            counter: 0,
            controls,
        }
    }

    fn update_coeffs(&mut self) {
        for i in 0..NUM_BANDS {
            let target = self.controls.band(i);
            let db = self.db_smooth[i].process(target);
            // `last_db` starts as NaN, so the first update must fire regardless
            // (NaN comparisons are always false — hence the explicit guard).
            if !self.last_db[i].is_finite() || (db - self.last_db[i]).abs() > 0.0005 {
                self.bands_l[i].set(self.kinds[i], FREQS[i], self.qs[i], db, self.fs);
                self.bands_r[i].set(self.kinds[i], FREQS[i], self.qs[i], db, self.fs);
                self.last_db[i] = db;
            }
        }
    }

    /// Process one stereo frame. Returns `(left, right, mono_tap)` where the tap
    /// is the post-preamp / pre-volume mono signal used to feed the spectrum.
    #[inline]
    pub fn process_frame(&mut self, l: f32, r: f32) -> (f32, f32, f32) {
        if self.counter == 0 {
            self.update_coeffs();
        }
        self.counter = (self.counter + 1) % BLOCK;

        let mut xl = l;
        let mut xr = r;
        for i in 0..NUM_BANDS {
            xl = self.bands_l[i].process(xl);
            xr = self.bands_r[i].process(xr);
        }

        let pg = self.preamp_lin.process(10f32.powf(self.controls.preamp() / 20.0));
        xl *= pg;
        xr *= pg;

        let tap = 0.5 * (xl + xr);

        let vg = self.volume_lin.process(self.controls.volume_target());
        (xl * vg, xr * vg, tap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 44_100.0;

    fn sine(freq: f32, n: usize) -> impl Iterator<Item = f32> {
        (0..n).map(move |i| (2.0 * std::f32::consts::PI * freq * i as f32 / FS).sin() * 0.5)
    }

    #[test]
    fn flat_eq_is_transparent() {
        let c = Arc::new(Controls::default());
        c.set_volume(1.0);
        let mut dsp = Dsp::new(FS, c);
        let mut max_err = 0.0f32;
        for x in sine(1000.0, 4000) {
            let (l, _r, _t) = dsp.process_frame(x, x);
            max_err = max_err.max((l - x).abs());
        }
        // All bands at 0 dB are identity biquads.
        assert!(max_err < 1e-4, "flat EQ altered signal by {max_err}");
    }

    #[test]
    fn mute_ramps_without_clicks() {
        let c = Arc::new(Controls::default());
        c.set_volume(1.0);
        c.set_muted(false);
        let mut dsp = Dsp::new(FS, c.clone());

        // Settle on DC = 1.0.
        let mut last = 0.0;
        for _ in 0..2000 {
            let (l, _, _) = dsp.process_frame(1.0, 1.0);
            last = l;
        }
        assert!((last - 1.0).abs() < 1e-3, "did not settle to unity: {last}");

        // Now mute and measure the largest sample-to-sample step.
        c.set_muted(true);
        let mut max_step = 0.0f32;
        let mut prev = last;
        for _ in 0..16000 {
            let (l, _, _) = dsp.process_frame(1.0, 1.0);
            max_step = max_step.max((l - prev).abs());
            prev = l;
        }
        // A hard cut would step by 1.0; the smoother must keep it tiny.
        assert!(max_step < 0.01, "mute stepped by {max_step} (zipper!)");
        assert!(prev.abs() < 1e-3, "did not reach silence: {prev}");
    }

    #[test]
    fn band_boost_adds_energy() {
        fn rms_at(freq: f32, band: Option<usize>) -> f32 {
            let c = Arc::new(Controls::default());
            c.set_volume(1.0);
            if let Some(b) = band {
                c.set_band(b, 12.0);
            }
            let mut dsp = Dsp::new(FS, c);
            // settle
            for x in sine(freq, 8000) {
                dsp.process_frame(x, x);
            }
            let mut sum = 0.0f64;
            let mut n = 0u64;
            for x in sine(freq, 8000) {
                let (l, _, _) = dsp.process_frame(x, x);
                sum += (l * l) as f64;
                n += 1;
            }
            (sum / n as f64).sqrt() as f32
        }
        // Band 7 is centred on 4 kHz.
        let flat = rms_at(4000.0, None);
        let boosted = rms_at(4000.0, Some(7));
        assert!(
            boosted > flat * 1.5,
            "boost had little effect: flat={flat}, boosted={boosted}"
        );
    }
}
