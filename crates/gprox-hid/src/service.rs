//! Background headset service: keeps [`HeadsetStatus`] current and runs the
//! two allowed writes.
//!
//! One worker thread owns the device. It handles:
//!
//! - **Dongle hotplug:** while no dongle is open it looks for one in sysfs
//!   every second. The hidraw reader thread reports an error when the node
//!   goes away (re-plug, USB reset on resume), and the worker starts looking
//!   again.
//! - **Headset power:** the dongle sends an ADC notification with 0 mV when
//!   the headset switches off and one with the voltage when it comes back.
//!   On "on" the worker re-reads everything, like G HUB. If a
//!   request times out or fails, the headset counts as off until the next
//!   "on" notification, with a slow probe as a fallback.
//! - **Resume from sleep:** `Instant` stops during suspend while the wall
//!   clock doesn't; when the two drift apart by more than a few seconds the
//!   worker re-reads everything.
//! - **Battery:** polled every [`Config::battery_poll`], plus the device's
//!   own notifications.
//!
//! On every (re)connect the worker re-sends the user's sidetone level if one
//! was set through gprox, as G HUB does after power-on. Other
//! than that, nothing is written unless a client asks.
//!
//! # Reply attribution
//!
//! Feature commands go to indices from Root.getFeature, and a getFeature
//! reply (`11 ff 00 0b <index>`) does not say which feature it answers. With
//! one fixed sw_id (G HUB's, [`crate::hidpp::SW_ID`]) a late reply to an
//! earlier lookup, or another program's lookup reply with the same sw_id
//! (hidraw gives every reader every input report), would be taken for the
//! answer and send later commands to the wrong feature: a read can become a
//! write, or a frame can reach a feature outside the allow-list. So the
//! worker counts Root requests sent and Root replies with its sw_id seen, and
//! only uses a map when the counts show every reply was its own:
//!
//! - a lookup starts only when no earlier Root reply is still due (it waits
//!   up to [`STALE_AFTER`] after the last Root request for one that timed out);
//! - after the lookups it listens for [`SETTLE`]; an extra Root reply in that
//!   window, or one that was already taken, means a reply was mis-attributed
//!   and the map is thrown away (retried at the next probe);
//! - a map that differs from the last one accepted is looked up once more
//!   before it is used;
//! - an unexplained Root reply while connected drops the map: feature
//!   commands (and so writes) are refused until a new lookup passes.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gprox_core::control::{BatteryStatus, HeadsetStatus};

use crate::hidpp::{
    AutoOffMinutes, BatteryReading, Command, Feature, FeatureMap, HidppError, Message,
    SidetoneLevel, Target, hex,
};
use crate::hidraw::{Hidraw, Incoming, Transport};
use crate::mute::{AFTER_POWER, AFTER_WRITE, AudioObservation, MuteTracker};
use crate::power::{ChargeTracker, PowerModel};

/// How long to wait for a reply (G HUB's replies arrive within ~50 ms).
const REPLY_TIMEOUT: Duration = Duration::from_millis(1000);
/// How long a client waits for a write to finish.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// Sysfs scan interval while no dongle is present.
const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// Wall clock vs monotonic drift that counts as a resume from sleep.
const RESUME_DRIFT: Duration = Duration::from_secs(5);
/// How long after the feature lookups the worker listens for a Root reply
/// that no request explains (the device answers within ~50 ms).
const SETTLE: Duration = Duration::from_millis(200);
/// How long a Root reply that timed out can still arrive late. After this it
/// counts as lost (the headset was off or out of range).
const STALE_AFTER: Duration = Duration::from_secs(3);

/// Asked on every worker tick for what the mic path has seen (mute
/// detection, [`crate::mute`]).
pub type AudioSource = Box<dyn FnMut() -> AudioObservation + Send>;

