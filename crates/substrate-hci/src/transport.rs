//! The platform seam.
//!
//! [`HciTransport`] is the *entire* OS-specific surface of the Bluetooth stack: a byte
//! pipe that moves framed HCI packets. Linux reaches it with an `AF_BLUETOOTH` socket on
//! `HCI_CHANNEL_USER`; Windows reaches it with `nusb` against a dongle bound to WinUSB,
//! because Winsock has no L2CAP and the inbox A2DP sink is SBC-only with no stream access
//! (architecture-substrate.md §11.1). Everything above this trait — L2CAP, SDP, AVDTP,
//! AVRCP — is portable and is tested once (ground rule 5).

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::error::HciError;
use crate::packet::HciPacket;

/// A byte pipe to a Bluetooth controller, framed as HCI packets.
///
/// `&self` rather than `&mut self` so one transport can be shared between the command
/// path and the ACL path; implementations serialise internally.
#[async_trait::async_trait]
pub trait HciTransport: Send + Sync {
    /// Send one packet to the controller.
    ///
    /// # Errors
    /// [`HciError::Transport`] if the device is gone or the write failed.
    async fn send(&self, packet: HciPacket) -> Result<(), HciError>;

    /// Receive the next packet from the controller, waiting if none is ready.
    ///
    /// # Errors
    /// [`HciError::Transport`] if the device is gone, or a decode error if the
    /// controller emitted something malformed.
    async fn recv(&self) -> Result<HciPacket, HciError>;

    /// Tell the transport that the controller is running its bootloader, or has left it.
    ///
    /// This exists for one quirk, and it is a *framing* quirk rather than a protocol one,
    /// which is why it lives here and not in the loader. While an Intel controller runs
    /// its bootloader, HCI does not stay on the pipes the USB transport spec assigns it:
    /// `Secure_Send` goes out on **bulk OUT**, and events come back on **bulk IN** as well
    /// as on the interrupt endpoint — with no packet-type byte in either direction, since
    /// the endpoint already says what the packet is. A transport that keeps reading bulk
    /// IN as ACL decodes every firmware acknowledgement as a malformed L2CAP fragment and
    /// drops it, and the upload times out on its first fragment having in fact succeeded
    /// (#229, confirmed against an AX210 on 2026-08-08).
    ///
    /// Default is a no-op: only the USB transport has pipes to choose between, and a
    /// controller reached over a socket or TCP has already had this done for it.
    fn set_bootloader_framing(&self, _on: bool) {}
}

/// A transport that answers from a script instead of a radio.
///
/// This is what lets the entire stack above HCI be tested with no hardware (ground rule
/// 6): queue the events a controller would emit, run the real host code against them, and
/// assert on the packets it sent. Used by `substrate-l2cap` and `proto-bluetooth-audio`
/// as well as this crate's own tests, which is why it ships in the library rather than
/// under `#[cfg(test)]`.
pub struct ScriptedTransport {
    inbound: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<HciPacket>>,
    feed: tokio::sync::mpsc::UnboundedSender<HciPacket>,
    sent: Mutex<Vec<HciPacket>>,
    #[allow(clippy::type_complexity)]
    responder: Option<Box<dyn Fn(&HciPacket) -> Vec<HciPacket> + Send + Sync>>,
}

impl std::fmt::Debug for ScriptedTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedTransport")
            .field("sent", &self.sent.lock().map(|s| s.len()).unwrap_or(0))
            .field("auto_responder", &self.responder.is_some())
            .finish()
    }
}

impl Default for ScriptedTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedTransport {
    /// A transport with nothing queued.
    #[must_use]
    pub fn new() -> Self {
        let (feed, inbound) = tokio::sync::mpsc::unbounded_channel();
        Self {
            inbound: tokio::sync::Mutex::new(inbound),
            feed,
            sent: Mutex::new(Vec::new()),
            responder: None,
        }
    }

    /// Attach a function that answers each sent packet, the way a controller would.
    #[must_use]
    pub fn with_responder(
        mut self,
        f: impl Fn(&HciPacket) -> Vec<HciPacket> + Send + Sync + 'static,
    ) -> Self {
        self.responder = Some(Box::new(f));
        self
    }

    /// Queue a packet for the host to receive.
    pub fn push(&self, packet: HciPacket) {
        let _ = self.feed.send(packet);
    }

