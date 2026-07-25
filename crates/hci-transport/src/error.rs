//! Typed transport and controller-initialisation failures (ground rule 7).

use thiserror::Error;

use crate::init::UsbId;

/// Failures opening a controller, loading its firmware, or moving packets.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransportError {
    /// No controller matched the selector.
    #[error("no bluetooth controller found{}", match .0 { Some(id) => format!(" matching {id}"), None => String::new() })]
    NoDevice(Option<UsbId>),

    /// The device is present but does not expose a Bluetooth HCI interface.
    #[error("{0} is not an HCI-class device (expected class E0/01/01)")]
    NotHci(UsbId),

    /// The OS refused access — almost always the kernel driver still holding it, or
    /// missing privileges.
    #[error("cannot claim {id}: {detail}")]
    Claim {
        /// Which device.
        id: UsbId,
        /// What the OS said.
        detail: String,
    },

    /// A USB or socket operation failed.
    #[error("io: {0}")]
    Io(String),

    /// The controller answered a command with a failure status, or not at all.
    #[error("controller rejected {what}: {detail}")]
    Controller {
        /// Which step.
        what: &'static str,
        /// What came back.
        detail: String,
    },

    /// A firmware image was missing, or not the shape its loader expects.
    #[error("firmware {name}: {detail}")]
    Firmware {
        /// Which image.
        name: String,
        /// What was wrong.
        detail: String,
    },

    /// No initialiser in the registry handles this controller.
    #[error("no firmware loader for {0}; add one to the ControllerInit registry")]
    UnsupportedController(UsbId),

    /// A step took too long. Firmware upload has no other failure mode on a wedged part.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),

    /// The underlying HCI layer failed to encode or decode.
    #[error(transparent)]
    Hci(#[from] substrate_hci::HciError),
}