pub struct Config {
    /// G HUB's power model; without it only the voltage is reported.
    pub power_model: Option<PowerModel>,
    /// Sidetone to re-apply on every connect (the user's last choice).
    pub sidetone: Option<SidetoneLevel>,
    /// Battery poll interval while connected.
    pub battery_poll: Duration,
    /// Probe interval while the dongle is open but the headset is silent.
    pub probe_interval: Duration,
    /// The mic path's audio observations, if there is a mic path.
    pub audio: Option<AudioSource>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("power_model", &self.power_model.is_some())
            .field("sidetone", &self.sidetone)
            .field("battery_poll", &self.battery_poll)
            .field("probe_interval", &self.probe_interval)
            .field("audio", &self.audio.is_some())
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            power_model: None,
            sidetone: None,
            battery_poll: Duration::from_secs(60),
            probe_interval: Duration::from_secs(15),
            audio: None,
        }
    }
}

/// Opens the device: returns `None` when there is none, else the transport.
/// The second argument is the sink for input reports. Real use:
/// [`hidraw_opener`]; tests pass a simulated device.
pub type Opener = Box<
    dyn FnMut(
            Box<dyn FnMut(Incoming) -> bool + Send>,
        ) -> Option<std::io::Result<Box<dyn Transport>>>
        + Send,
>;

/// Opens the real dongle via sysfs + hidraw.
pub fn hidraw_opener() -> Opener {
    Box::new(|sink| {
        let path = crate::hidraw::find_dongle()?;
        Some(Hidraw::open(&path, sink).map(|h| {
            tracing::info!(path = %h.path().display(), "headset dongle opened");
            Box::new(h) as Box<dyn Transport>
        }))
    })
}

type Reply = Sender<Result<HeadsetStatus, String>>;

enum Event {
    Input {
        generation: u64,
        at: Instant,
        data: Incoming,
    },
    Write {
        cmd: Command,
        reply: Reply,
    },
}

/// Handle to the worker thread. Cheap to clone.
#[derive(Debug, Clone)]
pub struct HeadsetService {
    tx: Sender<Event>,
    status: Arc<Mutex<HeadsetStatus>>,
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Input {
                generation, data, ..
            } => write!(f, "Input({generation}, {data:?})"),
            Event::Write { cmd, .. } => write!(f, "Write({cmd:?})"),
        }
    }
}

impl HeadsetService {
    /// Start the worker on the real dongle.
    pub fn spawn(cfg: Config) -> std::io::Result<HeadsetService> {
        Self::spawn_with(cfg, hidraw_opener())
    }

    pub fn spawn_with(cfg: Config, opener: Opener) -> std::io::Result<HeadsetService> {
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(HeadsetStatus::default()));
        let mut w = Worker {
            cfg,
            opener,
            rx,
            tx: tx.clone(),
            status: status.clone(),
            dev: None,
            generation: 0,
            pending: VecDeque::new(),
            root: RootReplies::default(),
            verified: None,
            tracker: ChargeTracker::default(),
            mute: MuteTracker::default(),
            clock: (Instant::now(), SystemTime::now()),
        };
        std::thread::Builder::new()
            .name("gprox-hid".into())
            .spawn(move || w.run())?;
        Ok(HeadsetService { tx, status })
    }

    pub fn status(&self) -> HeadsetStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn write(&self, cmd: Command) -> Result<HeadsetStatus, String> {
        let (reply, rx) = channel();
        self.tx
            .send(Event::Write { cmd, reply })
            .map_err(|_| "headset service stopped".to_owned())?;
        rx.recv_timeout(CLIENT_TIMEOUT)
            .map_err(|_| "headset did not answer in time".to_owned())?
    }

    /// Set the sidetone level; returns the status after the device confirmed.
    pub fn set_sidetone(&self, level: SidetoneLevel) -> Result<HeadsetStatus, String> {
        self.write(Command::SetSidetone(level))
    }

    /// Set the auto-off timer; returns the status after the read-back.
    pub fn set_auto_off(&self, minutes: AutoOffMinutes) -> Result<HeadsetStatus, String> {
        self.write(Command::SetAutoOff(minutes))
    }
}

