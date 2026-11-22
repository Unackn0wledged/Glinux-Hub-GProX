//! The dongle's hidraw node: discovery via sysfs and plain read/write.
//!
//! No ioctls are needed: on hidraw, `write()` of `[report id, data…]` sends an
//! output report (the kernel turns it into the control-pipe SET_REPORT G HUB
//! uses, since interface 3 has no interrupt OUT endpoint) and `read()` returns
//! one input report with its report id. So this module is safe std code.
//!
//! Only interface 3 of 046d:0aba is ever opened. Every other hidraw node,
//! including the mouse/keyboard receiver 046d:c547, is rejected by
//! [`is_dongle`] before anything is opened.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::hidpp::Report;

/// `HID_ID` of the dongle in sysfs `uevent`: bus USB, 046d:0aba.
const DONGLE_HID_ID: &str = "0003:0000046D:00000ABA";
/// The HID++ interface.
const DONGLE_INTERFACE: &str = "/input3";

/// Whether a hidraw node's sysfs `device/uevent` text is the dongle's HID++
/// interface.
pub fn is_dongle(uevent: &str) -> bool {
    let field = |k: &str| {
        uevent
            .lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
            .map(str::trim)
    };
    field("HID_ID").is_some_and(|id| id.eq_ignore_ascii_case(DONGLE_HID_ID))
        && field("HID_PHYS").is_some_and(|p| p.ends_with(DONGLE_INTERFACE))
}

/// The dongle's `/dev/hidrawN`, if present. `sys_class` is
/// `/sys/class/hidraw` (a parameter for tests).
pub fn find_dongle_in(sys_class: &Path, dev: &Path) -> Option<PathBuf> {
    let mut found: Vec<String> = std::fs::read_dir(sys_class)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let uevent = std::fs::read_to_string(e.path().join("device/uevent")).ok()?;
            is_dongle(&uevent).then_some(name)
        })
        .collect();
    found.sort();
    found.first().map(|n| dev.join(n))
}

/// The dongle's `/dev/hidrawN`, if present.
pub fn find_dongle() -> Option<PathBuf> {
    find_dongle_in(Path::new("/sys/class/hidraw"), Path::new("/dev"))
}

/// Something that sends HID++ reports. The only accepted input is a
/// [`Report`], which only the allow-listed commands can produce.
pub trait Transport: Send {
    fn send(&mut self, report: &Report) -> std::io::Result<()>;
}

/// An input report or the end of the stream (the device went away).
pub type Incoming = std::io::Result<Vec<u8>>;

/// An open hidraw node.
#[derive(Debug)]
pub struct Hidraw {
    file: File,
    path: PathBuf,
}

impl Transport for Hidraw {
    fn send(&mut self, report: &Report) -> std::io::Result<()> {
        tracing::trace!(path = %self.path.display(), ?report, "hid write");
        self.file.write_all(report.as_bytes())
    }
}

impl Hidraw {
    /// Open the dongle at `path` (checked against sysfs again) and start a
    /// reader thread that passes every input report to `sink` (which returns
    /// false to stop). The thread
    /// ends with an `Err` (or `Ok(empty)`) when the device goes away; it
    /// blocks in `read()` until then, so a node is only opened once per
    /// appearance.
    pub fn open(
        path: &Path,
        mut sink: Box<dyn FnMut(Incoming) -> bool + Send>,
    ) -> std::io::Result<Hidraw> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| std::io::Error::other("bad hidraw path"))?;
        let uevent = std::fs::read_to_string(
            Path::new("/sys/class/hidraw")
                .join(name)
                .join("device/uevent"),
        )?;
        if !is_dongle(&uevent) {
            return Err(std::io::Error::other(format!(
                "{} is not the PRO X Wireless dongle's HID++ interface",
                path.display()
            )));
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut reader = file.try_clone()?;
        std::thread::Builder::new()
            .name("gprox-hid-read".into())
            .spawn(move || {
                let mut buf = [0u8; 64];
                loop {
                    match reader.read(&mut buf) {
                        Ok(n) => {
                            let end = n == 0;
                            if !sink(Ok(buf[..n].to_vec())) || end {
                                return;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            sink(Err(e));
                            return;
                        }
                    }
                }
            })?;
        Ok(Hidraw {
            file,
            path: path.to_owned(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Copied from this host's sysfs (2026-11-21).
    const DONGLE: &str = "DRIVER=hid-generic\nHID_ID=0003:0000046D:00000ABA\n\
        HID_NAME=Logitech PRO X Wireless Gaming Headset\nHID_PHYS=usb-0000:2d:00.3-1.3/input3\n\
        HID_UNIQ=\nMODALIAS=hid:b0003g0001v0000046Dp00000ABA\n";
    const RECEIVER: &str = "DRIVER=logitech-djreceiver\nHID_ID=0003:0000046D:0000C547\n\
        HID_NAME=Logitech USB Receiver\nHID_PHYS=usb-0000:2d:00.3-3/input2\n";
    const RECEIVER_CHILD: &str = "DRIVER=logitech-hidpp-device\nHID_ID=0003:0000046D:00004093\n\
        HID_NAME=Logitech PRO X\nHID_PHYS=usb-0000:2d:00.3-3/input2:1\n";

    #[test]
    fn only_the_dongle_matches() {
        assert!(is_dongle(DONGLE));
        assert!(!is_dongle(RECEIVER));
        assert!(!is_dongle(RECEIVER_CHILD));
        assert!(!is_dongle(&DONGLE.replace("/input3", "/input0")));
        assert!(!is_dongle(&DONGLE.replace("0ABA", "0ABB")));
        assert!(!is_dongle(""));
    }

    #[test]
    fn find_in_fake_sysfs() {
        let d = tempfile::tempdir().unwrap();
        for (n, ue) in [
            ("hidraw0", RECEIVER),
            ("hidraw2", RECEIVER_CHILD),
            ("hidraw4", DONGLE),
        ] {
            let p = d.path().join(n).join("device");
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("uevent"), ue).unwrap();
        }
        assert_eq!(
            find_dongle_in(d.path(), Path::new("/dev")),
            Some(PathBuf::from("/dev/hidraw4"))
        );
        std::fs::remove_dir_all(d.path().join("hidraw4")).unwrap();
        assert_eq!(find_dongle_in(d.path(), Path::new("/dev")), None);
    }
}
