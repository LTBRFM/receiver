//! Live-edge drift control.
//!
//! A listener should sit about [`TARGET_LAG_MS`] behind the live edge — which
//! is not an arbitrary choice: Icecast is configured with
//! `burst-on-connect` / `burst-size 131072`, so at connect it hands us ~8s of
//! already-played audio in one go. That burst *is* the intended lag, and it is
//! also the jitter buffer that keeps the sound card fed.
//!
//! The failure this module exists to catch: `PIPE_CAP` is exactly twice the
//! burst, so if the decode side ever stalls (CPU spike, suspend/resume, an
//! audio device change) the pipe fills to ~16s and, because everything
//! downstream runs at exactly 1x, **that backlog never drains again**. The
//! listener is then permanently a quarter-minute behind with no way back.
//!
//! Two signals, and the relationship between them is what makes this safe:
//!
//!   * `local_ms` — our own buffer depth (pipe backlog + decoder read-ahead +
//!     output ring), converted through the measured bitrate. Always available,
//!     even when the station sends no metadata at all, and immune to clock
//!     skew because no clock is involved.
//!   * `lag_ms` — `wall_now - block.t`, sampled when a metadata block's audio
//!     actually reaches the speakers. This is the true end-to-end lag, but it
//!     trusts the listener's clock.
//!
//! Backpressure runs socket <- write_all <- PipeReader <- decoder, so a real
//! backlog *anywhere* downstream of Icecast's send queue shows up as a deep
//! local pipe. `local_ms` being high is therefore made a **necessary
//! condition** for acting. That one rule makes the controller immune to a
//! skewed client clock: no amount of clock error can manufacture a deep pipe.

use crate::icy;

/// Where a healthy listener sits — one Icecast burst behind the live edge.
pub const TARGET_LAG_MS: u64 = 8_000;
/// Stage 1 arms above this.
pub const CATCHUP_AT_MS: u64 = 12_000;
/// The hard ceiling the user asked for. Sustained breaches escalate.
pub const CEILING_MS: u64 = 15_000;
/// How long a breach must persist before we act. Deliberately time-based
/// rather than block-based: metadata heartbeats are 30s apart, so "two blocks"
/// could mean a full minute of a listener sitting 20s late.
pub const SUSTAIN_MS: u64 = 3_000;
/// Keep trimming for this long once armed. A single drop is not enough — it
/// empties our pipe, the network thread unblocks and immediately refills it
/// from the socket and Icecast's queue. Trimming repeatedly lets the reader
/// flush that backlog at memcpy speed until there is genuinely nothing left.
pub const CATCHUP_WINDOW_MS: u64 = 2_000;
/// Quiet period after a catch-up, so the measurement can settle before we
/// judge it. Also covers the decoder read-ahead, which a trim cannot reach.
pub const COOLDOWN_MS: u64 = 20_000;
/// Catch-ups within one episode before we stop trimming and just reconnect.
pub const MAX_CATCHUPS_PER_EPISODE: u32 = 3;
/// Calm for this long and the episode is forgotten.
pub const EPISODE_CLEAR_MS: u64 = 120_000;
/// Grace after the first decoded packet. Everything before this is burst being
/// consumed, not drift.
///
/// It must outlast the burst itself, not merely the first packet. Icecast
/// hands over ~8s of already-played audio on connect, and any metadata block
/// inside it carries a timestamp from before we ever connected — a live
/// capture showed the first released block reporting a 38s lag while the
/// buffer sat at a perfectly healthy 8s. Every one of those has to be
/// discarded, so this is sized just past [`TARGET_LAG_MS`].
pub const SETTLE_MS: u64 = 10_000;
/// Ceiling on audio discarded in one episode (~90s at 128kbps). Past this we
/// reconnect instead: something is wrong that trimming is not fixing.
pub const MAX_DROP_BYTES: u64 = 1_500_000;
/// The controller re-evaluates at most this often, regardless of packet rate.
pub const POLL_MS: u64 = 250;

const DEFAULT_BITRATE_KBPS: u32 = 128;
/// A `seq` drop larger than this means the source restarted rather than a
/// block arriving out of order.
const SEQ_RESET_SLACK: u64 = 32;

