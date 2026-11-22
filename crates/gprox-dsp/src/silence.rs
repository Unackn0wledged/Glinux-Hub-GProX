//! Mic activity meter on the raw headset mic, for the mute status.
//!
//! The dongle sends exact digital zero while the headset mic is muted,
//! but also while it is live and quiet: the headset gates
//! silence to exact zero. So silence proves nothing, while
//! a non-zero sample proves the mic is **not** muted. The meter records when
//! the last non-zero sample was captured and when the stream started (the
//! dongle sends a stray report then, which must not count as a mute press).
//!
//! [`SilenceMeter::observe`] runs on the real-time thread: a scan of the block
//! and two relaxed atomic stores; no allocation, locks, syscalls or clock
//! reads. [`SilenceProbe::poll`] turns frame counts into times on the control
//! side.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct Shared {
    /// Frames seen in total.
    frames: AtomicU64,
    /// `frames` count just after the last non-zero sample; 0 = none yet.
    last_audio: AtomicU64,
}

/// Real-time side.
#[derive(Debug)]
pub struct SilenceMeter {
    shared: Arc<Shared>,
    frames: u64,
}

/// Control side.
#[derive(Debug)]
pub struct SilenceProbe {
    shared: Arc<Shared>,
    rate: u32,
    last_frames: u64,
    last_advance: Option<Instant>,
    stream_started: Option<Instant>,
    /// The `last_audio` index already turned into a time, and that time.
    audio_seen: (u64, Option<Instant>),
}

/// What the probe has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MicActivity {
    /// When the last non-zero sample was captured (estimate).
    pub last_audio: Option<Instant>,
    /// When the stream last started after being idle (estimate).
    pub stream_started: Option<Instant>,
}

/// A connected meter/probe pair for a stream at `rate` Hz.
pub fn silence_meter(rate: u32) -> (SilenceMeter, SilenceProbe) {
    let shared = Arc::new(Shared::default());
    (
        SilenceMeter {
            shared: shared.clone(),
            frames: 0,
        },
        SilenceProbe {
            shared,
            rate: rate.max(1),
            last_frames: 0,
            last_advance: None,
            stream_started: None,
            audio_seen: (0, None),
        },
    )
}

impl SilenceMeter {
    /// Account one block of the raw mic signal.
    pub fn observe(&mut self, block: &[f32]) {
        if let Some(i) = block.iter().rposition(|&x| x != 0.0) {
            self.shared
                .last_audio
                .store(self.frames + i as u64 + 1, Ordering::Relaxed);
        }
        self.frames += block.len() as u64;
        self.shared.frames.store(self.frames, Ordering::Relaxed);
    }
}

/// No frames for this long means the stream stopped.
const STALE: Duration = Duration::from_secs(2);

impl SilenceProbe {
    pub fn poll(&mut self) -> MicActivity {
        self.poll_at(Instant::now())
    }

    fn secs(&self, frames: u64) -> Duration {
        Duration::from_secs_f64(frames as f64 / f64::from(self.rate))
    }

    fn poll_at(&mut self, now: Instant) -> MicActivity {
        let last_audio = self.shared.last_audio.load(Ordering::Relaxed);
        let frames = self.shared.frames.load(Ordering::Relaxed);
        if frames != self.last_frames {
            let idle = self
                .last_advance
                .is_none_or(|t| now.duration_since(t) > STALE);
            if idle {
                // Started somewhere since the last poll: back-date by the
                // frames that came in since (bounded by the last poll).
                let mut t = now
                    .checked_sub(self.secs(frames - self.last_frames))
                    .unwrap_or(now);
                if let Some(prev) = self.last_advance {
                    t = t.max(prev);
                }
                self.stream_started = Some(t);
            }
            self.last_frames = frames;
            self.last_advance = Some(now);
        }
        // New audio since the last poll: date it now, while the frame count
        // still runs continuously from it. Older audio keeps its time (a
        // stall in between would otherwise move it forward).
        if last_audio != self.audio_seen.0
            && let Some(seen) = self.last_advance
        {
            let age = self.secs(frames.saturating_sub(last_audio));
            self.audio_seen = (last_audio, Some(seen.checked_sub(age).unwrap_or(seen)));
        }
        MicActivity {
            last_audio: self.audio_seen.1,
            stream_started: self.stream_started,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_times_and_stream_starts() {
        let (mut m, mut p) = silence_meter(48_000);
        let t0 = Instant::now();
        assert_eq!(p.poll_at(t0), MicActivity::default());
        // 0.5 s of zeros: a stream start, no audio.
        let zeros = [0.0f32; 24_000];
        m.observe(&zeros);
        let a = p.poll_at(t0 + Duration::from_secs(1));
        assert_eq!(a.last_audio, None);
        assert_eq!(a.stream_started, Some(t0 + Duration::from_millis(500)));
        // A non-zero sample, then 0.25 s of zeros.
        let mut one = [0.0f32; 12_000];
        one[0] = 1e-4;
        m.observe(&one);
        let a = p.poll_at(t0 + Duration::from_millis(1250));
        let want = t0 + Duration::from_millis(1250) - Duration::from_secs_f64(11_999.0 / 48_000.0);
        let got = a.last_audio.unwrap();
        let d = if got > want { got - want } else { want - got };
        assert!(d < Duration::from_millis(1), "{d:?}");
        assert_eq!(
            a.stream_started,
            Some(t0 + Duration::from_millis(500)),
            "no new start while running"
        );
        // Stream stops; after STALE it restarts: a new start time.
        let t2 = t0 + Duration::from_millis(1250) + STALE + Duration::from_secs(1);
        m.observe(&zeros[..4_800]);
        let a = p.poll_at(t2);
        assert_eq!(a.stream_started, Some(t2 - Duration::from_millis(100)));
        assert_eq!(a.last_audio, Some(got), "old audio keeps its time");
    }
}
