//! The worker against a simulated dongle that answers like the real one
//! did in USB captures (indices 5/6, replies echo the first 4 bytes).
//! The simulated feature table also has a DFU feature, so a frame sent to the
//! wrong index can reach it; [`assert_frames_allowed`] checks every frame by
//! the *feature* at its index, not by the index.

use super::*;
use crate::hidpp::{BLOCKED_FEATURE_IDS, REPORT_LEN, Report};

type Sink = Box<dyn FnMut(Incoming) -> bool + Send>;

/// Decides what the simulated dongle delivers for one request: gets the
/// frame and the reply the device would give, returns the input reports to
/// deliver, in order (hold, reorder, inject).
type Script = Box<dyn FnMut(&[u8; REPORT_LEN], Option<Vec<u8>>) -> Vec<Vec<u8>> + Send>;

/// The headset's feature table (index, feature id): the indices seen in the
/// USB captures, plus DFU (0x00D0) at 0x0c for the tests that check no frame
/// ever reaches a feature outside the allow-list.
const TABLE: [(u8, u16); 3] = [(5, 0x8300), (6, 0x1F20), (0x0c, 0x00D0)];

#[derive(Default)]
struct SimState {
    /// The dongle is plugged in.
    present: bool,
    /// The headset is on.
    on: bool,
    sidetone: u8,
    auto_off: u8,
    millivolts: u16,
    flags: u8,
    /// Every frame the worker sent.
    sent: Vec<[u8; REPORT_LEN]>,
    /// The feature id at each sent frame's index when it was sent (0 = Root).
    sent_to: Vec<Option<u16>>,
    /// How many times the dongle was opened.
    opens: usize,
    /// Every write to the node fails (EPIPE) while the node stays.
    fail_writes: bool,
    /// (index, feature id); empty = [`TABLE`].
    table: Vec<(u8, u16)>,
    script: Option<Script>,
}

impl SimState {
    fn table(&self) -> &[(u8, u16)] {
        if self.table.is_empty() {
            &TABLE
        } else {
            &self.table
        }
    }

    fn feature_at(&self, index: u8) -> Option<u16> {
        if index == 0 {
            return Some(0);
        }
        self.table()
            .iter()
            .find(|&&(i, _)| i == index)
            .map(|&(_, f)| f)
    }

    fn index_of(&self, id: u16) -> u8 {
        self.table()
            .iter()
            .find(|&&(_, f)| f == id)
            .map_or(0, |&(i, _)| i)
    }
}

/// Whether a frame sent to `feature` is a write.
fn is_write(feature: Option<u16>, f: &[u8; REPORT_LEN]) -> bool {
    matches!((feature, f[3] >> 4), (Some(0x8300), 1) | (Some(0x1F20), 2))
}

#[derive(Clone, Default)]
struct Sim {
    state: Arc<Mutex<SimState>>,
    /// Where input reports go. Its own lock, held while delivering: the test
    /// thread and the worker (replies) push concurrently, and each report
    /// must arrive, in order, not be dropped while the other one delivers.
    sink: Arc<Mutex<Option<Sink>>>,
}

