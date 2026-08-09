//! Moving a pulled A/V session to a new position.
//!
//! Seeking looks like one operation and is four, spread across three threads that never
//! call each other:
//!
//! 1. Somebody asks — a control point's `Seek`, or a finger on the panel's scrubber. That
//!    arrives on the tokio runtime.
//! 2. The demuxer moves, on the decode thread, and flushes both decoders so no picture
//!    from the old position survives inside them.
//! 3. The **audio already queued** is thrown away, on the output thread. This is the step
//!    that is easy to leave out and impossible to miss afterwards: the queue between the
//!    demuxer and the speaker holds around a second of decoded sound, all of it from where
//!    playback used to be, so without this a seek plays a second of the old place first.
//! 4. The clock is re-anchored, or the picture waits for a time that is never coming.
//!
//! [`SeekControl`] is the handshake between those threads. It is deliberately not a
//! channel: two of the four steps are things a thread does to itself, and the only thing
//! that genuinely has to cross is "I have thrown mine away" — which is an acknowledgement,
//! not a message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the decode thread will wait for the output to admit it has dropped the audio
/// it had queued.
///
/// Bounded, because the alternative is a seek that hangs the decode thread when there is
/// no output thread at all — a video-only file, a build with no audio feature, an output
/// that already died. Long enough that a scheduled-out audio thread still gets there:
/// it looks at the flag once per block, which is tens of milliseconds.
const FLUSH_GRACE: Duration = Duration::from_millis(250);

/// How often the decode thread looks while waiting for that acknowledgement.
const FLUSH_POLL: Duration = Duration::from_millis(2);

/// The seek handshake for one media-URL session.
///
/// Shared by the pipeline handle (which requests), the decode thread (which performs), and
/// the PCM output thread (which discards what it had queued).
#[derive(Debug, Default)]
pub struct SeekControl {
    /// Where the driver asked to go, until the decode thread takes it.
    ///
    /// A single slot rather than a queue: two seeks arriving before either is served means
    /// somebody dragged a scrubber, and the second is the one they meant.
    requested: Mutex<Option<Duration>>,
    /// Where the presentation is going, from [`Self::request`] until the decode thread
    /// has finished serving it — the clock re-anchored, or the seek refused.
    ///
    /// This exists for whoever reports position (#232). The window between a `Seek`
    /// arriving on the runtime and `MediaClock::seek_to` running on the decode thread
    /// contains a blocking demuxer move — for an HTTP source, a range request and a
    /// refill — and through all of it the clock free-runs from the *pre-seek* anchor. A
    /// scrubber reading the clock alone snaps back to the old position and creeps until
    /// the demuxer lands. The answer already exists the whole time; this keeps it
    /// readable instead of consumed.
    destination: Mutex<Option<Duration>>,
    /// Bumped once per seek by the decode thread, after the demuxer has moved.
    epoch: AtomicU64,
    /// Echoed back by the output thread once it has dropped everything it had queued.
    drained: AtomicU64,
}

