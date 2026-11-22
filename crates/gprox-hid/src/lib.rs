//! HID++ for the PRO X Wireless (dongle 046d:0aba): sidetone, auto-off timer,
//! battery, headset connect/disconnect.
//!
//! - [`hidpp`]: frames and the command allow-list. Only the commands G HUB was
//!   seen sending can be encoded; DFU/firmware
//!   features have no representation at all.
//! - [`hidraw`]: finds and opens the dongle's HID++ interface (interface 3
//!   only; every other hidraw node is rejected).
//! - [`mute`]: mic mute state from the dongle's consumer-control reports.
//! - [`power`]: battery voltage → charge from G HUB's power model.
//! - [`service`]: the background worker gproxd runs (hotplug, power events,
//!   resume from sleep, battery polling, the two writes).
//!
//! No `unsafe`: hidraw needs nothing beyond `read()`/`write()` on the node.
//!
//! Frames can't be built from raw bytes outside this crate:
//!
//! ```compile_fail
//! let r = gprox_hid::hidpp::Report([0x11; 20]);
//! ```
//!
//! and parameters are range-checked:
//!
//! ```
//! use gprox_hid::hidpp::{AutoOffMinutes, SidetoneLevel};
//! assert!(SidetoneLevel::try_from(101).is_err());
//! assert!(AutoOffMinutes::try_from(7).is_err());
//! ```

pub mod hidpp;
pub mod hidraw;
pub mod mute;
pub mod power;
pub mod service;

pub use hidpp::{AutoOffMinutes, SidetoneLevel};
pub use power::PowerModel;
pub use service::{Config, HeadsetService};