    /// Queue several packets in order.
    pub fn push_all(&self, packets: impl IntoIterator<Item = HciPacket>) {
        for p in packets {
            self.push(p);
        }
    }

    /// Everything the host has sent, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<HciPacket> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Just the commands the host has sent, in order.
    #[must_use]
    pub fn sent_commands(&self) -> Vec<crate::opcode::OpCode> {
        self.sent()
            .iter()
            .filter_map(|p| match p {
                HciPacket::Command { opcode, .. } => Some(*opcode),
                _ => None,
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl HciTransport for ScriptedTransport {
    async fn send(&self, packet: HciPacket) -> Result<(), HciError> {
        let replies = self.responder.as_ref().map(|f| f(&packet));
        if let Ok(mut sent) = self.sent.lock() {
            sent.push(packet);
        }
        for reply in replies.into_iter().flatten() {
            self.push(reply);
        }
        Ok(())
    }

    async fn recv(&self) -> Result<HciPacket, HciError> {
        let mut guard = self.inbound.lock().await;
        guard
            .recv()
            .await
            .ok_or_else(|| HciError::Transport("scripted transport closed".into()))
    }
}

/// Reassembles L2CAP PDUs out of ACL fragments, per connection handle.
///
/// Lives here rather than in `substrate-l2cap` because it is a property of the *HCI*
/// framing — fragments are created by the controller's ACL buffer size, not by anything
/// L2CAP decided — and because both the ACL path and any future SCO path need the same
/// per-handle bookkeeping.
#[derive(Debug, Default)]
pub struct Reassembler {
    /// In-progress PDU per handle, with the length its L2CAP header declared.
    partial: std::collections::HashMap<u16, (usize, VecDeque<u8>)>,
}

impl Reassembler {
    /// A reassembler with no connections in flight.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one ACL fragment; returns a complete L2CAP PDU when one finishes.
    ///
    /// # Errors
    /// [`HciError::Truncated`] if a continuation arrives for a handle with nothing in
    /// progress — which means a fragment was lost and the stream is no longer trustworthy.
    pub fn push(
        &mut self,
        packet: &crate::packet::AclPacket,
    ) -> Result<Option<bytes::Bytes>, HciError> {
        let key = packet.handle.raw();
        if packet.boundary.starts_pdu() {
            // An L2CAP PDU starts with a 2-byte length that does *not* count itself or
            // the 2-byte CID, so the full PDU is length + 4.
            if packet.data.len() < 2 {
                return Err(HciError::Truncated {
                    what: "l2cap length header",
                    need: 2,
                    have: packet.data.len(),
                });
            }
            let declared = usize::from(u16::from_le_bytes([packet.data[0], packet.data[1]])) + 4;
            let mut buf = VecDeque::with_capacity(declared);
            buf.extend(packet.data.iter().copied());
            self.partial.insert(key, (declared, buf));
        } else {
            let Some((_, buf)) = self.partial.get_mut(&key) else {
                return Err(HciError::Truncated {
                    what: "acl continuation without a first fragment",
                    need: 1,
                    have: 0,
                });
            };
            buf.extend(packet.data.iter().copied());
        }

        let Some((declared, buf)) = self.partial.get(&key) else {
            return Ok(None);
        };
        if buf.len() >= *declared {
            let (declared, buf) = self.partial.remove(&key).unwrap_or((0, VecDeque::new()));
            let full: Vec<u8> = buf.into_iter().take(declared).collect();
            return Ok(Some(bytes::Bytes::from(full)));
        }
        Ok(None)
    }

    /// Forget any partial PDU for a handle that just disconnected.
    pub fn forget(&mut self, handle: crate::packet::ConnectionHandle) {
        self.partial.remove(&handle.raw());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use bytes::Bytes;

    use super::*;
    use crate::command::Command;
    use crate::event::{code, Event};
    use crate::opcode::OpCode;
    use crate::packet::{AclPacket, ConnectionHandle, PacketBoundary};

    #[tokio::test]
    async fn a_scripted_controller_answers_a_reset() {
        let transport = ScriptedTransport::new().with_responder(|sent| match sent {
            HciPacket::Command { opcode, .. } if *opcode == OpCode::RESET => {
                vec![HciPacket::Event {
                    code: code::COMMAND_COMPLETE,
                    params: Bytes::from_static(&[0x01, 0x03, 0x0c, 0x00]),
                }]
            }
            _ => vec![],
        });

        transport
            .send(Command::Reset.encode().unwrap())
            .await
            .unwrap();
        let HciPacket::Event { code, params } = transport.recv().await.unwrap() else {
            panic!("expected an event");
        };
        let Event::CommandComplete { opcode, .. } = Event::parse(code, &params).unwrap() else {
            panic!("expected command complete");
        };
        assert_eq!(opcode, OpCode::RESET);
        assert_eq!(transport.sent_commands(), vec![OpCode::RESET]);
    }

    fn acl(boundary: PacketBoundary, data: &[u8]) -> AclPacket {
        AclPacket::new(
            ConnectionHandle::new(0x0b).unwrap(),
            boundary,
            Bytes::copy_from_slice(data),
        )
    }

    #[test]
    fn a_pdu_split_across_fragments_is_rejoined() {
        // L2CAP header says 6 payload bytes on CID 0x0040, so the PDU is 10 bytes; the
        // controller's ACL buffer split it after 4. This is the ordinary case on any
        // dongle with a small buffer, not an edge case.
        let mut r = Reassembler::new();
        assert_eq!(
            r.push(&acl(
                PacketBoundary::FirstFlushable,
                &[0x06, 0x00, 0x40, 0x00]
            ))
            .unwrap(),
            None
        );
        let done = r
            .push(&acl(PacketBoundary::Continuing, &[1, 2, 3, 4, 5, 6]))
            .unwrap()
            .expect("PDU should be complete");
        assert_eq!(&done[..], &[0x06, 0x00, 0x40, 0x00, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_pdu_that_arrives_whole_needs_no_second_fragment() {
        let mut r = Reassembler::new();
        let done = r
            .push(&acl(
                PacketBoundary::FirstFlushable,
                &[0x02, 0x00, 0x40, 0x00, 0xAA, 0xBB],
            ))
            .unwrap();
        assert_eq!(done.unwrap()[..], [0x02, 0x00, 0x40, 0x00, 0xAA, 0xBB]);
    }

    #[test]
    fn a_continuation_with_nothing_in_progress_is_an_error() {
        // Means a first fragment was lost. Silently starting mid-PDU would hand L2CAP a
        // garbage header and desynchronise the channel for good.
        let mut r = Reassembler::new();
        assert!(r
            .push(&acl(PacketBoundary::Continuing, &[1, 2, 3]))
            .is_err());
    }

    #[test]
    fn interleaved_connections_do_not_mix_their_fragments() {
        let mut r = Reassembler::new();
        let a = ConnectionHandle::new(0x0a).unwrap();
        let b = ConnectionHandle::new(0x0b).unwrap();
        let first = |h, d: &[u8]| {
            AclPacket::new(h, PacketBoundary::FirstFlushable, Bytes::copy_from_slice(d))
        };
        let cont =
            |h, d: &[u8]| AclPacket::new(h, PacketBoundary::Continuing, Bytes::copy_from_slice(d));

        r.push(&first(a, &[0x04, 0x00, 0x40, 0x00])).unwrap();
        r.push(&first(b, &[0x04, 0x00, 0x41, 0x00])).unwrap();
        assert_eq!(
            r.push(&cont(b, &[0xB1, 0xB2, 0xB3, 0xB4]))
                .unwrap()
                .unwrap()[4..],
            [0xB1, 0xB2, 0xB3, 0xB4]
        );
        assert_eq!(
            r.push(&cont(a, &[0xA1, 0xA2, 0xA3, 0xA4]))
                .unwrap()
                .unwrap()[4..],
            [0xA1, 0xA2, 0xA3, 0xA4]
        );
    }

    #[test]
    fn forgetting_a_handle_drops_its_partial_pdu() {
        let mut r = Reassembler::new();
        let h = ConnectionHandle::new(0x0b).unwrap();
        r.push(&acl(
            PacketBoundary::FirstFlushable,
            &[0x08, 0x00, 0x40, 0x00],
        ))
        .unwrap();
        r.forget(h);
        assert!(r.push(&acl(PacketBoundary::Continuing, &[1, 2])).is_err());
    }
}
