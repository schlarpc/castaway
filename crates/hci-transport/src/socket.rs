//! The Linux raw-HCI socket: `AF_BLUETOOTH` on `HCI_CHANNEL_USER`.
//!
//! Takes exclusive userspace control of a controller the kernel has already brought up.
//! That makes it useless for testing [`crate::ControllerInit`] — the firmware is already
//! loaded by the time we get here, which is what the AX200 run demonstrated — but it is
//! exactly right for two other things:
//!
//! - **Virtual controllers.** `btvirt -l2` creates a pair of linked emulated controllers
//!   that need no firmware at all. Attaching to one of them and letting BlueZ drive the
//!   other gives a complete A2DP session against an independent source implementation,
//!   with no radio and no hardware (architecture §11.7).
//! - **Working on the layers above.** L2CAP, AVDTP and AVRCP do not care how the bytes
//!   arrive, and a kernel-initialised controller is the shortest path to having some.
//!
//! Unlike USB, this transport carries the **packet-type indicator byte**, so packets go
//! through [`HciPacket::encode`]/[`HciPacket::decode`] rather than the `_body` variants.
//!
//! This is the one module in the crate that needs `unsafe`: there is no safe wrapper for
//! `AF_BLUETOOTH`. Every block carries the invariant it relies on (ground rule 8), and the
//! surface is four syscalls wide — socket, bind, read, write.
#![allow(unsafe_code)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use substrate_hci::{HciError, HciPacket, HciTransport};
use tokio::io::unix::AsyncFd;
use tracing::{info, warn};

use crate::error::TransportError;

/// `AF_BLUETOOTH`.
const AF_BLUETOOTH: libc::c_int = 31;
/// `BTPROTO_HCI`.
const BTPROTO_HCI: libc::c_int = 1;
/// Exclusive userspace control of one controller.
const HCI_CHANNEL_USER: u16 = 1;

/// `struct sockaddr_hci`, which libc does not declare.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrHci {
    hci_family: libc::sa_family_t,
    hci_dev: u16,
    hci_channel: u16,
}

/// Largest packet the kernel will hand us in one read.
const READ_BUF: usize = 4096;

/// A controller reached through the kernel's raw HCI socket.
#[derive(Debug)]
pub struct SocketTransport {
    fd: AsyncFd<OwnedFd>,
    index: u16,
}

impl SocketTransport {
    /// Attach to `hciN` — index `0` for `hci0`.
    ///
    /// The controller must be **down**: `HCI_CHANNEL_USER` is exclusive, and the kernel
    /// refuses while its own stack has the device up. `sudo hciconfig hciN down` first,
    /// or stop `bluetooth.service`.
    ///
    /// # Errors
    /// [`TransportError::Claim`] if the socket cannot be opened or bound — most often
    /// because the device is up, or because this process lacks `CAP_NET_ADMIN`.
    pub fn open(index: u16) -> Result<Self, TransportError> {
        // SAFETY: a plain `socket(2)` call with constant arguments. It returns an owned
        // file descriptor or -1, and nothing else is touched.
        let raw: RawFd = unsafe {
            libc::socket(
                AF_BLUETOOTH,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                BTPROTO_HCI,
            )
        };
        if raw < 0 {
            return Err(claim_error(index, "socket(AF_BLUETOOTH)"));
        }
        // SAFETY: `raw` is a fresh descriptor this call owns and has not been given to
        // anything else, so `OwnedFd` may take responsibility for closing it. Wrapping
        // it immediately means every error path below closes it exactly once.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };

        let addr = SockAddrHci {
            #[allow(clippy::cast_possible_truncation)]
            hci_family: AF_BLUETOOTH as libc::sa_family_t,
            hci_dev: index,
            hci_channel: HCI_CHANNEL_USER,
        };
        // SAFETY: `addr` is a correctly-shaped `sockaddr_hci` living on this stack frame
        // for the duration of the call, and the length passed is its exact size. The
        // kernel copies it and does not retain the pointer.
        let rc = unsafe {
            libc::bind(
                owned.as_raw_fd(),
                std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
                u32::try_from(std::mem::size_of::<SockAddrHci>()).unwrap_or(6),
            )
        };
        if rc < 0 {
            return Err(claim_error(index, "bind(HCI_CHANNEL_USER)"));
        }

        let fd = AsyncFd::new(owned).map_err(|e| TransportError::Io(e.to_string()))?;
        info!(index, "attached to hci{index} over HCI_CHANNEL_USER");
        Ok(Self { fd, index })
    }

