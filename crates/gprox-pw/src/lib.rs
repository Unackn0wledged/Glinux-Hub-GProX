//! PipeWire integration for gprox.
//!
//! A [`Duplex`] is one processing path built from two `pw_stream`s that share
//! a node group and link group:
//!
//! ```text
//!   upstream ──► [capture stream] ─┐            ┌─► [playback stream] ──► downstream
//!                                  └─ Processor ┘
//! ```
//!
//! * **Render** (`Direction::Render`): the capture stream *is* the virtual sink
//!   (`media.class = Audio/Sink`) that apps play into; the playback stream is
//!   an ordinary output stream targeted at the headset sink.
//! * **Capture** (`Direction::Capture`): the capture stream records from the
//!   headset mic; the playback stream *is* the virtual source
//!   (`media.class = Audio/Source`) that apps record from.
//!
//! Scheduling follows PipeWire's own module-loopback (1.6): the playback
//! stream is connected first with `PW_STREAM_FLAG_TRIGGER`, the capture stream
//! with `PW_STREAM_FLAG_ASYNC`, and the capture stream's process callback only
//! calls `pw_stream_trigger_process()` on the playback stream. The playback stream's process callback
//! dequeues the capture buffer and its own buffer and runs the processor in
//! the same graph cycle, so the path adds no buffering of its own.
//!
//! The `pipewire` crate has no `pw_filter` binding; this
//! design needs only the safe stream API, so this crate contains no `unsafe`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gprox_dsp::{MAX_CHANNELS, Processor};
use pipewire as pw;
use pw::core::CoreRc;
use pw::properties::PropertiesBox;
use pw::spa;
use pw::stream::{StreamFlags, StreamRc};
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::pod::Pod;

/// Largest block we size scratch buffers for (PipeWire's default
/// `default.clock.quantum-limit`).
pub const MAX_FRAMES: usize = 8192;

/// Channel position constants (re-exported so callers don't need spa-sys).
pub mod position {
    use pipewire::spa::sys;
    pub const MONO: u32 = sys::SPA_AUDIO_CHANNEL_MONO;
    pub const FL: u32 = sys::SPA_AUDIO_CHANNEL_FL;
    pub const FR: u32 = sys::SPA_AUDIO_CHANNEL_FR;
}

/// Errors from building a [`Duplex`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pipewire: {0}")]
    PipeWire(#[from] pw::Error),
    #[error("invalid spec: {0}")]
    Spec(&'static str),
    #[error("could not serialise format pod")]
    Pod,
}

/// Which way a path faces the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// apps → virtual sink → processor → headset sink
    Render,
    /// headset source → processor → virtual source → apps
    Capture,
}

/// Static description of a processing path.
#[derive(Debug, Clone)]
pub struct DuplexSpec {
    pub direction: Direction,
    /// `node.name` of the virtual device; the other stream gets a suffix.
    pub name: String,
    /// `node.description` of the virtual device (what desktop mixers show).
    pub description: String,
    /// `node.name` of the real device to attach to. `None` lets WirePlumber choose.
    pub target: Option<String>,
    /// Channel positions; the length is the channel count.
    pub positions: Vec<u32>,
    /// Processing rate. The stream adapters convert if the graph runs at another rate.
    pub rate: u32,
    /// Requested quantum in frames (`node.latency = <frames>/<rate>`).
    pub latency_frames: u32,
    /// `priority.session` of the virtual device. WirePlumber picks the
    /// available node with the highest one as the default device unless the
    /// user's own choice says otherwise. `None` leaves it unset.
    pub priority: Option<u32>,
}

/// Counters written on the data thread with relaxed atomics, read anywhere.
#[derive(Debug, Default)]
pub struct Stats {
    /// Playback process callbacks that produced output.
    pub cycles: AtomicU64,
    /// Cycles with no capture buffer available (silence was sent instead).
    pub no_input: AtomicU64,
    /// Process callbacks where the playback stream had no free buffer.
    pub no_output: AtomicU64,
    /// Capture buffers discarded because more than one was queued.
    pub dropped_input: AtomicU64,
    /// Cycles where the output buffer was smaller than the input block.
    pub truncated: AtomicU64,
    /// `pw_stream_trigger_process` failures.
    pub trigger_failed: AtomicU64,
    /// Capture stream process callbacks.
    pub capture_cycles: AtomicU64,
    /// Frames in the most recent block.
    pub last_frames: AtomicU64,
}

/// Plain copy of [`Stats`] for reporting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub cycles: u64,
    pub no_input: u64,
    pub no_output: u64,
    pub dropped_input: u64,
    pub truncated: u64,
    pub trigger_failed: u64,
    pub capture_cycles: u64,
    pub last_frames: u64,
}

