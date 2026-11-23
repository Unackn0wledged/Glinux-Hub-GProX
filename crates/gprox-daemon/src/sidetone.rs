//! Always-on sidetone: the headset only mixes sidetone while
//! the host streams its USB capture interface. While the setting is on,
//! gproxd keeps a [`KeepAlive`] stream on the headset mic, so the stream runs
//! even when no app records from "PRO X Mic (gprox)".
//!
//! Everything here runs on the PipeWire main-loop thread. The controller only
//! flips [`KeepOpen::wanted`]; a 1 s timer calls [`SidetoneStream::tick`],
//! which creates or drops the stream to match. A stream that fails is dropped
//! and created again (after [`RETRY`]); it never stops gproxd, since the
//! audio paths don't depend on it.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gprox_dsp::silence::MeterSource;
use gprox_pw::{KeepAlive, KeepAliveSpec, fatal_transition};
use pipewire::core::CoreRc;
use pipewire::stream::StreamState;

use crate::control::KeepOpen;

/// `node.name` of the keep-alive stream.
pub const NODE_NAME: &str = "gprox_sidetone";
/// What mixers (pavucontrol, `pw-top`) show for it.
pub const DESCRIPTION: &str = "PRO X sidetone (gprox keeps the mic open)";
/// Quantum it asks for: large, so it never forces small blocks on its own; an
/// app recording through gprox still gets its smaller one.
pub const QUANTUM: u32 = 1024;
/// Wait this long before creating the stream again after it failed.
pub const RETRY: Duration = Duration::from_secs(5);

/// What [`SidetoneStream::tick`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Create,
    Drop,
    Nothing,
}

/// The decision of one tick: create the stream when it is wanted and missing
/// (unless a failure is still in its retry wait), drop it when it is not
/// wanted any more.
pub fn action(wanted: bool, have: bool, retry_at: Option<Instant>, now: Instant) -> Action {
    match (wanted, have) {
        (true, false) if retry_at.is_none_or(|t| now >= t) => Action::Create,
        (false, true) => Action::Drop,
        _ => Action::Nothing,
    }
}

/// Owns the keep-alive stream while the setting is on.
pub struct SidetoneStream {
    core: CoreRc,
    spec: KeepAliveSpec,
    flags: Arc<KeepOpen>,
    meters: MeterSource,
    stream: Option<KeepAlive>,
    /// Set by the stream's state handler when it errored or was disconnected.
    failed: Rc<Cell<bool>>,
    retry_at: Option<Instant>,
}

impl SidetoneStream {
    /// `meters` feed the mute tracking: the dongle sends a stray mute-button
    /// report when the stream starts, which must not count
    /// as a press.
    pub fn new(
        core: CoreRc,
        target: &str,
        rate: u32,
        flags: Arc<KeepOpen>,
        meters: MeterSource,
    ) -> Self {
        SidetoneStream {
            core,
            spec: KeepAliveSpec {
                name: NODE_NAME.into(),
                description: DESCRIPTION.into(),
                target: target.into(),
                rate,
                latency_frames: QUANTUM,
            },
            flags,
            meters,
            stream: None,
            failed: Rc::default(),
            retry_at: None,
        }
    }

    /// Bring the stream in line with the setting.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if self.failed.replace(false) && self.stream.take().is_some() {
            self.flags.streaming.store(false, Ordering::Relaxed);
            self.retry_at = Some(now + RETRY);
        }
        let wanted = self.flags.wanted.load(Ordering::Relaxed);
        match action(wanted, self.stream.is_some(), self.retry_at, now) {
            Action::Create => match self.create() {
                Ok(s) => {
                    tracing::info!(target = %self.spec.target,
                        "always-on sidetone: keeping the headset mic stream open");
                    self.stream = Some(s);
                    self.retry_at = None;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "always-on sidetone: cannot create the stream");
                    self.retry_at = Some(now + RETRY);
                }
            },
            Action::Drop => {
                self.stream = None;
                self.flags.streaming.store(false, Ordering::Relaxed);
                tracing::info!("always-on sidetone: off, the headset mic stream is released");
            }
            Action::Nothing => {}
        }
    }

    fn create(&self) -> Result<KeepAlive, gprox_pw::Error> {
        let mut meter = self.meters.meter();
        let flags = self.flags.clone();
        let failed = self.failed.clone();
        KeepAlive::new(
            &self.core,
            &self.spec,
            move |block| meter.observe(block),
            move |old: &StreamState, new: &StreamState| {
                let streaming = matches!(new, StreamState::Streaming);
                if streaming != matches!(old, StreamState::Streaming) {
                    // Paused ↔ Streaming: the headset mic came or went.
                    tracing::info!(streaming, "always-on sidetone stream");
                }
                flags.streaming.store(streaming, Ordering::Relaxed);
                if let Some(why) = fatal_transition(old, new) {
                    tracing::warn!("always-on sidetone stream: {why}; creating it again");
                    failed.set(true);
                }
            },
        )
    }

    /// Process callbacks of the current stream (0 without one).
    pub fn cycles(&self) -> u64 {
        self.stream.as_ref().map_or(0, KeepAlive::cycles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_actions() {
        let now = Instant::now();
        let later = Some(now + Duration::from_secs(1));
        let earlier = Some(now - Duration::from_secs(1));
        assert_eq!(action(true, false, None, now), Action::Create);
        assert_eq!(action(true, false, earlier, now), Action::Create);
        assert_eq!(action(true, false, Some(now), now), Action::Create);
        // A failed stream waits before it is created again.
        assert_eq!(action(true, false, later, now), Action::Nothing);
        assert_eq!(action(true, true, None, now), Action::Nothing);
        assert_eq!(action(false, true, None, now), Action::Drop);
        assert_eq!(action(false, true, later, now), Action::Drop);
        assert_eq!(action(false, false, None, now), Action::Nothing);
    }
}
