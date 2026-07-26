//! # hci-transport
//!
//! The platform half of the Bluetooth stack: [`HciTransport`] backends, and the
//! [`ControllerInit`] registry that wakes a cold radio.
//!
//! Split from `substrate-hci` because ground rule 8 has every `substrate-*` crate at
//! `unsafe_code = "forbid"` and the Linux raw-HCI socket needs syscalls. The USB backend
//! needs no `unsafe` at all — `nusb` is a safe API — which also makes both firmware
//! loaders pure safe Rust (architecture §11.2).
//!
//! Two seams, because they are two problems. Moving packets is vendor-neutral; waking a
//! controller is not, and most modern parts ship with no usable ROM image.
#![cfg_attr(not(feature = "socket"), forbid(unsafe_code))]
#![cfg_attr(feature = "socket", deny(unsafe_op_in_unsafe_fn))]

pub mod error;
pub mod firmware;
pub mod init;

#[cfg(all(feature = "socket", target_os = "linux"))]
pub mod socket;
#[cfg(feature = "usb")]
pub mod usb;

/// The firmware table `build.rs` generated.
mod embedded {
    include!(concat!(env!("OUT_DIR"), "/firmware.rs"));
}

pub use error::TransportError;
pub use firmware::{Firmware, FirmwareSet};
pub use init::{ControllerInit, IntelInit, NoInit, RealtekInit, UsbId};

/// Every firmware image compiled into this build.
#[must_use]
pub fn embedded_firmware_names() -> Vec<&'static str> {
    embedded::IMAGES.iter().map(|(name, _)| *name).collect()
}