impl Stats {
    pub fn snapshot(&self) -> StatsSnapshot {
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        StatsSnapshot {
            cycles: r(&self.cycles),
            no_input: r(&self.no_input),
            no_output: r(&self.no_output),
            dropped_input: r(&self.dropped_input),
            truncated: r(&self.truncated),
            trigger_failed: r(&self.trigger_failed),
            capture_cycles: r(&self.capture_cycles),
            last_frames: r(&self.last_frames),
        }
    }
}

#[inline]
fn bump(a: &AtomicU64) {
    a.fetch_add(1, Ordering::Relaxed);
}

/// Per-path state used by the data-thread callback.
struct Core<P> {
    processor: P,
    channels: usize,
    default_frames: usize,
    /// Preallocated zeros used as input when no capture buffer is available.
    silence: Box<[f32]>,
    stats: Arc<Stats>,
}

/// User data of whichever stream runs the processing: the *other* stream plus the core.
struct CycleState<P> {
    other: StreamRc,
    core: Core<P>,
}

/// A running processing path. Dropping it disconnects both streams.
pub struct Duplex {
    // Listeners are declared first so they drop (and release their stream
    // handles) before the streams themselves.
    _capture_listener: Box<dyn std::any::Any>,
    _playback_listener: Box<dyn std::any::Any>,
    capture: StreamRc,
    playback: StreamRc,
    stats: Arc<Stats>,
    spec: DuplexSpec,
}

impl std::fmt::Debug for Duplex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Duplex")
            .field("spec", &self.spec)
            .field("capture_node", &self.capture.node_id())
            .field("playback_node", &self.playback.node_id())
            .finish_non_exhaustive()
    }
}

impl Duplex {
    /// Create both streams and connect them. Must be called on the main-loop thread.
    pub fn new<P: Processor>(
        core: &CoreRc,
        spec: DuplexSpec,
        mut processor: P,
    ) -> Result<Self, Error> {
        let channels = spec.positions.len();
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(Error::Spec("channel count must be 1..=MAX_CHANNELS"));
        }
        if spec.latency_frames == 0 || spec.latency_frames as usize > MAX_FRAMES {
            return Err(Error::Spec("latency_frames must be 1..=MAX_FRAMES"));
        }

        let (cap_props, play_props) = stream_properties(&spec);
        let capture = StreamRc::new(core.clone(), &format!("{}.capture", spec.name), cap_props)?;
        let playback = StreamRc::new(core.clone(), &format!("{}.playback", spec.name), play_props)?;

        processor.prepare(spec.rate, MAX_FRAMES);
        let stats = Arc::new(Stats::default());

        let core_state = Core {
            processor,
            channels,
            default_frames: spec.latency_frames as usize,
            silence: vec![0.0; MAX_FRAMES].into_boxed_slice(),
            stats: stats.clone(),
        };

        let state = CycleState {
            other: capture.clone(),
            core: core_state,
        };
        let playback_listener = playback
            .add_local_listener_with_user_data(state)
            .process(|stream, st| run_cycle(&st.other, stream, &mut st.core))
            .register()?;
        let trigger_stats = stats.clone();
        let capture_listener = capture
            .add_local_listener_with_user_data(playback.clone())
            .process(move |stream, playback| {
                bump(&trigger_stats.capture_cycles);
                if playback.trigger_process().is_err() {
                    bump(&trigger_stats.trigger_failed);
                    // Nothing will consume the input this cycle; recycle it.
                    while stream.dequeue_buffer().is_some() {}
                }
            })
            .register()?;

        let format = format_pod(spec.rate, &spec.positions)?;
        let base = StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS;
        // Playback connects first so it is active before capture triggers it.
        {
            let mut params = [Pod::from_bytes(&format).ok_or(Error::Pod)?];
            playback.connect(
                spa::utils::Direction::Output,
                None,
                base | StreamFlags::TRIGGER,
                &mut params,
            )?;
        }
        {
            // ASYNC: the capture stream's buffers are dequeued/queued from the
            // playback stream's process callback, not its own. Without this
            // flag the group only runs every other graph cycle.
            let mut params = [Pod::from_bytes(&format).ok_or(Error::Pod)?];
            capture.connect(
                spa::utils::Direction::Input,
                None,
                base | stream_flag_async(),
                &mut params,
            )?;
        }

        Ok(Self {
            _capture_listener: Box::new(capture_listener),
            _playback_listener: Box::new(playback_listener),
            capture,
            playback,
            stats,
            spec,
        })
    }

    pub fn spec(&self) -> &DuplexSpec {
        &self.spec
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// PipeWire node ids of (capture, playback) streams; `u32::MAX` until connected.
    pub fn node_ids(&self) -> (u32, u32) {
        (self.capture.node_id(), self.playback.node_id())
    }
}

