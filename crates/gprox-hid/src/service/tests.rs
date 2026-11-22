//! The worker against a simulated dongle that answers like the real one
//! did in USB captures (indices 5/6, replies echo the first 4 bytes).

use super::*;
use crate::hidpp::{REPORT_LEN, Report};

type Sink = Box<dyn FnMut(Incoming) -> bool + Send>;

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
    /// How many times the dongle was opened.
    opens: usize,
    sink: Option<Sink>,
}

#[derive(Clone, Default)]
struct Sim(Arc<Mutex<SimState>>);

impl Sim {
    fn with<R>(&self, f: impl FnOnce(&mut SimState) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }

    fn push(&self, data: Incoming) {
        let mut sink = self.with(|s| s.sink.take());
        if let Some(k) = &mut sink {
            k(data);
        }
        self.with(|s| {
            if s.sink.is_none() {
                s.sink = sink;
            }
        });
    }

    fn event(&self, mv: u16, flags: u8) {
        let mut b = vec![0u8; REPORT_LEN];
        b[..4].copy_from_slice(&[0x11, 0xff, 0x06, 0x00]);
        b[4..6].copy_from_slice(&mv.to_be_bytes());
        b[6] = flags;
        self.push(Ok(b));
    }

    fn unplug(&self) {
        self.with(|s| s.present = false);
        self.push(Err(std::io::Error::from_raw_os_error(19)));
        self.with(|s| s.sink = None);
    }

    fn opener(&self) -> Opener {
        let sim = self.clone();
        Box::new(move |sink| {
            if !sim.with(|s| s.present) {
                return None;
            }
            sim.with(|s| {
                s.opens += 1;
                s.sink = Some(sink);
            });
            Some(Ok(Box::new(SimTransport(sim.clone())) as Box<dyn Transport>))
        })
    }

    fn writes(&self) -> Vec<[u8; REPORT_LEN]> {
        self.with(|s| {
            s.sent
                .iter()
                .filter(|f| matches!((f[2], f[3] >> 4), (5, 1) | (6, 2)))
                .copied()
                .collect()
        })
    }
}

struct SimTransport(Sim);

impl Transport for SimTransport {
    fn send(&mut self, report: &Report) -> std::io::Result<()> {
        let f = *report.as_bytes();
        let reply = self.0.with(|s| {
            s.sent.push(f);
            if !s.present {
                return Err(std::io::Error::from_raw_os_error(19));
            }
            if !s.on {
                return Ok(None);
            }
            let mut r = vec![0u8; REPORT_LEN];
            r[..4].copy_from_slice(&f[..4]);
            match (f[2], f[3] >> 4) {
                (0, 0) => {
                    r[4] = match (f[4], f[5]) {
                        (0x83, 0x00) => 5,
                        (0x1f, 0x20) => 6,
                        _ => 0,
                    }
                }
                (5, 0) => r[4] = s.sidetone,
                (5, 1) => {
                    s.sidetone = f[4];
                    r[4] = f[4];
                }
                (6, 0) => {
                    r[4..6].copy_from_slice(&s.millivolts.to_be_bytes());
                    r[6] = s.flags;
                }
                (6, 1) => r[4] = s.auto_off,
                (6, 2) => {
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
        if let Some(r) = reply {
            self.0.push(Ok(r));
            // The real dongle follows a write's echo with a consumer-control
            // report (live test); it must not count as a mute press.
            if matches!((f[2], f[3] >> 4), (5, 1) | (6, 2)) {
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
    let t = Instant::now();
    loop {
        let s = svc.status();
        if f(&s) {
            return s;
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "timeout waiting for {what}: {s:?}"
        );
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
    // Every frame the worker sent is one of the allow-listed shapes.
    for f in sim.with(|s| s.sent.clone()) {
        assert!(
            matches!(
                (f[2], f[3]),
                (0, 0x0b) | (0, 0x1b) | (5, 0x0b) | (5, 0x1b) | (6, 0x0b) | (6, 0x1b) | (6, 0x2b)
            ),
            "{f:02x?}"
        );
    }
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
