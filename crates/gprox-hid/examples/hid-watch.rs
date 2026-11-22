//! Read-only live check: start the headset service on the real dongle and
//! print its status whenever it changes. Sends only get requests (no sidetone
//! re-apply, no writes).
//!
//! `cargo run -p gprox-hid --example hid-watch -- [SECONDS] [POWER_MODEL.xml]`
//! (`RUST_LOG=gprox_hid=trace` shows every frame.)

use std::time::{Duration, Instant};

use gprox_hid::{Config, HeadsetService, PowerModel};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gprox_hid=debug".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let power_model = args
        .next()
        .map(|p| PowerModel::load(p.as_ref()).expect("power model"));
    let svc = HeadsetService::spawn(Config {
        power_model,
        battery_poll: Duration::from_secs(10),
        ..Config::default()
    })
    .expect("spawn");
    let t0 = Instant::now();
    let mut last = None;
    while t0.elapsed() < Duration::from_secs(secs) {
        let s = svc.status();
        if last.as_ref() != Some(&s) {
            println!(
                "{:7.3} {}",
                t0.elapsed().as_secs_f64(),
                serde_json::to_string(&s).unwrap()
            );
            last = Some(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