impl Sim {
    fn with<R>(&self, f: impl FnOnce(&mut SimState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    fn push(&self, data: Incoming) {
        if let Some(k) = self.sink.lock().unwrap().as_mut() {
            k(data);
        }
    }

    fn event(&self, mv: u16, flags: u8) {
        let mut b = vec![0u8; REPORT_LEN];
        let adc = self.with(|s| s.index_of(0x1F20));
        b[..4].copy_from_slice(&[0x11, 0xff, adc, 0x00]);
        b[4..6].copy_from_slice(&mv.to_be_bytes());
        b[6] = flags;
        self.push(Ok(b));
    }

    fn unplug(&self) {
        self.with(|s| s.present = false);
        self.push(Err(std::io::Error::from_raw_os_error(19)));
        *self.sink.lock().unwrap() = None;
    }

    fn opener(&self) -> Opener {
        let sim = self.clone();
        Box::new(move |sink| {
            if !sim.with(|s| s.present) {
                return None;
            }
            sim.with(|s| s.opens += 1);
            *sim.sink.lock().unwrap() = Some(sink);
            Some(Ok(Box::new(SimTransport(sim.clone())) as Box<dyn Transport>))
        })
    }

    /// Every write frame sent, by the feature at its index.
    fn writes(&self) -> Vec<[u8; REPORT_LEN]> {
        self.with(|s| {
            s.sent
                .iter()
                .zip(&s.sent_to)
                .filter(|&(f, &to)| is_write(to, f))
                .map(|(f, _)| *f)
                .collect()
        })
    }

    fn script(&self, f: Script) {
        self.with(|s| s.script = Some(f));
    }
}

/// Every frame sent is allow-listed by the feature that
/// actually lives at its index, and none reaches a blocked feature.
fn assert_frames_allowed(sim: &Sim) {
    sim.with(|s| {
        for (f, &feature) in s.sent.iter().zip(&s.sent_to) {
            let ok = match (feature, f[3] >> 4) {
                (Some(0), 0) => matches!((f[4], f[5]), (0x83, 0x00) | (0x1f, 0x20)),
                (Some(0), 1) | (Some(0x8300), 0 | 1) | (Some(0x1F20), 0..=2) => true,
                _ => false,
            };
            assert!(
                ok,
                "frame {} reaches feature {feature:04x?}, not allow-listed",
                hex(&f[..8])
            );
            assert!(feature.is_none_or(|id| !BLOCKED_FEATURE_IDS.contains(&id)));
            assert_eq!(f[3] & 0x0f, 0x0b, "sw_id");
        }
    });
}

/// A Root.getFeature reply `11 ff 00 0b <index>`.
fn get_feature_reply(index: u8) -> Vec<u8> {
    let mut b = vec![0u8; REPORT_LEN];
    b[..5].copy_from_slice(&[0x11, 0xff, 0x00, 0x0b, index]);
    b
}

fn is_lookup(f: &[u8; REPORT_LEN], id: u16) -> bool {
    f[2] == 0 && f[3] == 0x0b && u16::from_be_bytes([f[4], f[5]]) == id
}

struct SimTransport(Sim);

impl Transport for SimTransport {
    fn send(&mut self, report: &Report) -> std::io::Result<()> {
        let f = *report.as_bytes();
        let reply = self.0.with(|s| {
            s.sent.push(f);
            let to = s.feature_at(f[2]);
            s.sent_to.push(to);
            if !s.present {
                return Err(std::io::Error::from_raw_os_error(19));
            }
            if s.fail_writes {
                return Err(std::io::Error::from_raw_os_error(32));
            }
            if !s.on {
                return Ok(None);
            }
            let mut r = vec![0u8; REPORT_LEN];
            r[..4].copy_from_slice(&f[..4]);
            match (s.feature_at(f[2]), f[3] >> 4) {
                (Some(0), 0) => r[4] = s.index_of(u16::from_be_bytes([f[4], f[5]])),
                (Some(0x8300), 0) => r[4] = s.sidetone,
                (Some(0x8300), 1) => {
                    s.sidetone = f[4];
                    r[4] = f[4];
                }
                (Some(0x1F20), 0) => {
                    r[4..6].copy_from_slice(&s.millivolts.to_be_bytes());
                    r[6] = s.flags;
                }
                (Some(0x1F20), 1) => r[4] = s.auto_off,
                (Some(0x1F20), 2) => {
                    s.auto_off = f[4];
                    r[4] = f[4];
                }
                _ => {
                    r = vec![0u8; REPORT_LEN];
                    r[..6].copy_from_slice(&[0x11, 0xff, 0xff, f[2], f[3], 0x07]);
                }
            }
            Ok(Some(r))
        })?;
        let mut script = self.0.with(|s| s.script.take());
        let out = match &mut script {
            Some(run) => run(&f, reply),
            None => reply.into_iter().collect(),
        };
        self.0.with(|s| {
            if s.script.is_none() {
                s.script = script;
            }
        });
        let write = self
            .0
            .with(|s| is_write(s.sent_to.last().copied().flatten(), &f));
        for r in out {
            self.0.push(Ok(r));
            // The real dongle follows a write's echo with a consumer-control
            // report (live test); it must not count as a mute press.
            if write {
                self.0.push(Ok(vec![1, 0, 0, 0, 0]));
            }
        }
        Ok(())
    }
}

fn cfg() -> Config {
    Config {
        power_model: Some(PowerModel::parse(crate::power::tests::SYNTH).unwrap()),
        sidetone: None,
        battery_poll: Duration::from_secs(3600),
        probe_interval: Duration::from_millis(300),
        audio: None,
    }
}

fn wait_for(svc: &HeadsetService, what: &str, f: impl Fn(&HeadsetStatus) -> bool) -> HeadsetStatus {
    wait_up_to(svc, what, Duration::from_secs(5), f)
}

fn wait_up_to(
    svc: &HeadsetService,
    what: &str,
    limit: Duration,
    f: impl Fn(&HeadsetStatus) -> bool,
) -> HeadsetStatus {
    let t = Instant::now();
    loop {
        let s = svc.status();
        if f(&s) {
            return s;
        }
        assert!(t.elapsed() < limit, "timeout waiting for {what}: {s:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn sim(on: bool) -> Sim {
    let sim = Sim::default();
    sim.with(|s| {
        s.present = true;
        s.on = on;
        s.millivolts = 3700;
        s.flags = 1;
        s.auto_off = 30;
    });
    sim
}

#[test]
fn reads_everything_and_writes_nothing_unasked() {
    let sim = sim(true);
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    let s = wait_for(&svc, "connect", |s| s.auto_off_minutes.is_some());
    assert!(s.dongle && s.connected);
    assert_eq!(s.sidetone, Some(0));
    assert_eq!(s.auto_off_minutes, Some(30));
    let b = s.battery.unwrap();
    assert_eq!(
        (b.millivolts, b.charging, b.percent),
        (3700, false, Some(50))
    );
    assert!(sim.writes().is_empty(), "no write without a request");
}

#[test]
fn writes_and_validation() {
    let sim = sim(true);
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    wait_for(&svc, "connect", |s| s.auto_off_minutes.is_some());
    let s = svc
        .set_sidetone(SidetoneLevel::try_from(40).unwrap())
        .unwrap();
    assert_eq!(s.sidetone, Some(40));
    let s = svc
        .set_auto_off(AutoOffMinutes::try_from(10).unwrap())
        .unwrap();
    assert_eq!(s.auto_off_minutes, Some(10));
    let w = sim.writes();
    assert_eq!(w.len(), 2);
    assert_eq!(w[0][..5], [0x11, 0xff, 0x05, 0x1b, 40]);
    assert_eq!(w[1][..5], [0x11, 0xff, 0x06, 0x2b, 10]);
    std::thread::sleep(crate::mute::APPLY_DELAY + Duration::from_millis(200));
    assert_eq!(
        svc.status().mic_muted,
        None,
        "write echo is not a mute press"
    );
    assert_frames_allowed(&sim);
}

#[test]
fn headset_power_cycle_and_sidetone_reapply() {
    let sim = sim(false);
    let mut c = cfg();
    c.sidetone = Some(SidetoneLevel::try_from(25).unwrap());
    let svc = HeadsetService::spawn_with(c, sim.opener()).unwrap();
    let s = wait_for(&svc, "dongle", |s| s.dongle);
    assert!(!s.connected);
    // Writes are refused while the headset is off.
    let e = svc
        .set_sidetone(SidetoneLevel::try_from(10).unwrap())
        .unwrap_err();
    assert!(e.contains("off"), "{e}");
    // Power on: the dongle sends an ADC event with the voltage.
    sim.with(|s| s.on = true);
    sim.event(3700, 1);
    let s = wait_for(&svc, "power on", |s| s.connected && s.sidetone == Some(25));
    assert_eq!(s.battery.unwrap().millivolts, 3700);
    assert_eq!(sim.writes().len(), 1, "sidetone re-applied once");
    // Power off: ADC event with 0 mV.
    sim.with(|s| s.on = false);
    sim.event(0, 0);
    let s = wait_for(&svc, "power off", |s| !s.connected);
    assert!(s.dongle && s.battery.is_none());
    // On again: the device lost the sidetone (as G HUB assumes), re-applied.
    sim.with(|s| {
        s.on = true;
        s.sidetone = 0;
    });
    sim.event(3690, 1);
    wait_for(&svc, "power on again", |s| {
        s.connected && s.sidetone == Some(25)
    });
    assert_eq!(sim.writes().len(), 2);
}

#[test]
fn headset_found_by_probe_without_event() {
    let sim = sim(false);
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    wait_for(&svc, "dongle", |s| s.dongle && !s.connected);
    sim.with(|s| s.on = true);
    wait_for(&svc, "probe", |s| s.connected);
}

#[test]
fn dongle_replug() {
    let sim = sim(true);
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    wait_for(&svc, "connect", |s| s.connected);
    sim.unplug();
    let s = wait_for(&svc, "unplug", |s| !s.dongle);
    assert_eq!(s, HeadsetStatus::default());
    let e = svc
        .set_auto_off(AutoOffMinutes::try_from(5).unwrap())
        .unwrap_err();
    assert!(e.contains("not found"), "{e}");
    sim.with(|s| s.present = true);
    wait_for(&svc, "replug", |s| {
        s.connected && s.auto_off_minutes == Some(30)
    });
    assert_eq!(sim.with(|s| s.opens), 2);
}

#[test]
fn charging_and_battery_events() {
    let sim = sim(true);
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    wait_for(&svc, "connect", |s| s.connected && s.battery.is_some());
    sim.event(4000, 3);
    let s = wait_for(&svc, "charging", |s| {
        s.battery.as_ref().is_some_and(|b| b.charging)
    });
    let b = s.battery.unwrap();
    assert_eq!((b.percent, b.hours_left), (Some(50), None));
}

#[test]
fn open_error_is_reported() {
    let opener: Opener = Box::new(|_| {
        Some(Err(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )))
    });
    let svc = HeadsetService::spawn_with(cfg(), opener).unwrap();
    let s = wait_for(&svc, "error", |s| s.error.is_some());
    assert!(!s.dongle);
    assert!(s.error.unwrap().contains("cannot open"));
}

#[test]
fn resume_detection() {
    let (m, w) = (Instant::now(), SystemTime::now());
    let s = Duration::from_secs;
    // Normal tick: both clocks advance together.
    assert_eq!(slept((m, w), (m + s(1), w + s(1))), None);
    // NTP step back: not a resume.
    assert_eq!(slept((m, w + s(60)), (m + s(1), w)), None);
    // Suspended for 10 minutes: the monotonic clock advanced 1 s.
    assert_eq!(slept((m, w), (m + s(1), w + s(601))), Some(s(600)));
}

#[test]
fn mute_presses_reach_the_status() {
    use crate::mute::{APPLY_DELAY, AudioObservation};
    let sim = sim(true);
    let obs = Arc::new(Mutex::new(AudioObservation::default()));
    let mut c = cfg();
    let o = obs.clone();
    c.audio = Some(Box::new(move || *o.lock().unwrap()));
    let svc = HeadsetService::spawn_with(c, sim.opener()).unwrap();
    wait_for(&svc, "connect", |s| s.auto_off_minutes.is_some());
    assert_eq!(svc.status().mic_muted, None, "unknown until audio");
    obs.lock().unwrap().last_audio = Some(Instant::now());
    wait_for(&svc, "live", |s| s.mic_muted == Some(false));
    // Past the post-connect quiet window, one press (two reports) mutes.
    std::thread::sleep(crate::mute::AFTER_POWER);
    sim.push(Ok(vec![1, 0, 0, 0, 0]));
    sim.push(Ok(vec![1, 0, 0, 0, 0]));
    std::thread::sleep(APPLY_DELAY / 2);
    assert_eq!(svc.status().mic_muted, Some(false), "not applied yet");
    wait_for(&svc, "muted", |s| s.mic_muted == Some(true));
    // The write echo report is not a press.
    svc.set_sidetone(SidetoneLevel::try_from(5).unwrap())
        .unwrap();
    std::thread::sleep(APPLY_DELAY + Duration::from_millis(200));
    assert_eq!(svc.status().mic_muted, Some(true));
    // Power-off hides it, power-on shows the kept state again.
    sim.with(|s| s.on = false);
    sim.event(0, 0);
    wait_for(&svc, "off", |s| !s.connected && s.mic_muted.is_none());
    sim.with(|s| s.on = true);
    sim.event(3700, 1);
    wait_for(&svc, "on, still muted", |s| {
        s.connected && s.mic_muted == Some(true)
    });
}

// ------------------------------------------------ reply attribution

/// First case: one ADC lookup times out and its reply
/// arrives during the next feature lookup, just before the real one. The
/// old worker swapped the indices and turned "read auto-off" into
/// SetSidetone(0). Nothing may reach the wrong feature, nothing may be
/// written unasked, and the headset must still connect in the end.
#[test]
fn late_lookup_reply_is_not_taken_for_another() {
    let sim = sim(true);
    let mut held: Option<Vec<u8>> = None;
    let mut adc_lookups = 0;
    sim.script(Box::new(move |f, reply| {
        if is_lookup(f, 0x1F20) {
            adc_lookups += 1;
            if adc_lookups == 1 {
                held = reply;
                return vec![];
            }
        }
        if is_lookup(f, 0x8300)
            && let Some(late) = held.take()
        {
            return std::iter::once(late).chain(reply).collect();
        }
        reply.into_iter().collect()
    }));
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    let s = wait_up_to(&svc, "connect", Duration::from_secs(10), |s| {
        s.auto_off_minutes.is_some()
    });
    assert_eq!((s.sidetone, s.auto_off_minutes), (Some(0), Some(30)));
    svc.set_auto_off(AutoOffMinutes::try_from(10).unwrap())
        .unwrap();
    assert_frames_allowed(&sim);
    let w = sim.writes();
    assert_eq!(w.len(), 1, "only the requested write: {w:02x?}");
    assert_eq!(w[0][..5], [0x11, 0xff, 0x06, 0x2b, 10]);
    assert_eq!(sim.with(|s| (s.sidetone, s.auto_off)), (0, 10));
}

/// Second case: another HID++ client with the same sw_id
/// gets a getFeature reply (index 0x0c, DFU here) just before ours. No frame
/// may go to 0x0c.
#[test]
fn foreign_lookup_reply_is_detected() {
    let sim = sim(true);
    let mut injected = false;
    sim.script(Box::new(move |f, reply| {
        if is_lookup(f, 0x8300) && !injected {
            injected = true;
            return std::iter::once(get_feature_reply(0x0c))
                .chain(reply)
                .collect();
        }
        reply.into_iter().collect()
    }));
    let mut c = cfg();
    c.sidetone = Some(SidetoneLevel::try_from(25).unwrap());
    let svc = HeadsetService::spawn_with(c, sim.opener()).unwrap();
    wait_for(&svc, "connect", |s| {
        s.sidetone == Some(25) && s.auto_off_minutes.is_some()
    });
    svc.set_sidetone(SidetoneLevel::try_from(40).unwrap())
        .unwrap();
    assert_frames_allowed(&sim);
    assert!(sim.with(|s| s.sent.iter().all(|f| f[2] != 0x0c)));
    assert_eq!(sim.writes().len(), 2, "re-apply + the requested write");
}

/// A Root reply with gprox's sw_id that no request explains, while the
/// headset is connected: the map can no longer be trusted, so writes are
/// refused until the next verified lookup.
#[test]
fn unexplained_reply_while_connected_blocks_writes() {
    let sim = sim(true);
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    wait_for(&svc, "connect", |s| s.auto_off_minutes.is_some());
    sim.push(Ok(get_feature_reply(0x0c)));
    wait_for(&svc, "map dropped", |s| !s.connected);
    // The probe looks the features up again and finds the right map.
    wait_for(&svc, "reconnect", |s| {
        s.connected && s.auto_off_minutes.is_some()
    });
    svc.set_auto_off(AutoOffMinutes::try_from(5).unwrap())
        .unwrap();
    assert_frames_allowed(&sim);
    assert_eq!(sim.writes().len(), 1);
}

/// A feature table that differs from the last verified one is looked up
/// once more before anything is sent to it.
#[test]
fn changed_feature_table_is_checked_again() {
    let sim = sim(true);
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    wait_for(&svc, "connect", |s| s.auto_off_minutes.is_some());
    sim.with(|s| s.on = false);
    sim.event(0, 0);
    wait_for(&svc, "off", |s| !s.connected);
    let mark = sim.with(|s| {
        s.table = vec![(7, 0x8300), (8, 0x1F20), (0x0c, 0x00D0)];
        s.on = true;
        s.sent.len()
    });
    sim.event(3700, 1);
    wait_for(&svc, "on with the new table", |s| {
        s.connected && s.auto_off_minutes.is_some()
    });
    assert_frames_allowed(&sim);
    let after: Vec<_> = sim.with(|s| s.sent[mark..].to_vec());
    let first_feature_frame = after.iter().position(|f| f[2] != 0).unwrap();
    let lookups = after[..first_feature_frame]
        .iter()
        .filter(|f| f[2] == 0 && f[3] == 0x0b)
        .count();
    assert_eq!(lookups, 4, "two lookup rounds before the first use");
}

/// Writes to the node fail while it stays present. The old
/// worker closed and reopened the dongle every second, leaking the blocked
/// reader thread and its file descriptor each time. Now the node stays open,
/// the error is shown, and the probe retries.
#[test]
fn write_errors_do_not_reopen_the_dongle() {
    let sim = sim(true);
    sim.with(|s| s.fail_writes = true);
    let svc = HeadsetService::spawn_with(cfg(), sim.opener()).unwrap();
    let s = wait_for(&svc, "error", |s| s.error.is_some());
    assert!(s.dongle && !s.connected, "{s:?}");
    assert!(s.error.unwrap().contains("write"));
    std::thread::sleep(Duration::from_millis(3000));
    assert_eq!(sim.with(|s| s.opens), 1, "the dongle was reopened");
    // Retries are paced by the probe, not a tight loop.
    let tries = sim.with(|s| s.sent.len());
    assert!(tries <= 3000 / 300 + 2, "{tries} frames in 3 s");
    sim.with(|s| s.fail_writes = false);
    wait_for(&svc, "recovered", |s| {
        s.connected && s.auto_off_minutes.is_some() && s.error.is_none()
    });
    assert_eq!(sim.with(|s| s.opens), 1);
}
