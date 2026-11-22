//! Mic mute state: best effort, from the dongle's consumer-control reports
//! plus a live-audio check.
//!
//! The headset has no HID++ mute feature and G HUB shows no mute state for it.
//! What can be observed:
//!
//! - Each press of the mute button makes the dongle send one or two
//!   `01 00 00 00 00` reports (two ~45 ms apart, sometimes one). The report
//!   carries no state, so a press is a **toggle**.
//! - A muted mic is exact digital zero on the USB stream, but so is a live
//!   mic that is quiet (the headset gates silence). So audio proves "live",
//!   silence proves nothing.
//!
//! The same report also follows other things, which are filtered out:
//! a write (sidetone), headset power-off/on and dongle re-plug (within
//! ~1 s), the volume wheel (`01 01`/`01 02` presses and their releases), and
//! the start of an audio stream after idle (seen when gproxd's mic stream
//! starts, and in C/12 for playback). Only stream starts gproxd sees
//! itself (its own mic stream) can be filtered; others can flip the state
//! wrongly until the next press or the next audio.
//!
//! The state starts unknown and becomes known with the first audio. Toggles
//! of an unknown state stay unknown. The state survives headset power cycles
//! (the headset keeps its mute; live test).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Reports this close together are one press.
pub const BURST_WINDOW: Duration = Duration::from_millis(120);
/// A press is applied this long after it, so a stream start that is noticed
/// late can still cancel it.
pub const APPLY_DELAY: Duration = Duration::from_millis(1000);
/// Reports this close to a stream start are not presses.
pub const STREAM_START_WINDOW: Duration = Duration::from_millis(1000);
/// Reports are ignored this long after a write.
pub const AFTER_WRITE: Duration = Duration::from_millis(500);
/// Reports are ignored this long after a power event or (re)connect.
pub const AFTER_POWER: Duration = Duration::from_secs(2);
/// Reports are ignored this long after a volume-wheel press.
pub const AFTER_WHEEL: Duration = Duration::from_millis(500);
/// Audio must come this long after the last applied press to count.
pub const AUDIO_MARGIN: Duration = Duration::from_millis(200);

const RELEASE: [u8; 5] = [0x01, 0, 0, 0, 0];

/// What the audio side has seen (gproxd's mic path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioObservation {
    /// When the last non-zero mic sample was captured.
    pub last_audio: Option<Instant>,
    /// When the mic stream last started after being idle.
    pub stream_started: Option<Instant>,
}

#[derive(Debug, Clone, Default)]
pub struct MuteTracker {
    state: Option<bool>,
    /// Last report of the current burst.
    burst_last: Option<Instant>,
    /// Presses not applied yet (their first report's time).
    pending: VecDeque<Instant>,
    ignore_until: Option<Instant>,
    wheel_until: Option<Instant>,
    last_change: Option<Instant>,
    stream_started: Option<Instant>,
    last_audio: Option<Instant>,
}

fn later(a: Option<Instant>, b: Instant) -> Option<Instant> {
    Some(a.map_or(b, |a| a.max(b)))
}

impl MuteTracker {
    pub fn muted(&self) -> Option<bool> {
        self.state
    }

    /// Ignore reports for `d` from `now` (writes, power events).
    pub fn quiet(&mut self, now: Instant, d: Duration) {
        self.ignore_until = later(self.ignore_until, now + d);
        self.burst_last = None;
    }