/// Lock, recovering a poisoned mutex: a panic elsewhere is not a reason to stop
/// answering where playback is.
fn lock_recovering<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match lock.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl SeekControl {
    /// A control with no seek outstanding.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for playback to move to `target`. Called from whoever is driving.
    pub fn request(&self, target: Duration) {
        *lock_recovering(&self.requested) = Some(target);
        *lock_recovering(&self.destination) = Some(target);
    }

    /// Where the presentation is going, while a seek is outstanding or being served.
    ///
    /// `None` in steady state. While this is `Some`, it — not the clock — is the honest
    /// answer to "where is playback": the clock still measures the position the seek is
    /// leaving.
    #[must_use]
    pub fn destination(&self) -> Option<Duration> {
        *lock_recovering(&self.destination)
    }

    /// The seek has been served: the clock is re-anchored at the destination, or the
    /// demuxer refused and playback carries on from where it was. Either way the clock is
    /// the authority again. Called by the decode thread.
    pub fn served(&self) {
        // The `requested` guard is held across the clear, and `request` writes both
        // fields in the same order: a request that lost the race to this lock re-fills
        // the destination after it, and one that won is seen here — so a seek the decode
        // thread has not started yet never has its destination wiped.
        let requested = lock_recovering(&self.requested);
        if requested.is_none() {
            *lock_recovering(&self.destination) = None;
        }
    }

    /// Whether a seek is waiting to be served.
    ///
    /// Read from inside the decode thread's frame-pacing wait: a frame held for its turn
    /// against a clock that is about to move is a frame nobody wants, and a *paused*
    /// session's frame would otherwise wait for a turn that never comes — so a seek has to
    /// be able to interrupt the wait, not merely be noticed after it.
    #[must_use]
    pub fn pending(&self) -> bool {
        lock_recovering(&self.requested).is_some()
    }

    /// Take the outstanding seek, if any. Called by the decode thread.
    #[must_use]
    pub fn take(&self) -> Option<Duration> {
        lock_recovering(&self.requested).take()
    }

    /// Tell the output thread that everything it has queued is from before a seek, and
    /// wait — briefly — for it to say it has thrown that away.
    ///
    /// Returning without an acknowledgement is a normal outcome, not a failure: a session
    /// with no audio at all has nobody to answer, and a seek must not hang waiting for a
    /// thread that does not exist. The cost of the rare unacknowledged case is the glitch
    /// this exists to remove, not a broken session.
    pub fn flush_audio(&self) {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let deadline = Instant::now() + FLUSH_GRACE;
        while self.drained.load(Ordering::SeqCst) != epoch {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(FLUSH_POLL);
        }
    }

    /// Whether the output thread should drop what it has queued, and which epoch doing so
    /// would satisfy. [`None`] when nothing has changed since it last looked.
    #[must_use]
    pub fn flush_wanted(&self) -> Option<u64> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        (epoch != self.drained.load(Ordering::SeqCst)).then_some(epoch)
    }

    /// Acknowledge that everything queued as of `epoch` has been dropped.
    pub fn flushed(&self, epoch: u64) {
        self.drained.store(epoch, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_last_request_wins() {
        // A dragged scrubber emits a run of positions and means only the final one.
        let s = SeekControl::new();
        s.request(Duration::from_secs(10));
        s.request(Duration::from_secs(90));
        assert!(s.pending());
        assert_eq!(s.take(), Some(Duration::from_secs(90)));
        assert!(!s.pending());
        assert_eq!(s.take(), None);
    }

    #[test]
    fn the_destination_outlives_the_take_until_the_seek_is_served() {
        // #232's window, as the scrubber sees it. The decode thread takes the request and
        // then spends however long a blocking demuxer move costs — a range request and a
        // refill for an HTTP source — before the clock is re-anchored. `take` consuming
        // the answer is what made the scrubber snap back to the clock's pre-seek position
        // through that whole window.
        let s = SeekControl::new();
        assert_eq!(s.destination(), None, "no seek, the clock is the authority");
        s.request(Duration::from_secs(90));
        assert_eq!(s.destination(), Some(Duration::from_secs(90)));
        // The decode thread picks it up; the answer must not go with it.
        assert_eq!(s.take(), Some(Duration::from_secs(90)));
        assert_eq!(
            s.destination(),
            Some(Duration::from_secs(90)),
            "mid-service is exactly when the scrubber asks"
        );
        s.served();
        assert_eq!(s.destination(), None, "the clock is the authority again");
    }

    #[test]
    fn a_request_landing_mid_service_is_not_wiped_by_the_older_seeks_served() {
        // A finger still dragging while the decode thread serves the previous position:
        // the older seek's `served` must not erase where the newer one is going.
        let s = SeekControl::new();
        s.request(Duration::from_secs(10));
        assert_eq!(s.take(), Some(Duration::from_secs(10)));
        s.request(Duration::from_secs(20));
        s.served();
        assert_eq!(
            s.destination(),
            Some(Duration::from_secs(20)),
            "the newer seek has not been started, let alone served"
        );
        assert_eq!(s.take(), Some(Duration::from_secs(20)));
        s.served();
        assert_eq!(s.destination(), None);
    }

    #[test]
    fn a_flush_is_wanted_once_and_satisfied_once() {
        let shared = std::sync::Arc::new(SeekControl::new());
        assert_eq!(
            shared.flush_wanted(),
            None,
            "nothing to drop before any seek"
        );
        let audio = {
            let shared = std::sync::Arc::clone(&shared);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    if let Some(epoch) = shared.flush_wanted() {
                        shared.flushed(epoch);
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                false
            })
        };
        shared.flush_audio();
        assert!(audio.join().unwrap(), "the output never saw the flush");
        assert_eq!(
            shared.flush_wanted(),
            None,
            "a satisfied flush must not be asked for again"
        );
    }

    /// A video-only session has no output thread to answer, and a seek that waited for one
    /// would hang the decode thread for as long as the file lasts.
    #[test]
    fn a_flush_nobody_answers_gives_up_rather_than_hanging() {
        let s = SeekControl::new();
        let start = Instant::now();
        s.flush_audio();
        let waited = start.elapsed();
        assert!(
            waited >= FLUSH_GRACE,
            "it did not actually wait: {waited:?}"
        );
        assert!(
            waited < FLUSH_GRACE * 4,
            "it waited far past the grace period: {waited:?}"
        );
    }
}
