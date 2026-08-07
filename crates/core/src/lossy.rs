//! Ground rule 4's drop-late-frames policy as a type: a frame sender that never
//! blocks, tells a slow consumer apart from a dead one, and counts its own drops.
//!
//! Every live-media adapter sends frames with `try_send` because latency beats
//! freshness — a full queue means the decoder is behind, and the right answer is to
//! drop the frame, not stall the socket. Five adapters hand-rolled that policy, and
//! the conflation hazard shipped twice: a bare `is_err()` reads a **closed** channel
//! as "queue full", so a receiver that is simply gone produced thousands of lines
//! blaming backpressure (`proto-bluetooth-audio` documented it; `proto-airplay`
//! carried the identical bug until #221). [`LossySend`] makes the distinction the
//! type system's problem: there is no way to learn whether the frame went through
//! without also being told which failure it was.
//!
//! Drops are counted in the sender itself, where every send site passes, rather than
//! retrofitted at each call site (the #174/#175 lesson). A counter can also be shared
//! across senders via [`LossySender::with_counter`] — for an adapter that rebuilds
//! its channels mid-session (pause/resume) but owes the log one session total (#192).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

/// What one lossy send did.
///
/// A `match` on this is the point: [`LossySend::Dropped`] is a hiccup (slow consumer,
/// frame sacrificed, counted), [`LossySend::Closed`] is a dead session (receiver gone,
/// not counted as a drop) — and they must never share a log line.
#[must_use = "a Closed channel is a dead consumer, not a slow one; ignoring the difference re-creates the bluetooth log-storm bug"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossySend {
    /// The frame was queued for the consumer.
    Sent,
    /// The queue was full; the frame was dropped and counted. The consumer is alive
    /// but behind — the ground-rule-4 trade, working as designed.
    Dropped,
    /// The receiver is gone and is not coming back. Not counted as a drop: nothing
    /// about backpressure is true here, and the caller usually wants to end the
    /// session rather than keep sending.
    Closed,
}

/// A shared drop tally, readable by tests and end-of-session reports.
///
/// Cloning shares the underlying counter; [`DropCounter::get`] reads the total across
/// every [`LossySender`] built from it.
#[derive(Debug, Clone, Default)]
pub struct DropCounter(Arc<AtomicU64>);

impl DropCounter {
    /// A fresh counter at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of frames dropped so far.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn increment(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A non-blocking frame sender that drops on a full queue and counts the drop.
///
/// Wraps [`mpsc::Sender`]; the adapter keeps only the per-protocol decision of *which*
/// streams are lossy and what each outcome means for its session.
#[derive(Debug)]
pub struct LossySender<T> {
    tx: mpsc::Sender<T>,
    drops: DropCounter,
}

impl<T> Clone for LossySender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            drops: self.drops.clone(),
        }
    }
}

impl<T> LossySender<T> {
    /// Wrap a channel with its own fresh drop counter.
    #[must_use]
    pub fn new(tx: mpsc::Sender<T>) -> Self {
        Self::with_counter(tx, DropCounter::new())
    }

    /// Wrap a channel, tallying drops into an existing counter — for a session whose
    /// channels are rebuilt (pause/resume) but whose drop total must span the rebuilds.
    #[must_use]
    pub fn with_counter(tx: mpsc::Sender<T>, drops: DropCounter) -> Self {
        Self { tx, drops }
    }

    /// Send without blocking; a full queue drops `item` and counts it.
    pub fn send(&self, item: T) -> LossySend {
        match self.tx.try_send(item) {
            Ok(()) => LossySend::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.drops.increment();
                LossySend::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => LossySend::Closed,
        }
    }

    /// Frames dropped so far (across every sender sharing the counter).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.drops.get()
    }

    /// A handle on the drop tally, for reporting after the sender itself is gone.
    #[must_use]
    pub fn drop_counter(&self) -> DropCounter {
        self.drops.clone()
    }

    /// Completes when the receiving half is dropped — the select-branch form of
    /// [`LossySend::Closed`], for loops that should wake on a dead consumer rather
    /// than discover it on the next frame.
    pub async fn closed(&self) {
        self.tx.closed().await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn a_full_queue_drops_and_counts() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = LossySender::new(tx);
        assert_eq!(sender.send(1u8), LossySend::Sent);
        assert_eq!(sender.send(2), LossySend::Dropped);
        assert_eq!(sender.send(3), LossySend::Dropped);
        assert_eq!(sender.dropped(), 2);
        // The frame that got through is the oldest, untouched by the drops.
        assert_eq!(rx.recv().await, Some(1));
    }

    #[tokio::test]
    async fn a_closed_channel_is_not_a_drop() {
        // The bluetooth/airplay conflation: a dead consumer must not read as
        // backpressure, and must not inflate the drop tally.
        let (tx, rx) = mpsc::channel::<u8>(1);
        drop(rx);
        let sender = LossySender::new(tx);
        assert_eq!(sender.send(1), LossySend::Closed);
        assert_eq!(sender.dropped(), 0);
    }

    #[tokio::test]
    async fn a_shared_counter_survives_channel_rebuilds() {
        // The miracast pause/resume case (#192): the channels are rebuilt, the
        // session total is not.
        let session_drops = DropCounter::new();
        for _ in 0..2 {
            let (tx, _rx) = mpsc::channel(1);
            let sender = LossySender::with_counter(tx, session_drops.clone());
            assert_eq!(sender.send(1u8), LossySend::Sent);
            assert_eq!(sender.send(2), LossySend::Dropped);
        }
        assert_eq!(session_drops.get(), 2);
    }

    #[tokio::test]
    async fn closed_wakes_when_the_receiver_drops() {
        let (tx, rx) = mpsc::channel::<u8>(1);
        let sender = LossySender::new(tx);
        drop(rx);
        // Completes rather than hangs.
        sender.closed().await;
    }
}