    /// When [`MuteTracker::tick`] must run next.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.front().map(|&t| t + APPLY_DELAY)
    }

    /// A non-HID++ input report that arrived at `at`.
    pub fn report(&mut self, at: Instant, data: &[u8]) {
        if data.len() < RELEASE.len() || data[0] != RELEASE[0] {
            return;
        }
        if data[1..] != RELEASE[1..] {
            self.wheel_until = later(self.wheel_until, at + AFTER_WHEEL);
            self.burst_last = None;
            return;
        }
        if self.wheel_until.is_some_and(|t| at < t) || self.ignore_until.is_some_and(|t| at < t) {
            return;
        }
        if let Some(l) = self.burst_last
            && at.duration_since(l) <= BURST_WINDOW
        {
            self.burst_last = Some(at);
            return;
        }
        self.burst_last = Some(at);
        self.pending.push_back(at);
    }

    /// News from the audio side.
    pub fn audio(&mut self, obs: AudioObservation) {
        if let Some(t) = obs.stream_started {
            self.stream_started = later(self.stream_started, t);
        }
        if let Some(t) = obs.last_audio {
            self.last_audio = later(self.last_audio, t);
        }
    }

    /// Apply due presses, then the audio evidence.
    pub fn tick(&mut self, now: Instant) {
        while let Some(&t) = self.pending.front() {
            if now < t + APPLY_DELAY {
                break;
            }
            self.pending.pop_front();
            let near_stream_start = self.stream_started.is_some_and(|s| {
                let d = if s > t { s - t } else { t - s };
                d <= STREAM_START_WINDOW
            });
            if near_stream_start {
                tracing::debug!("mute report next to a mic stream start: ignored");
                continue;
            }
            self.state = self.state.map(|m| !m);
            self.last_change = Some(t);
        }
        if self.pending.is_empty()
            && let Some(a) = self.last_audio
            && self.last_change.is_none_or(|c| a > c + AUDIO_MARGIN)
        {
            self.state = Some(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: [u8; 5] = [1, 1, 0, 0, 0];

    fn ms(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn audio_at(t: Instant) -> AudioObservation {
        AudioObservation {
            last_audio: Some(t),
            stream_started: None,
        }
    }

    /// Third live run: live, then ON (one report), OFF
    /// (one report), ON (two reports).
    #[test]
    fn live_run_3() {
        let t0 = Instant::now();
        let mut m = MuteTracker::default();
        m.tick(t0);
        assert_eq!(m.muted(), None);
        m.audio(audio_at(ms(t0, 500)));
        m.tick(ms(t0, 1_000));
        assert_eq!(m.muted(), Some(false));
        m.report(ms(t0, 41_000), &RELEASE);
        assert_eq!(m.deadline(), Some(ms(t0, 42_000)));
        m.tick(ms(t0, 41_500));
        assert_eq!(m.muted(), Some(false), "applied after APPLY_DELAY");
        m.tick(ms(t0, 42_000));
        assert_eq!(m.muted(), Some(true));
        m.report(ms(t0, 53_760), &RELEASE);
        m.tick(ms(t0, 55_000));
        assert_eq!(m.muted(), Some(false));
        m.report(ms(t0, 65_223), &RELEASE);
        m.report(ms(t0, 65_273), &RELEASE);
        m.tick(ms(t0, 67_000));
        assert_eq!(m.muted(), Some(true), "a two-report burst is one press");
        // Audio from before the press doesn't undo it ...
        m.audio(audio_at(ms(t0, 65_100)));
        m.tick(ms(t0, 68_000));
        assert_eq!(m.muted(), Some(true));
        // ... audio after it does.
        m.audio(audio_at(ms(t0, 70_000)));
        m.tick(ms(t0, 70_100));
        assert_eq!(m.muted(), Some(false));
    }

    #[test]
    fn unknown_until_audio() {
        let t0 = Instant::now();
        let mut m = MuteTracker::default();
        m.report(ms(t0, 0), &RELEASE);
        m.tick(ms(t0, 2_000));
        assert_eq!(m.muted(), None, "toggling an unknown state");
        m.audio(audio_at(ms(t0, 3_000)));
        m.tick(ms(t0, 3_100));
        assert_eq!(m.muted(), Some(false));
    }

    #[test]
    fn exclusions() {
        let t0 = Instant::now();
        let mut m = MuteTracker::default();
        m.audio(audio_at(t0));
        m.tick(t0);
        assert_eq!(m.muted(), Some(false));
        // Volume wheel incl. a lone release 200 ms after the last press
        // (C/10 t=1.314).
        m.report(ms(t0, 1_000), &UP);
        m.report(ms(t0, 1_050), &RELEASE);
        m.report(ms(t0, 1_250), &RELEASE);
        // Write echo.
        m.quiet(ms(t0, 3_000), AFTER_WRITE);
        m.report(ms(t0, 3_095), &RELEASE);
        // Power-on: reports up to ~1 s later.
        m.quiet(ms(t0, 5_000), AFTER_POWER);
        for t in [5_150, 5_750, 5_800, 6_000] {
            m.report(ms(t0, t), &RELEASE);
        }
        // gproxd's mic stream starts 50 ms after a report (noticed late).
        m.report(ms(t0, 10_000), &RELEASE);
        m.audio(AudioObservation {
            last_audio: None,
            stream_started: Some(ms(t0, 10_050)),
        });
        m.tick(ms(t0, 12_000));
        assert_eq!(m.muted(), Some(false));
        // A real press afterwards counts.
        m.report(ms(t0, 20_000), &RELEASE);
        m.tick(ms(t0, 21_000));
        assert_eq!(m.muted(), Some(true));
    }
}
