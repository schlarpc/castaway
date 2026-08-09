//! The one place ACL data leaves this process.
//!
//! Everything outbound — AVDTP replies, SDP responses, L2CAP signaling, AVRCP commands —
//! funnels through a single writer task. That is two guarantees, and #71
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
    /// Links the controller has told us are gone.
    ///
    /// Needed because `link_down` reclaims what is *outstanding*, and jobs already sitting
    /// in the queue for that handle are not outstanding yet. Without this the writer went
    /// on to `claim` against a dead handle, inserting an entry that no
    /// `Number_Of_Completed_Packets` will ever retire and that `link_down` — long since
    /// fired — will never reclaim. Each occurrence permanently shrank the pool, and with
    /// the deploy dongle's six credits, six phones walking away mid-write wedged all
    /// outbound ACL for the life of the process.
    dead: std::collections::HashSet<u16>,
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
                dead: std::collections::HashSet::new(),
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
        let reclaimed = {
            let mut buffers = self.buffers.lock().await;
            buffers.dead.insert(handle.raw());
            buffers.credits.link_down(handle)
        };
        // Anything still queued for this handle is now undeliverable, so wake the writer
        // even when nothing was reclaimed: it may be parked in `claim` for a link that is
        // never going to return a credit.
        self.replenished.notify_one();
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
    ///
    /// `false` if the link died while waiting — the caller must abandon the PDU rather
    /// than send it, or it consumes a credit that will never come back.
    async fn claim(&self, handle: ConnectionHandle) -> bool {
        loop {
            {
                let mut buffers = self.buffers.lock().await;
                if buffers.dead.contains(&handle.raw()) {
                    return false;
                }
                if buffers.credits.claim(handle) {
                    return true;
                }
            }
            trace!(%handle, "acl: out of controller buffers; waiting");
            self.replenished.notified().await;
        }
    }

    /// Note a link coming up, so a reused handle is not treated as dead.
    ///
    /// Handles are the controller's to allocate and it reuses them freely, so "dead"
    /// cannot be permanent without eventually refusing to write to a live phone.
    pub async fn link_up(&self, handle: ConnectionHandle) {
        self.buffers.lock().await.dead.remove(&handle.raw());
    }

    async fn pump(self, transport: Arc<dyn HciTransport>, mut jobs: mpsc::UnboundedReceiver<Job>) {
        while let Some(Job { handle, pdu }) = jobs.recv().await {
            // A PDU queued just before the link dropped. Sending it is impossible and
            // claiming for it leaks a credit, so drop it here.
            if self.buffers.lock().await.dead.contains(&handle.raw()) {
                debug!(%handle, "acl: dropping a pdu queued for a dead link");
                continue;
            }
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
                if !self.claim(handle).await {
                    debug!(%handle, "acl: link died mid-pdu; abandoning the rest");
                    break;
                }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Arc;

    use bytes::Bytes;
    use substrate_hci::{ConnectionHandle, HciPacket, HciTransport};
    use substrate_l2cap::{Cid, L2capPdu};

    use super::AclWriter;

    /// Accepts everything and remembers nothing. The point here is credit accounting, not
    /// what reached the wire.
    #[derive(Debug)]
    struct Sink;

    #[async_trait::async_trait]
    impl HciTransport for Sink {
        async fn send(&self, _packet: HciPacket) -> Result<(), substrate_hci::HciError> {
            Ok(())
        }
        async fn recv(&self) -> Result<HciPacket, substrate_hci::HciError> {
            std::future::pending().await
        }
    }

    fn pdu() -> L2capPdu {
        L2capPdu::new(Cid::new(0x0040), Bytes::from_static(b"hello"))
    }

    #[tokio::test]
    async fn a_pdu_queued_for_a_link_that_just_died_does_not_eat_a_credit() {
        // The leak the existing end-to-end test cannot reach: it drops a link with
        // *nothing* queued. `link_down` reclaims what is outstanding, but a job already in
        // the queue is not outstanding yet — so the writer went on to claim against a dead
        // handle, inserting an entry no completion event will ever retire and that
        // `link_down` has already run past. Each one permanently shrank the pool; six of
        // them on the deploy dongle stopped all outbound ACL for the life of the process.
        let writer = AclWriter::spawn(Arc::new(Sink));
        writer.configure(6, 1021).await;
        let handle = ConnectionHandle::new(0x0001).unwrap();
        writer.link_up(handle).await;

        writer.link_down(handle).await;
        for _ in 0..10 {
            writer.send(handle, pdu());
        }
        // Let the writer drain the queue.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            writer.outstanding().await,
            0,
            "a dead link's queued pdus must not consume credits"
        );
    }

    #[tokio::test]
    async fn a_reused_handle_is_writable_again() {
        // Handles belong to the controller and it reuses them, so "dead" cannot be
        // permanent — the next phone to arrive on that handle must not be refused.
        let writer = AclWriter::spawn(Arc::new(Sink));
        writer.configure(6, 1021).await;
        let handle = ConnectionHandle::new(0x0001).unwrap();
        writer.link_down(handle).await;
        writer.link_up(handle).await;
        writer.send(handle, pdu());
        // "A live link must still be able to send": the claim shows up whenever the
        // writer task gets to the queue, so poll for it rather than sleeping a guess.
        castaway_test_support::eventually_async("a live link's pdu claiming a credit", || async {
            (writer.outstanding().await > 0).then_some(())
        })
        .await;
    }
}