/// `PW_STREAM_FLAG_ASYNC` (PipeWire ≥ 0.3.73). The `pipewire` crate 0.10 has no
/// named constant for it, so it is built from the sys value.
fn stream_flag_async() -> StreamFlags {
    StreamFlags::from_bits_retain(pw::sys::pw_stream_flags_PW_STREAM_FLAG_ASYNC)
}

/// Data-thread callback. RT rules: no allocation, locks, syscalls or logging.
fn run_cycle<P: Processor>(
    capture: &pw::stream::Stream,
    playback: &pw::stream::Stream,
    st: &mut Core<P>,
) {
    // Declaration order matters: `input` drops (re-queues to the capture
    // stream) before `output` is queued to the playback stream.
    let Some(mut output) = playback.dequeue_buffer() else {
        // Still recycle input so the capture side doesn't starve.
        while capture.dequeue_buffer().is_some() {}
        bump(&st.stats.no_output);
        return;
    };
    let mut input = None;
    while let Some(b) = capture.dequeue_buffer() {
        if input.replace(b).is_some() {
            bump(&st.stats.dropped_input);
        }
    }

    let ch = st.channels;
    let requested = usize::try_from(output.requested()).unwrap_or(0);

    let frames = {
        let mut ins: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        let mut in_frames = usize::MAX;
        if let Some(buf) = input.as_mut() {
            let mut got = 0;
            for (slot, d) in ins.iter_mut().zip(buf.datas_mut().iter_mut()).take(ch) {
                let (off, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
                let samples = d
                    .data()
                    .and_then(|bytes| {
                        let end = off.saturating_add(size).min(bytes.len());
                        bytemuck::try_cast_slice::<u8, f32>(&bytes[off.min(end)..end]).ok()
                    })
                    .unwrap_or(&[]);
                *slot = samples;
                in_frames = in_frames.min(samples.len());
                got += 1;
            }
            if got < ch {
                in_frames = 0;
            }
        } else {
            bump(&st.stats.no_input);
        }

        let mut outs: [&mut [f32]; MAX_CHANNELS] = Default::default();
        let mut out_cap = usize::MAX;
        let mut got = 0;
        for (slot, d) in outs.iter_mut().zip(output.datas_mut().iter_mut()).take(ch) {
            let samples = d
                .data()
                .and_then(|bytes| bytemuck::try_cast_slice_mut::<u8, f32>(bytes).ok())
                .unwrap_or(&mut []);
            out_cap = out_cap.min(samples.len());
            *slot = samples;
            got += 1;
        }
        if got < ch {
            out_cap = 0;
        }

        // An empty or incomplete input buffer is treated like a missing one:
        // emit a block of silence rather than a zero-length block.
        let wanted = if in_frames != usize::MAX && in_frames > 0 {
            in_frames
        } else if requested > 0 {
            requested
        } else {
            st.default_frames
        };
        let frames = wanted.min(out_cap).min(st.silence.len());
        if frames < wanted {
            bump(&st.stats.truncated);
        }

        for s in ins.iter_mut().take(ch) {
            *s = if s.len() >= frames {
                &s[..frames]
            } else {
                &st.silence[..frames]
            };
        }
        for o in outs.iter_mut().take(ch) {
            let full = std::mem::take(o);
            *o = &mut full[..frames];
        }
        st.processor.process(&ins[..ch], &mut outs[..ch]);
        frames
    };

    for d in output.datas_mut().iter_mut().take(ch) {
        let chunk = d.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = std::mem::size_of::<f32>() as i32;
        *chunk.size_mut() = (frames * std::mem::size_of::<f32>()) as u32;
    }
    st.stats.last_frames.store(frames as u64, Ordering::Relaxed);
    bump(&st.stats.cycles);
}

fn stream_properties(spec: &DuplexSpec) -> (PropertiesBox, PropertiesBox) {
    let group = spec.name.clone();
    let latency = format!("{}/{}", spec.latency_frames, spec.rate);
    let positions = spec
        .positions
        .iter()
        .map(|p| position_name(*p))
        .collect::<Vec<_>>()
        .join(",");

    let common = |p: &mut PropertiesBox| {
        p.insert(*pw::keys::MEDIA_TYPE, "Audio");
        p.insert(*pw::keys::NODE_GROUP, group.as_str());
        p.insert(*pw::keys::NODE_LINK_GROUP, group.as_str());
        p.insert(*pw::keys::NODE_LATENCY, latency.as_str());
        p.insert("audio.position", positions.as_str());
        p.insert("gprox.path", spec.name.as_str());
    };

    let mut cap = PropertiesBox::new();
    let mut play = PropertiesBox::new();
    common(&mut cap);
    common(&mut play);
    let priority = spec.priority.map(|p| p.to_string());

    match spec.direction {
        Direction::Render => {
            // Virtual sink apps play into.
            cap.insert(*pw::keys::NODE_NAME, spec.name.as_str());
            cap.insert(*pw::keys::NODE_DESCRIPTION, spec.description.as_str());
            cap.insert(*pw::keys::MEDIA_CLASS, "Audio/Sink");
            cap.insert(*pw::keys::NODE_VIRTUAL, "true");
            if let Some(p) = &priority {
                cap.insert(*pw::keys::PRIORITY_SESSION, p.as_str());
            }
            // Stream to the headset; passive so an idle virtual sink doesn't
            // keep the headset running.
            play.insert(*pw::keys::NODE_NAME, format!("{}.output", spec.name));
            play.insert(
                *pw::keys::NODE_DESCRIPTION,
                format!("{} output", spec.description),
            );
            play.insert(*pw::keys::NODE_PASSIVE, "true");
            play.insert(*pw::keys::MEDIA_CATEGORY, "Playback");
            play.insert("stream.dont-remix", "true");
            if let Some(t) = &spec.target {
                play.insert(*pw::keys::TARGET_OBJECT, t.as_str());
            }
        }
        Direction::Capture => {
            // Stream from the headset mic; passive so the mic is only opened
            // while something records from the virtual source.
            cap.insert(*pw::keys::NODE_NAME, format!("{}.input", spec.name));
            cap.insert(
                *pw::keys::NODE_DESCRIPTION,
                format!("{} input", spec.description),
            );
            cap.insert(*pw::keys::NODE_PASSIVE, "true");
            cap.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
            cap.insert("stream.dont-remix", "true");
            if let Some(t) = &spec.target {
                cap.insert(*pw::keys::TARGET_OBJECT, t.as_str());
            }
            // Virtual source apps record from.
            play.insert(*pw::keys::NODE_NAME, spec.name.as_str());
            play.insert(*pw::keys::NODE_DESCRIPTION, spec.description.as_str());
            play.insert(*pw::keys::MEDIA_CLASS, "Audio/Source");
            play.insert(*pw::keys::NODE_VIRTUAL, "true");
            if let Some(p) = &priority {
                play.insert(*pw::keys::PRIORITY_SESSION, p.as_str());
            }
        }
    }
    (cap, play)
}

fn position_name(p: u32) -> &'static str {
    match p {
        position::MONO => "MONO",
        position::FL => "FL",
        position::FR => "FR",
        _ => "UNK",
    }
}

