//! The one place ACL data leaves this process.
//!
//! Everything outbound — AVDTP replies, SDP responses, L2CAP signaling, AVRCP commands —
//! funnels through a single writer task. That is two guarantees, and OPEN-QUESTIONS Q26
//! was most likely one of them being missing:
//!
//! 1. **Credits.** The controller advertises a fixed number of ACL buffers and silently
//!    drops anything beyond them ([`AclCredits`]). A dropped L2CAP configuration response
//!    is invisible from this end — the write succeeded — and presents as a peer that
//!    stops talking. The writer claims a credit per fragment and waits when the pool is
//!    empty.
//! 2. **No interleaving.** Basic-mode L2CAP has no segmentation, so a PDU's fragments
//!    must reach the peer consecutively; two tasks fragmenting concurrently onto one
//!    handle produce two corrupt PDUs. Before this, the adapter's main loop and the AVRCP
//!    control writer both wrote straight to the transport.
//!
//! The writer being its own task is also what keeps the reader honest: enqueueing never
//! blocks, so the actor loop that *receives* the completion events can never be parked
//! waiting for the credits those events would have delivered.

use std::sync::Arc;

use bytes::Bytes;
use substrate_hci::{
    AclCredits, AclPacket, ConnectionHandle, HciPacket, HciTransport, PacketBoundary,
};
use substrate_l2cap::L2capPdu;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, trace, warn};

/// What the controller told us about its ACL buffers.
#[derive(Debug)]
struct Buffers {
    credits: AclCredits,
    /// Largest ACL fragment the controller accepts.
    mtu: u16,
}

/// Serialises and paces every outbound ACL PDU.
///
/// Cheap to clone — a queue handle plus shared buffer accounting — so the actor loop and
/// any spawned writer share one pacing point rather than racing.
#[derive(Clone)]
pub struct AclWriter {
    jobs: mpsc::UnboundedSender<Job>,
    buffers: Arc<tokio::sync::Mutex<Buffers>>,
    /// Signalled whenever credits are returned. Exactly one task ever waits on it — the
    /// writer — so `notify_one` cannot lose a wakeup to a race with the claim check.
    replenished: Arc<Notify>,
}

impl std::fmt::Debug for AclWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AclWriter").finish_non_exhaustive()
    }
}

struct Job {
    handle: ConnectionHandle,
    pdu: L2capPdu,
}

/// Conservative defaults for the window before `HCI_Read_Buffer_Size` completes.
///
/// One credit and the spec's minimum fragment size: every controller can honour these,
/// and nothing but bring-up commands is sent before the real numbers arrive anyway.
const INITIAL_CREDITS: u16 = 1;
const INITIAL_MTU: u16 = 255;

impl AclWriter {
    /// Start the writer task over `transport`.
    #[must_use]
    pub fn spawn(transport: Arc<dyn HciTransport>) -> Self {
        let (jobs, rx) = mpsc::unbounded_channel();
        let writer = Self {
            jobs,
            buffers: Arc::new(tokio::sync::Mutex::new(Buffers {
                credits: AclCredits::new(INITIAL_CREDITS),
                mtu: INITIAL_MTU,
            })),
            replenished: Arc::new(Notify::new()),
        };
        tokio::spawn(writer.clone().pump(transport, rx));
        writer
    }

    /// Adopt the controller's real buffer size and count, from `HCI_Read_Buffer_Size`.
    pub async fn configure(&self, capacity: u16, mtu: u16) {
        {
            let mut buffers = self.buffers.lock().await;
            buffers.credits.set_capacity(capacity);
            buffers.mtu = mtu.max(1);
        }
        // Growing the pool can unblock a writer that is already waiting.
        self.replenished.notify_one();
    }

    /// Queue a PDU. Never blocks, so the actor loop that feeds credits back cannot be
    /// parked behind a write that is waiting for them.
    pub fn send(&self, handle: ConnectionHandle, pdu: L2capPdu) {
        if self.jobs.send(Job { handle, pdu }).is_err() {
            debug!("acl writer is gone; dropping a pdu");
        }
    }

    /// Hand back the credits an `HCI_Number_Of_Completed_Packets` event reported.
    pub async fn completed(&self, handle: ConnectionHandle, count: u16) {
        let released = self.buffers.lock().await.credits.complete(handle, count);
        if released > 0 {
            self.replenished.notify_one();
        }
    }

    /// Reclaim the buffers the controller flushed when a link dropped. No completion
    /// event ever arrives for those, so nothing else would return them.
    pub async fn link_down(&self, handle: ConnectionHandle) {
        let reclaimed = self.buffers.lock().await.credits.link_down(handle);
        if reclaimed > 0 {
            debug!(%handle, reclaimed, "acl: reclaimed buffers from a dead link");
            self.replenished.notify_one();
        }
    }

    /// Fragments outstanding at the controller. For tests and diagnostics.
    pub async fn outstanding(&self) -> u16 {
        self.buffers.lock().await.credits.outstanding()
    }

    /// Wait until one fragment may be sent on `handle`.
    async fn claim(&self, handle: ConnectionHandle) {
        loop {
            if self.buffers.lock().await.credits.claim(handle) {
                return;
            }
            trace!(%handle, "acl: out of controller buffers; waiting");
            self.replenished.notified().await;
        }
    }

    async fn pump(self, transport: Arc<dyn HciTransport>, mut jobs: mpsc::UnboundedReceiver<Job>) {
        while let Some(Job { handle, pdu }) = jobs.recv().await {
            let bytes = match pdu.encode() {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "acl: undeliverable pdu");
                    continue;
                }
            };
            // Read once per PDU: a mid-PDU resize would change the fragment size partway
            // through and the peer reassembles by declared length, not by fragment count.
            let mtu = usize::from(self.buffers.lock().await.mtu.max(1));
            for (i, chunk) in bytes.chunks(mtu).enumerate() {
                let boundary = if i == 0 {
                    PacketBoundary::FirstFlushable
                } else {
                    PacketBoundary::Continuing
                };
                self.claim(handle).await;
                if let Err(e) = transport
                    .send(HciPacket::Acl(AclPacket::new(
                        handle,
                        boundary,
                        Bytes::copy_from_slice(chunk),
                    )))
                    .await
                {
                    warn!(error = %e, "acl: transport write failed");
                    return;
                }
            }
        }
    }
}