struct Device {
    transport: Box<dyn Transport>,
    map: FeatureMap,
    /// The headset answers.
    connected: bool,
    /// Re-read everything at the next chance.
    resync: bool,
    next_probe: Instant,
    next_battery: Instant,
}

#[derive(Debug)]
enum TxError {
    /// No reply in time.
    Timeout,
    /// HID++ error reply.
    Device(u8),
    /// The device node went away (the reader saw the end of the stream).
    Gone(String),
    /// Writing to the node failed while it may still be there. The node stays
    /// open: its reader only ends when the node goes away, so closing and
    /// reopening here would leak a blocked reader thread and its file
    /// descriptor per retry.
    Write(String),
    Hidpp(HidppError),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxError::Timeout => write!(f, "no reply from the headset"),
            TxError::Device(c) => write!(f, "HID++ error {c:#04x}"),
            TxError::Gone(e) => write!(f, "dongle gone: {e}"),
            TxError::Write(e) => write!(f, "write to the headset dongle failed: {e}"),
            TxError::Hidpp(e) => write!(f, "{e}"),
        }
    }
}

/// Root requests sent and Root replies with gprox's sw_id seen, since the
/// dongle was opened ("Reply attribution" in the module docs).
#[derive(Debug, Default)]
struct RootReplies {
    sent: u32,
    seen: u32,
    last_sent: Option<Instant>,
    /// A Root reply arrived that no request explains: some lookup answer
    /// may have been somebody else's.
    unexplained: bool,
}

impl RootReplies {
    fn sent(&mut self) {
        self.sent += 1;
        self.last_sent = Some(Instant::now());
    }

    fn seen(&mut self) {
        self.seen += 1;
        if self.seen > self.sent {
            self.unexplained = true;
        }
    }

    /// Forget lost and foreign replies before a lookup.
    fn rebase(&mut self) {
        self.seen = self.sent;
        self.unexplained = false;
    }
}

struct Worker {
    cfg: Config,
    opener: Opener,
    rx: Receiver<Event>,
    tx: Sender<Event>,
    status: Arc<Mutex<HeadsetStatus>>,
    dev: Option<Device>,
    /// Bumped on every open, so a stale reader's messages are ignored.
    generation: u64,
    /// Events that arrived while waiting for a reply.
    pending: VecDeque<Event>,
    root: RootReplies,
    /// The last feature map that passed the attribution checks.
    verified: Option<FeatureMap>,
    tracker: ChargeTracker,
    mute: MuteTracker,
    /// Last (monotonic, wall) clock pair, for resume detection.
    clock: (Instant, SystemTime),
}