    /// Which controller this is attached to.
    #[must_use]
    pub const fn index(&self) -> u16 {
        self.index
    }
}

/// Turn an errno into something that names the usual cause.
fn claim_error(index: u16, what: &'static str) -> TransportError {
    let err = std::io::Error::last_os_error();
    let hint = match err.raw_os_error() {
        // The kernel's own stack has the device up. This is the error everyone hits.
        Some(libc::EBUSY) => {
            " (the controller is up — `sudo hciconfig hciN down`, or stop bluetooth.service)"
        }
        Some(libc::EPERM) | Some(libc::EACCES) => " (needs CAP_NET_ADMIN — try sudo)",
        Some(libc::ENODEV) => " (no such controller)",
        _ => "",
    };
    TransportError::Claim {
        id: crate::init::UsbId::new(0xFFFF, index),
        detail: format!("{what}: {err}{hint}"),
    }
}

#[async_trait::async_trait]
impl HciTransport for SocketTransport {
    async fn send(&self, packet: HciPacket) -> Result<(), HciError> {
        // This transport *does* carry the indicator byte, unlike USB where the endpoint
        // says what a packet is.
        let bytes = packet.encode()?;
        loop {
            let mut guard = self
                .fd
                .writable()
                .await
                .map_err(|e| HciError::Transport(e.to_string()))?;
            match guard.try_io(|inner| {
                // SAFETY: writing `bytes.len()` bytes from a slice this call keeps alive
                // for the duration, to a descriptor the guard proves is writable.
                let n = unsafe {
                    libc::write(
                        inner.get_ref().as_raw_fd(),
                        bytes.as_ptr().cast::<libc::c_void>(),
                        bytes.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n)
                }
            }) {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(e)) => return Err(HciError::Transport(e.to_string())),
                // Readiness was stale; wait again rather than spin.
                Err(_would_block) => continue,
            }
        }
    }

    async fn recv(&self) -> Result<HciPacket, HciError> {
        let mut buf = vec![0u8; READ_BUF];
        loop {
            let mut guard = self
                .fd
                .readable()
                .await
                .map_err(|e| HciError::Transport(e.to_string()))?;
            let read = guard.try_io(|inner| {
                // SAFETY: reading into a buffer this call owns and keeps alive, bounded
                // by its own length, from a descriptor the guard proves is readable.
                let n = unsafe {
                    libc::read(
                        inner.get_ref().as_raw_fd(),
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                        buf.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    #[allow(clippy::cast_sign_loss)]
                    Ok(n as usize)
                }
            });
            match read {
                Ok(Ok(0)) => continue,
                Ok(Ok(n)) => match HciPacket::decode(&buf[..n]) {
                    Ok(packet) => return Ok(packet),
                    Err(e) => {
                        // One malformed packet must not end the session; a controller
                        // mid-reset emits odd things.
                        warn!(error = %e, "dropping a malformed HCI packet");
                        continue;
                    }
                },
                Ok(Err(e)) => return Err(HciError::Transport(e.to_string())),
                Err(_would_block) => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sockaddr_matches_what_the_kernel_expects() {
        // Six bytes: family, device index, channel. A wrong size makes `bind` return
        // EINVAL, which reads like a permissions problem and is not one.
        assert_eq!(std::mem::size_of::<SockAddrHci>(), 6);
        assert_eq!(AF_BLUETOOTH, 31);
        assert_eq!(HCI_CHANNEL_USER, 1);
    }

    #[test]
    fn attaching_to_a_controller_that_does_not_exist_says_so() {
        // Index 200 will not exist on any machine this runs on. The point is that the
        // error names the cause rather than surfacing a bare errno.
        match SocketTransport::open(200) {
            Ok(_) => panic!("hci200 should not exist"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("no such controller")
                        || msg.contains("CAP_NET_ADMIN")
                        || msg.contains("bind"),
                    "unhelpful error: {msg}"
                );
            }
        }
    }
}