/// Serialise an `EnumFormat` pod for planar f32 at a fixed rate and layout.
fn format_pod(rate: u32, positions: &[u32]) -> Result<Vec<u8>, Error> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32P);
    info.set_rate(rate);
    info.set_channels(positions.len() as u32);
    let mut pos = [0u32; spa::param::audio::MAX_CHANNELS];
    pos[..positions.len()].copy_from_slice(positions);
    info.set_position(pos);

    let obj = spa::pod::Value::Object(spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    });
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &obj)
        .map(|(c, _)| c.into_inner())
        .map_err(|_| Error::Pod)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(direction: Direction) -> DuplexSpec {
        DuplexSpec {
            direction,
            name: "gprox_test".into(),
            description: "Test".into(),
            target: Some("alsa_x".into()),
            positions: vec![position::FL, position::FR],
            rate: 48_000,
            latency_frames: 256,
            priority: Some(3000),
        }
    }

    #[test]
    fn render_properties() {
        let (cap, play) = stream_properties(&spec(Direction::Render));
        assert_eq!(cap.get("media.class"), Some("Audio/Sink"));
        assert_eq!(cap.get("node.name"), Some("gprox_test"));
        assert_eq!(play.get("target.object"), Some("alsa_x"));
        assert_eq!(play.get("node.latency"), Some("256/48000"));
        assert_eq!(cap.get("audio.position"), Some("FL,FR"));
        assert_eq!(cap.get("node.link-group"), play.get("node.link-group"));
        assert_eq!(cap.get("priority.session"), Some("3000"));
        assert_eq!(play.get("priority.session"), None);
    }

    #[test]
    fn capture_properties() {
        let (cap, play) = stream_properties(&spec(Direction::Capture));
        assert_eq!(play.get("media.class"), Some("Audio/Source"));
        assert_eq!(cap.get("target.object"), Some("alsa_x"));
        assert_eq!(cap.get("node.passive"), Some("true"));
        assert_eq!(play.get("priority.session"), Some("3000"));
        assert_eq!(cap.get("priority.session"), None);
    }

    #[test]
    fn format_pod_serialises() {
        let bytes = format_pod(48_000, &[position::MONO]).unwrap();
        assert!(Pod::from_bytes(&bytes).is_some());
    }
}