/// Milliseconds since the Unix epoch. Falls back to 0 before 1970, which only
/// a badly wrong clock can produce and which the controller treats as skew.
pub fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn bytes_to_ms(bytes: u64, bitrate_kbps: u32) -> u64 {
    let kbps = bitrate_kbps.max(1) as u64;
    bytes.saturating_mul(8) / kbps
}

pub fn ms_to_bytes(ms: u64, bitrate_kbps: u32) -> u64 {
    let kbps = bitrate_kbps.max(1) as u64;
    ms.saturating_mul(kbps) / 8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// Discard the oldest buffered audio until at most `keep_bytes` remain.
    /// Returned on every poll for [`CATCHUP_WINDOW_MS`], not just once.
    CatchUp { keep_bytes: usize },
    /// Give up on trimming: drop the attempt and reconnect, which lands us
    /// back on the burst by construction. Deliberate, so it must not surface
    /// as a stream fault.
    Reconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockVerdict {
    /// Usable: in sequence and after the settle grace.
    Fresh,
    /// Out of order or a duplicate — ignored entirely.
    StaleSeq,
    /// Arrived before the connection settled. Its audio is older than the
    /// block describes (burst-on-connect), so it would under-report the lag.
    Provisional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Settling,
    Steady,
    CatchingUp,
    Cooldown,
    Reconnecting,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Settling => "settling",
            State::Steady => "steady",
            State::CatchingUp => "catching-up",
            State::Cooldown => "cooldown",
            State::Reconnecting => "reconnecting",
        }
    }
}

/// A snapshot for the UI / diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub state: State,
    pub lag_ms: Option<i64>,
    pub excess_ms: Option<i64>,
    pub buffer_ms: u64,
    pub buffer_bytes: u64,
    pub bitrate_kbps: u32,
    pub dropped_ms: u64,
    pub catchups: u32,
    pub reconnects: u32,
}

pub struct Drift {
    state: State,
    bitrate_kbps: u32,

    first_packet_at: Option<u64>,
    last_poll: u64,

    buffer_bytes: u64,
    local_ms: u64,

    /// When the current uninterrupted breach began.
    over_since: Option<u64>,
    catchup_until: u64,
    cooldown_until: u64,
    /// Since when the buffer has looked healthy, for clearing the episode.
    calm_since: Option<u64>,

    last_seq: Option<u64>,
    lag_ms: Option<i64>,
    /// Smallest lag seen since this connection settled. Constant clock skew
    /// lands in here and is subtracted straight back out as `excess_ms`.
    baseline_lag: Option<i64>,
    consecutive_excess: u32,
    meta_trigger: bool,

    dropped_bytes_episode: u64,
    dropped_bytes_total: u64,
    catchups: u32,
    reconnects: u32,
}