impl Worker {
    fn update(&self, f: impl FnOnce(&mut HeadsetStatus)) {
        f(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn snapshot(&self) -> HeadsetStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn run(&mut self) {
        loop {
            if self.dev.is_none() {
                self.try_open();
            }
            self.check_resume();
            if let Some(a) = &mut self.cfg.audio {
                self.mute.audio(a());
            }
            self.mute.tick(Instant::now());
            self.publish_mute();
            if self.dev.as_ref().is_some_and(|d| d.resync) {
                self.sync();
            }
            if let Some(d) = &self.dev
                && d.connected
                && Instant::now() >= d.next_battery
            {
                self.read_battery();
            }
            if let Some(d) = &self.dev
                && !d.connected
                && Instant::now() >= d.next_probe
            {
                if let Some(d) = &mut self.dev {
                    d.resync = true;
                }
                continue;
            }
            let ev = match self.pending.pop_front() {
                Some(ev) => ev,
                None => match self.rx.recv_timeout(self.wait()) {
                    Ok(ev) => ev,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            self.handle(ev);
        }
    }

    /// Time until the next scheduled job.
    fn wait(&self) -> Duration {
        let now = Instant::now();
        let mut next = match &self.dev {
            None => now + SCAN_INTERVAL,
            Some(d) if d.connected => d.next_battery,
            Some(d) => d.next_probe,
        };
        if let Some(t) = self.mute.deadline() {
            next = next.min(t + Duration::from_millis(1));
        }
        next.saturating_duration_since(now)
            .clamp(Duration::from_millis(10), SCAN_INTERVAL)
    }

    fn try_open(&mut self) {
        let generation = self.generation + 1;
        let tx = self.tx.clone();
        let sink = Box::new(move |data: Incoming| {
            tx.send(Event::Input {
                generation,
                at: Instant::now(),
                data,
            })
            .is_ok()
        });
        match (self.opener)(sink) {
            None => self.update(|s| {
                if s.dongle || s.error.is_some() {
                    *s = HeadsetStatus::default();
                }
            }),
            Some(Err(e)) => {
                let msg = format!("cannot open the headset dongle: {e}");
                let changed = self.snapshot().error.as_deref() != Some(msg.as_str());
                if changed {
                    tracing::warn!("{msg}");
                }
                self.update(|s| {
                    *s = HeadsetStatus {
                        error: Some(msg),
                        ..Default::default()
                    }
                });
            }
            Some(Ok(transport)) => {
                self.generation = generation;
                self.root = RootReplies::default();
                let now = Instant::now();
                self.dev = Some(Device {
                    transport,
                    map: FeatureMap::default(),
                    connected: false,
                    resync: true,
                    next_probe: now,
                    next_battery: now,
                });
                self.mute.quiet(now, AFTER_POWER);
                self.update(|s| {
                    *s = HeadsetStatus {
                        dongle: true,
                        ..Default::default()
                    }
                });
            }
        }
    }

    fn check_resume(&mut self) {
        let now = (Instant::now(), SystemTime::now());
        let slept = slept(self.clock, now);
        self.clock = now;
        if let Some(slept) = slept
            && let Some(d) = &mut self.dev
        {
            tracing::info!(?slept, "resume from sleep: re-reading the headset");
            d.resync = true;
            self.mute.quiet(now.0, AFTER_POWER);
        }
    }

    /// Show the mute state while the headset is connected.
    fn publish_mute(&self) {
        let connected = self.dev.as_ref().is_some_and(|d| d.connected);
        let m = self.mute.muted().filter(|_| connected);
        if self.snapshot().mic_muted != m {
            tracing::debug!(muted = ?m, "mic mute");
            self.update(|s| s.mic_muted = m);
        }
    }

    fn drop_device(&mut self, why: &str) {
        if self.dev.take().is_some() {
            tracing::info!("headset dongle closed: {why}");
        }
        self.tracker = ChargeTracker::default();
        self.mute.quiet(Instant::now(), AFTER_POWER);
        self.update(|s| *s = HeadsetStatus::default());
    }

    /// A write to the node failed: keep it open, show the error, and let the
    /// probe retry after [`Config::probe_interval`].
    fn write_failed(&mut self, msg: &str) {
        if self.snapshot().error.as_deref() != Some(msg) {
            tracing::warn!("{msg}");
        }
        self.mark_off(msg);
        let msg = msg.to_owned();
        self.update(|s| s.error = Some(msg));
    }

    fn mark_off(&mut self, why: &str) {
        if let Some(d) = &mut self.dev {
            if d.connected {
                tracing::info!("headset off or out of range ({why})");
            }
            d.connected = false;
            d.resync = false;
            d.map = FeatureMap::default();
            d.next_probe = Instant::now() + self.cfg.probe_interval;
        }
        self.tracker = ChargeTracker::default();
        self.mute.quiet(Instant::now(), AFTER_POWER);
        self.update(|s| {
            *s = HeadsetStatus {
                dongle: true,
                ..Default::default()
            }
        });
    }

    /// Send `cmd` and wait for its reply. Other input is queued.
    fn transact(&mut self, cmd: Command) -> Result<[u8; 16], TxError> {
        let Some(d) = &mut self.dev else {
            return Err(TxError::Gone("no dongle".into()));
        };
        let root = cmd.target() == Target::Root;
        if !root && self.root.unexplained {
            return Err(TxError::Hidpp(HidppError::Protocol(
                "an unexplained HID++ reply arrived; the feature map is not trusted".into(),
            )));
        }
        let report = cmd.encode(&d.map).map_err(TxError::Hidpp)?;
        if cmd.is_write() {
            tracing::info!(?cmd, frame = %hex(report.as_bytes()), "HID++ write");
        }
        d.transport
            .send(&report)
            .map_err(|e| TxError::Write(e.to_string()))?;
        if root {
            self.root.sent();
        }
        if cmd.is_write() {
            // The dongle answers a write with a consumer-control report
            // that is not a mute press.
            self.mute.quiet(Instant::now(), AFTER_WRITE);
        }
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = match self.rx.recv_timeout(left) {
                Ok(ev) => ev,
                Err(_) => return Err(TxError::Timeout),
            };
            match ev {
                Event::Input {
                    generation,
                    at,
                    data,
                } if generation == self.generation => {
                    let data = data.map_err(|e| TxError::Gone(e.to_string()))?;
                    if data.is_empty() {
                        return Err(TxError::Gone("end of stream".into()));
                    }
                    let msg = Message::parse(&data);
                    let Some(d) = &self.dev else {
                        return Err(TxError::Gone("no dongle".into()));
                    };
                    let root_reply = msg.is_root_reply();
                    if root_reply {
                        self.root.seen();
                    }
                    if cmd.answers(&d.map, &msg) {
                        return match msg {
                            Message::Reply { params, .. } => Ok(params),
                            Message::Error { code, .. } => Err(TxError::Device(code)),
                            _ => unreachable!("answers() only matches replies and errors"),
                        };
                    }
                    if root_reply {
                        tracing::debug!(report = %hex(&data), "unexplained HID++ Root reply");
                        continue;
                    }
                    self.pending.push_back(Event::Input {
                        generation,
                        at,
                        data: Ok(data),
                    });
                }
                Event::Input { .. } => {}
                other => self.pending.push_back(other),
            }
        }
    }

    /// Run `cmd`; on failure classify it (gone → close, else headset off).
    fn call(&mut self, cmd: Command) -> Option<[u8; 16]> {
        match self.transact(cmd) {
            Ok(p) => Some(p),
            Err(TxError::Gone(e)) => {
                self.drop_device(&e);
                None
            }
            Err(e @ TxError::Write(_)) => {
                self.write_failed(&e.to_string());
                None
            }
            Err(e) => {
                tracing::debug!(?cmd, error = %e, "HID++ request failed");
                self.mark_off(&e.to_string());
                None
            }
        }
    }

    /// Move everything queued to `pending`, counting and dropping Root
    /// replies (they answer nothing any more).
    fn drain_root_replies(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.pending.push_back(ev);
        }
        let generation = self.generation;
        let before = self.pending.len();
        self.pending.retain(|ev| {
            !matches!(ev, Event::Input { generation: g, data: Ok(d), .. }
                if *g == generation && Message::parse(d).is_root_reply())
        });
        for _ in self.pending.len()..before {
            self.root.seen();
        }
    }

    /// Listen for [`SETTLE`] after the lookups; false if a Root reply that no
    /// request explains arrived (now or during the lookups).
    fn settle(&mut self) -> bool {
        let deadline = Instant::now() + SETTLE;
        while !self.root.unexplained {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.rx.recv_timeout(left) {
                Ok(Event::Input {
                    generation,
                    data: Ok(data),
                    ..
                }) if generation == self.generation && Message::parse(&data).is_root_reply() => {
                    self.root.seen();
                }
                Ok(ev) => self.pending.push_back(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
        !self.root.unexplained
    }

    /// The lookup result can't be trusted: headset counts as off, the probe
    /// tries again.
    fn reject_map(&mut self, why: &str) {
        tracing::warn!("headset feature lookup not used: {why}");
        self.mark_off(why);
        let msg = format!("headset feature lookup not used: {why}; retrying");
        self.update(|s| s.error = Some(msg));
    }

    /// Look the features up; `None` if the result can't be used (the reason
    /// is handled here).
    fn look_up_features(&mut self) -> Option<FeatureMap> {
        self.drain_root_replies();
        if self.root.seen < self.root.sent
            && let Some(due) = self.root.last_sent.map(|t| t + STALE_AFTER)
            && Instant::now() < due
        {
            tracing::debug!("waiting for a late HID++ reply before the feature lookup");
            self.mark_off("waiting for a late HID++ reply");
            if let Some(d) = &mut self.dev {
                d.next_probe = due;
            }
            return None;
        }
        self.root.rebase();
        let mut map = FeatureMap::default();
        for f in Feature::ALL {
            let p = self.call(Command::GetFeature(f))?;
            if let Err(e) = map.set(f, p[0]) {
                tracing::warn!(error = %e, "headset feature table");
                self.mark_off(&e.to_string());
                return None;
            }
        }
        if !self.settle() {
            self.reject_map(
                "a HID++ reply arrived that gprox did not ask for \
                 (a late reply, or another program using the same sw_id)",
            );
            return None;
        }
        if self.verified.as_ref().is_some_and(|v| *v != map) {
            tracing::warn!(old = ?self.verified, new = ?map, "headset feature table changed");
            self.verified = Some(map);
            self.reject_map("the feature table changed; checking it again");
            return None;
        }
        self.verified = Some(map.clone());
        Some(map)
    }

    /// (Re-)read everything after an open, power-on or resume.
    fn sync(&mut self) {
        let Some(d) = &mut self.dev else { return };
        d.resync = false;
        d.map = FeatureMap::default();
        let Some(map) = self.look_up_features() else {
            return;
        };
        if let Some(d) = &mut self.dev {
            d.map = map;
            if !d.connected {
                tracing::info!(map = ?d.map, "headset connected");
            }
            d.connected = true;
        }
        self.update(|s| {
            s.connected = true;
            s.error = None;
        });
        if !self.read_battery() {
            return;
        }
        let Some(p) = self.call(Command::GetSidetone) else {
            return;
        };
        let current = p[0];
        self.update(|s| s.sidetone = Some(current));
        if let Some(want) = self.cfg.sidetone
            && want.get() != current
        {
            tracing::info!(level = want.get(), was = current, "re-applying sidetone");
            if let Some(p) = self.call(Command::SetSidetone(want)) {
                self.update(|s| s.sidetone = Some(p[0]));
            }
        }
        if let Some(p) = self.call(Command::GetAutoOff) {
            self.update(|s| s.auto_off_minutes = Some(p[0]));
        }
    }

    /// Read the battery; false if the headset didn't answer.
    fn read_battery(&mut self) -> bool {
        if let Some(d) = &mut self.dev {
            d.next_battery = Instant::now() + self.cfg.battery_poll;
        }
        match self.call(Command::ReadBattery) {
            Some(p) => {
                self.battery(BatteryReading::parse(&p));
                true
            }
            None => false,
        }
    }

    fn battery(&mut self, b: BatteryReading) {
        let charge = self
            .cfg
            .power_model
            .as_ref()
            .map(|m| self.tracker.estimate(m, b.millivolts, b.charging()));
        tracing::debug!(mv = b.millivolts, flags = b.flags, ?charge, "battery");
        self.update(|s| {
            s.battery = Some(BatteryStatus {
                millivolts: b.millivolts,
                flags: b.flags,
                charging: b.charging(),
                percent: charge.map(|c| c.percent.round() as u8),
                hours_left: charge.and_then(|c| c.hours_left),
            })
        });
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Input { generation, .. } if generation != self.generation => {}
            Event::Input { data: Err(e), .. } => self.drop_device(&e.to_string()),
            Event::Input { data: Ok(data), .. } if data.is_empty() => {
                self.drop_device("end of stream")
            }
            Event::Input {
                at, data: Ok(data), ..
            } => self.input(at, &data),
            Event::Write { cmd, reply } => {
                let r = self.client_write(cmd);
                let _ = reply.send(r);
            }
        }
    }

    /// An unsolicited input report.
    fn input(&mut self, at: Instant, data: &[u8]) {
        let msg = Message::parse(data);
        let Some(d) = &self.dev else { return };
        match msg {
            // The ADC notification. While the headset is off its index isn't
            // known, so any function-0 event counts.
            Message::Event {
                index,
                function: 0,
                params,
            } if d.map.feature_at(index) == Some(Feature::AdcMeasurement)
                || (!d.connected && d.map.feature_at(index).is_none()) =>
            {
                let b = BatteryReading::parse(&params);
                if b.headset_off() {
                    self.mark_off("power-off notification");
                } else if !d.connected {
                    tracing::info!(mv = b.millivolts, "headset power-on notification");
                    if let Some(d) = &mut self.dev {
                        d.resync = true;
                    }
                    self.mute.quiet(at, AFTER_POWER);
                } else {
                    self.battery(b);
                }
            }
            Message::Other(ref b) => {
                tracing::trace!(report = %hex(b), "other input report");
                self.mute.report(at, b);
            }
            ref m if m.is_root_reply() => {
                self.root.seen();
                if self.root.unexplained && d.connected {
                    tracing::warn!(
                        report = %hex(data),
                        "unexplained HID++ Root reply (another program using the same \
                         sw_id?): feature map dropped until the next lookup"
                    );
                    self.mark_off("unexplained HID++ reply");
                }
            }
            m => tracing::debug!(?m, "unexpected HID++ input"),
        }
    }

    fn client_write(&mut self, cmd: Command) -> Result<HeadsetStatus, String> {
        if !self.dev.as_ref().is_some_and(|d| d.connected) {
            return Err(if self.dev.is_none() {
                "headset dongle not found".into()
            } else {
                "headset is off or out of range".into()
            });
        }
        match self.transact(cmd) {
            Ok(p) => {
                match cmd {
                    Command::SetSidetone(l) => {
                        self.cfg.sidetone = Some(l);
                        self.update(|s| s.sidetone = Some(p[0]));
                    }
                    Command::SetAutoOff(_) => {
                        // G HUB reads the value back after setting it.
                        if let Some(p) = self.call(Command::GetAutoOff) {
                            self.update(|s| s.auto_off_minutes = Some(p[0]));
                        }
                    }
                    _ => {}
                }
                Ok(self.snapshot())
            }
            Err(e) => {
                let msg = e.to_string();
                match e {
                    TxError::Gone(why) => self.drop_device(&why),
                    TxError::Write(_) => self.write_failed(&msg),
                    TxError::Timeout => self.mark_off(&msg),
                    TxError::Device(_) | TxError::Hidpp(_) => {}
                }
                Err(msg)
            }
        }
    }
}

/// How long the machine slept between two (monotonic, wall clock) samples,
/// if it did: `Instant` doesn't advance during suspend, the wall clock does.
fn slept(prev: (Instant, SystemTime), now: (Instant, SystemTime)) -> Option<Duration> {
    let dm = now.0.duration_since(prev.0);
    let dw = now.1.duration_since(prev.1).unwrap_or_default();
    (dw > dm + RESUME_DRIFT).then(|| dw - dm)
}

#[cfg(test)]
mod tests;
