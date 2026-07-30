//! The two-lane render channel: frames may drop, transitions may not.
//!
//! The regression this pins: `RestPanel` (and every other state transition) used to
//! ride the same depth-3 bounded lane as decoded video frames, sent with `try_send`.
//! A cast that arrived while the lane happened to be full never took the screen —
//! the toast appeared, the audio played, and the panel just stayed where it was.
//! No GPU is needed here; this is channel semantics only.
#![cfg(feature = "render")]

use castaway_core::{DecodedFrame, PixelFormat};
use pipeline::{render_channel, RenderCommand};

fn frame() -> DecodedFrame {
    DecodedFrame::cpu(
        2,
        2,
        PixelFormat::Rgba8,
        std::time::Duration::ZERO,
        bytes::Bytes::from(vec![0u8; 16]),
    )
}

#[test]
fn a_transition_cannot_be_crowded_out_by_frames() {
    let (tx, rx) = render_channel(1);
    // Flood the frame lane far past its depth. The excess drops, by design.
    for _ in 0..8 {
        tx.send(RenderCommand::Video(frame()));
    }
    // The transition still arrives — and first, because a transition ordered
    // before a frame must apply before it.
    tx.send(RenderCommand::RestPanel);
    assert!(
        matches!(rx.try_recv(), Some(RenderCommand::RestPanel)),
        "a state transition must never be refused because frames are queued"
    );
    // The one frame the lane holds is still there behind it.
    assert!(matches!(rx.try_recv(), Some(RenderCommand::Video(_))));
    assert!(rx.try_recv().is_none(), "the flood was dropped, not queued");
}

#[test]
fn transitions_queue_without_bound() {
    let (tx, rx) = render_channel(1);
    for _ in 0..64 {
        tx.send(RenderCommand::RestPanel);
    }
    let mut got = 0;
    while let Some(cmd) = rx.try_recv() {
        assert!(matches!(cmd, RenderCommand::RestPanel));
        got += 1;
    }
    assert_eq!(got, 64, "every transition arrives, none dropped");
}

#[test]
fn a_dead_receiver_does_not_panic_the_sender() {
    let (tx, rx) = render_channel(1);
    drop(rx);
    // Both lanes: the send is a no-op, not a panic (ground rule 7 — no panicking
    // sends on runtime-reachable paths).
    tx.send(RenderCommand::RestPanel);
    assert!(!tx.send_frame(frame()), "a gone loop reports itself");
}