impl Drift {
    pub fn new(bitrate_kbps: u32) -> Self {
        Drift {
            state: State::Settling,
            bitrate_kbps: sane_bitrate(bitrate_kbps),
            first_packet_at: None,
            last_poll: 0,
            buffer_bytes: 0,
            local_ms: 0,
            over_since: None,
            catchup_until: 0,
            cooldown_until: 0,
            calm_since: None,
            last_seq: None,
            lag_ms: None,
            baseline_lag: None,
            consecutive_excess: 0,
            meta_trigger: false,
            dropped_bytes_episode: 0,
            dropped_bytes_total: 0,
            catchups: 0,
            reconnects: 0,
        }
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        self.bitrate_kbps = sane_bitrate(kbps);
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// The first packet has been decoded; start the settle clock.
    pub fn first_packet(&mut self, now_ms: u64) {
        if self.first_packet_at.is_none() {
            self.first_packet_at = Some(now_ms);
        }
    }

    /// True once the connection has settled — before that, lag readings are
    /// meaningless because we are still chewing through the burst.
    pub fn settled(&self) -> bool {
        !matches!(self.state, State::Settling)
    }

    pub fn on_trimmed(&mut self, bytes: u64) {
        self.dropped_bytes_episode = self.dropped_bytes_episode.saturating_add(bytes);
        self.dropped_bytes_total = self.dropped_bytes_total.saturating_add(bytes);
    }

    /// Fold in a metadata block at the moment its audio reaches the speakers.
    pub fn on_block(&mut self, url: &icy::StreamUrl, now_ms: u64) -> BlockVerdict {
        if let Some(last) = self.last_seq {
            if url.seq + SEQ_RESET_SLACK < last {
                // A large decrease means the station's player restarted and
                // began a fresh session, not a reordered block. Re-anchor:
                // the old baseline described a timeline that no longer exists.
                self.last_seq = None;
                self.baseline_lag = None;
                self.consecutive_excess = 0;
            } else if url.seq <= last {
                return BlockVerdict::StaleSeq;
            }
        }
        self.last_seq = Some(url.seq);

        if !self.settled() {
            return BlockVerdict::Provisional;
        }

        let lag = now_ms as i64 - url.wall_ms;
        self.lag_ms = Some(lag);
        self.baseline_lag = Some(self.baseline_lag.map_or(lag, |b| b.min(lag)));

        // Secondary trigger. `local_ms` can sit under the local threshold while
        // the true lag has blown out — Icecast queueing for a slow client puts
        // the backlog upstream of our pipe. Only ever arms alongside a pipe
        // that is at least a full target deep, which is what keeps a bad clock
        // from reaching it.
        if self.excess_ms().unwrap_or(0) > (CEILING_MS - TARGET_LAG_MS) as i64 {
            self.consecutive_excess += 1;
            if self.consecutive_excess >= 2 && self.local_ms >= TARGET_LAG_MS {
                self.meta_trigger = true;
            }
        } else {
            self.consecutive_excess = 0;
        }

        BlockVerdict::Fresh
    }

    fn excess_ms(&self) -> Option<i64> {
        Some(self.lag_ms? - self.baseline_lag?)
    }

    /// Drive the controller. Call freely; it self-gates to [`POLL_MS`].
    pub fn poll(&mut self, buffer_bytes: u64, ring_ms: u64, now_ms: u64) -> Action {
        if self.last_poll != 0 && now_ms < self.last_poll.saturating_add(POLL_MS) {
            return Action::None;
        }
        self.last_poll = now_ms;
        self.buffer_bytes = buffer_bytes;
        self.local_ms = bytes_to_ms(buffer_bytes, self.bitrate_kbps) + ring_ms;

        // Forget an old episode once things have been calm for long enough.
        if self.local_ms <= CATCHUP_AT_MS {
            let since = *self.calm_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= EPISODE_CLEAR_MS {
                self.catchups = 0;
                self.dropped_bytes_episode = 0;
            }
        } else {
            self.calm_since = None;
        }

        match self.state {
            State::Settling => {
                if let Some(t0) = self.first_packet_at {
                    if now_ms.saturating_sub(t0) >= SETTLE_MS {
                        self.state = State::Steady;
                        self.over_since = None;
                    }
                }
                Action::None
            }

            State::Steady => {
                if self.local_ms > CATCHUP_AT_MS {
                    let since = *self.over_since.get_or_insert(now_ms);
                    if now_ms.saturating_sub(since) >= SUSTAIN_MS {
                        return self.begin_catchup(now_ms);
                    }
                } else {
                    self.over_since = None;
                }

                // The metadata path bypasses SUSTAIN_MS: by the time two
                // heartbeats have agreed, a minute has already passed.
                if self.meta_trigger && self.local_ms >= TARGET_LAG_MS {
                    return self.begin_catchup(now_ms);
                }
                Action::None
            }

            State::CatchingUp => {
                if now_ms >= self.catchup_until || self.dropped_bytes_episode >= MAX_DROP_BYTES {
                    self.state = State::Cooldown;
                    self.cooldown_until = now_ms.saturating_add(COOLDOWN_MS);
                    return Action::None;
                }
                Action::CatchUp {
                    keep_bytes: ms_to_bytes(TARGET_LAG_MS, self.bitrate_kbps) as usize,
                }
            }

            State::Cooldown => {
                if now_ms < self.cooldown_until {
                    return Action::None;
                }
                if self.local_ms > CEILING_MS || self.catchups >= MAX_CATCHUPS_PER_EPISODE {
                    // Trimming is not holding. A reconnect re-enters on the
                    // burst, which is the target by definition.
                    self.state = State::Reconnecting;
                    self.reconnects += 1;
                    return Action::Reconnect;
                }
                self.state = State::Steady;
                self.over_since = None;
                Action::None
            }

            State::Reconnecting => Action::None,
        }
    }

    fn begin_catchup(&mut self, now_ms: u64) -> Action {
        self.state = State::CatchingUp;
        self.catchup_until = now_ms.saturating_add(CATCHUP_WINDOW_MS);
        self.over_since = None;
        self.meta_trigger = false;
        self.consecutive_excess = 0;
        self.catchups += 1;
        Action::CatchUp {
            keep_bytes: ms_to_bytes(TARGET_LAG_MS, self.bitrate_kbps) as usize,
        }
    }

    pub fn report(&self) -> Report {
        Report {
            state: self.state,
            lag_ms: if self.settled() { self.lag_ms } else { None },
            excess_ms: if self.settled() { self.excess_ms() } else { None },
            buffer_ms: self.local_ms,
            buffer_bytes: self.buffer_bytes,
            bitrate_kbps: self.bitrate_kbps,
            dropped_ms: bytes_to_ms(self.dropped_bytes_total, self.bitrate_kbps),
            catchups: self.catchups,
            reconnects: self.reconnects,
        }
    }
}

fn sane_bitrate(kbps: u32) -> u32 {
    if (32..=512).contains(&kbps) {
        kbps
    } else {
        DEFAULT_BITRATE_KBPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KBPS: u32 = 128;

    fn url(seq: u64, wall_ms: i64) -> icy::StreamUrl {
        icy::StreamUrl {
            seq,
            playhead_ms: 0,
            wall_ms,
            kind: "track".into(),
            item_id: "t".into(),
            payload: None,
        }
    }

    /// Drive `poll` at the real cadence for `ms`, returning the first non-None
    /// action and the time it happened.
    fn run(d: &mut Drift, depth_ms: u64, from: u64, ms: u64) -> (Action, u64) {
        let bytes = ms_to_bytes(depth_ms, KBPS);
        let mut t = from;
        while t < from + ms {
            let a = d.poll(bytes, 0, t);
            if a != Action::None {
                return (a, t);
            }
            t += POLL_MS;
        }
        (Action::None, t)
    }

    fn settled(depth_ms: u64) -> (Drift, u64) {
        let mut d = Drift::new(KBPS);
        d.first_packet(0);
        let (a, t) = run(&mut d, depth_ms, 0, SETTLE_MS + POLL_MS);
        assert_eq!(a, Action::None, "settling must never act");
        assert_eq!(d.state(), State::Steady);
        (d, t)
    }

    #[test]
    fn settling_absorbs_the_burst() {
        // A full burst in the pipe is healthy, not drift — and even a pipe well
        // over the threshold must not trigger before the settle grace expires.
        let mut d = Drift::new(KBPS);
        d.first_packet(0);
        let (a, _) = run(&mut d, 14_000, 0, SETTLE_MS - POLL_MS);
        assert_eq!(a, Action::None);
        assert_eq!(d.state(), State::Settling);
        assert!(d.report().lag_ms.is_none(), "no lag reported while settling");
    }

    #[test]
    fn a_healthy_buffer_never_acts() {
        let (mut d, t) = settled(TARGET_LAG_MS);
        let (a, _) = run(&mut d, TARGET_LAG_MS, t, 300_000);
        assert_eq!(a, Action::None);
        assert_eq!(d.report().catchups, 0);
    }

    #[test]
    fn sustained_overrun_trims_back_to_target() {
        let (mut d, t) = settled(TARGET_LAG_MS);
        let (a, at) = run(&mut d, 13_000, t, 60_000);
        assert_eq!(
            a,
            Action::CatchUp { keep_bytes: ms_to_bytes(TARGET_LAG_MS, KBPS) as usize }
        );
        // Must have waited out SUSTAIN_MS rather than firing on first sight.
        assert!(at - t >= SUSTAIN_MS, "fired after {}ms, want >= {SUSTAIN_MS}", at - t);
        assert_eq!(d.report().catchups, 1);
    }

    #[test]
    fn a_brief_spike_is_ignored() {
        let (mut d, mut t) = settled(TARGET_LAG_MS);
        // Over the line, but for less than SUSTAIN_MS.
        let (a, nt) = run(&mut d, 13_000, t, SUSTAIN_MS - POLL_MS);
        assert_eq!(a, Action::None);
        t = nt;
        // Back to healthy: the breach timer must reset, not accumulate.
        let (a, nt) = run(&mut d, TARGET_LAG_MS, t, 10_000);
        assert_eq!(a, Action::None);
        let (a, _) = run(&mut d, 13_000, nt, SUSTAIN_MS - POLL_MS);
        assert_eq!(a, Action::None, "breach timer must have restarted");
    }

    #[test]
    fn catch_up_keeps_trimming_then_cools_down() {
        let (mut d, t) = settled(TARGET_LAG_MS);
        let (a, mut t) = run(&mut d, 13_000, t, 60_000);
        assert!(matches!(a, Action::CatchUp { .. }));

        // A single drop would just be refilled from the socket, so the window
        // must keep asking.
        let mut trims = 1;
        let bytes = ms_to_bytes(13_000, KBPS);
        while d.state() == State::CatchingUp {
            t += POLL_MS;
            if matches!(d.poll(bytes, 0, t), Action::CatchUp { .. }) {
                trims += 1;
            }
        }
        assert!(trims >= 4, "expected repeated trims across the window, got {trims}");
        assert_eq!(d.state(), State::Cooldown);
    }

    #[test]
    fn cooldown_suppresses_a_second_catch_up() {
        let (mut d, t) = settled(TARGET_LAG_MS);
        let (_, t) = run(&mut d, 13_000, t, 60_000);

        // Stay breached throughout; nothing more may fire until cooldown ends.
        let bytes = ms_to_bytes(13_000, KBPS);
        let mut tt = t;
        let deadline = t + CATCHUP_WINDOW_MS + COOLDOWN_MS - POLL_MS;
        let mut extra_catchups = 0;
        while tt < deadline {
            tt += POLL_MS;
            if d.state() != State::CatchingUp {
                if matches!(d.poll(bytes, 0, tt), Action::CatchUp { .. }) {
                    extra_catchups += 1;
                }
            } else {
                d.poll(bytes, 0, tt);
            }
        }
        assert_eq!(extra_catchups, 0, "cooldown must gate re-firing");
    }

    #[test]
    fn repeated_failure_escalates_to_reconnect() {
        // Lag pinned above the ceiling: trimming is not helping, so the
        // controller must stop trimming and reconnect.
        let (mut d, t) = settled(TARGET_LAG_MS);
        let bytes = ms_to_bytes(20_000, KBPS);
        let mut tt = t;
        let mut saw_reconnect = false;
        for _ in 0..4_000 {
            tt += POLL_MS;
            if d.poll(bytes, 0, tt) == Action::Reconnect {
                saw_reconnect = true;
                break;
            }
        }
        assert!(saw_reconnect, "never escalated; state {:?}", d.state());
        assert_eq!(d.report().reconnects, 1);
        assert_eq!(d.state(), State::Reconnecting);
    }

    #[test]
    fn metadata_lag_alone_never_acts() {
        // The clock says we are a minute late, but our buffers are healthy —
        // the extra latency is upstream or the clock is wrong. Trimming an
        // already-shallow pipe would only cause an underrun.
        let (mut d, mut t) = settled(TARGET_LAG_MS);
        let bytes = ms_to_bytes(4_000, KBPS);
        for i in 1..40u64 {
            t += 30_000;
            d.on_block(&url(i, t as i64 - 60_000), t);
            assert_eq!(d.poll(bytes, 0, t), Action::None);
        }
        assert_eq!(d.report().catchups, 0);
    }

    #[test]
    fn constant_clock_skew_is_absorbed() {
        // A listener whose clock is an hour fast reports a huge lag on every
        // block; the baseline subtracts it out, so `excess_ms` stays ~0.
        let (mut d, mut t) = settled(TARGET_LAG_MS);
        const SKEW: i64 = 3_600_000;
        for i in 1..10u64 {
            t += 30_000;
            d.on_block(&url(i, t as i64 - SKEW - 8_000), t);
        }
        let r = d.report();
        assert!(r.lag_ms.unwrap() > SKEW, "raw lag carries the skew");
        assert!(r.excess_ms.unwrap().abs() < 100, "excess must cancel it: {r:?}");
        assert_eq!(r.catchups, 0);
    }

    #[test]
    fn metadata_excess_with_a_deep_pipe_bypasses_the_sustain_wait() {
        let (mut d, mut t) = settled(TARGET_LAG_MS);
        let bytes = ms_to_bytes(TARGET_LAG_MS, KBPS);

        // Establish a baseline at ~8s of lag.
        t += 30_000;
        d.on_block(&url(1, t as i64 - 8_000), t);
        d.poll(bytes, 0, t);

        // Now the true lag blows out while the local pipe stays exactly at
        // target — two consecutive blocks agree, so it fires without waiting.
        for i in 2..4u64 {
            t += 30_000;
            d.on_block(&url(i, t as i64 - 30_000), t);
        }
        t += POLL_MS;
        assert!(matches!(d.poll(bytes, 0, t), Action::CatchUp { .. }));
    }

    #[test]
    fn stale_sequence_ignored_and_a_reset_re_anchors() {
        let (mut d, t) = settled(TARGET_LAG_MS);
        // A real session runs its sequence up into the hundreds (s=233 was on
        // air while this was written) before the source ever restarts.
        assert_eq!(d.on_block(&url(233, t as i64 - 8_000), t), BlockVerdict::Fresh);
        // Icecast re-injects the same block after a source bounce; the audio
        // is unchanged, so it must not be re-measured.
        assert_eq!(d.on_block(&url(233, t as i64 - 8_000), t), BlockVerdict::StaleSeq);
        assert_eq!(d.on_block(&url(232, t as i64 - 8_000), t), BlockVerdict::StaleSeq);
        // The station's player restarted: seq falls back to the start of a new
        // session, describing a timeline the old baseline knows nothing about.
        assert_eq!(d.on_block(&url(1, t as i64 - 9_000), t), BlockVerdict::Fresh);
        assert_eq!(d.report().excess_ms, Some(0), "baseline must re-anchor");
    }

    /// Regression: observed against the live stream, where the first released
    /// block reported a 38s lag because it was a heartbeat already sitting in
    /// Icecast's burst when we connected — while the buffer was a perfectly
    /// healthy 8s. Counting it would have poisoned the baseline.
    #[test]
    fn a_stale_block_from_the_burst_never_becomes_the_baseline() {
        let mut d = Drift::new(KBPS);
        d.first_packet(0);
        let bytes = ms_to_bytes(TARGET_LAG_MS, KBPS);

        let mut t = SETTLE_MS / 2;
        d.poll(bytes, 0, t);
        assert_eq!(
            d.on_block(&url(1, t as i64 - 38_000), t),
            BlockVerdict::Provisional,
            "a burst-era block must not be measured"
        );

        // Once settled, real blocks report the true ~8.5s.
        t += SETTLE_MS;
        d.poll(bytes, 0, t);
        assert!(d.settled());
        assert_eq!(d.on_block(&url(2, t as i64 - 8_500), t), BlockVerdict::Fresh);
        assert_eq!(d.report().lag_ms, Some(8_500));
        assert_eq!(d.report().excess_ms, Some(0), "baseline is the real lag");

        let (a, _) = run(&mut d, TARGET_LAG_MS, t, 60_000);
        assert_eq!(a, Action::None, "a healthy stream must never be trimmed");
    }

    #[test]
    fn blocks_before_settling_are_provisional() {
        let mut d = Drift::new(KBPS);
        d.first_packet(0);
        // Burst-on-connect means this block describes newer audio than we are
        // hearing, so its lag would read far too low.
        assert_eq!(d.on_block(&url(1, 0), 1_000), BlockVerdict::Provisional);
        assert!(d.report().lag_ms.is_none());
    }

    #[test]
    fn byte_and_millisecond_conversions_agree_with_the_burst() {
        // The Icecast burst is 131072 bytes; at 128kbps that is the 8s target.
        assert_eq!(ms_to_bytes(TARGET_LAG_MS, 128), 128_000);
        assert_eq!(bytes_to_ms(131_072, 128), 8_192);
        assert_eq!(bytes_to_ms(ms_to_bytes(12_345, 192), 192), 12_345);
        // A nonsense bitrate falls back rather than dividing by zero.
        assert_eq!(Drift::new(0).bitrate_kbps(), 128);
        assert_eq!(Drift::new(9_999).bitrate_kbps(), 128);
        assert_eq!(Drift::new(192).bitrate_kbps(), 192);
    }
}
