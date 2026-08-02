//! What the panel shows after a cast ends.
//!
//! The regression this pins, found by #98's first end-to-end run through a real
//! compositor: the video layer was never taken down when an item finished, so the last
//! decoded frame stayed on a two-metre panel for the life of the session.
//!
//! It needed all three of the render channel's design decisions at once, which is why no
//! unit test had it. Frames ride a *bounded* lane, so at the end of an item several are
//! still queued. Control commands ride an unbounded lane that is drained *first*, so the
//! `ClearVideo` is applied before those stragglers. And a video frame *cancels* a pending
//! clear, because a control point that cannot seek in-band stops and re-`LOAD`s to scrub
//! and the screen must not flash idle between the two. Put together: the clear was
//! scheduled, the stragglers cancelled it, and nothing re-armed it.
//!
//! Everything below is about the seam between the lane and the loop, so it wants a real
//! `RenderLoop` — `render_channel.rs` holds the half that is channel semantics alone.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use castaway_core::{DecodedFrame, PixelFormat};
use pipeline::{render_channel, LayerId, RenderCommand};

const W: u32 = 320;
const H: u32 = 180;
const FRAME: Duration = Duration::from_millis(16);

fn frame() -> DecodedFrame {
    DecodedFrame::cpu(
        16,
        16,
        PixelFormat::Rgba8,
        Duration::ZERO,
        bytes::Bytes::from(vec![0xa0; 16 * 16 * 4]),
    )
}

#[test]
fn the_last_frame_of_a_finished_item_does_not_stay_on_the_panel() {
    let (tx, rx) = render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(W, H, rx) else {
        return;
    };

    // An item, playing: the layer is up.
    tx.send(RenderCommand::Video(frame()));
    render.pump();
    assert!(
        render.layer_size(LayerId::Video).is_some(),
        "a frame should put the video layer on the panel"
    );

    // The item ends the way it really ends — with frames still in the lane behind the
    // clear, because the decoder ran ahead of the render loop. Filling the lane is the
    // whole point of the test; a clear sent to an empty lane never had this bug.
    for _ in 0..8 {
        tx.send(RenderCommand::Video(frame()));
    }
    tx.send(RenderCommand::ClearVideo);

    // Now run the loop the way the kiosk does, for long enough to cover the clear's grace
    // and the layer's exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        render.pump();
        render.tick_motion(FRAME);
        if render.layer_size(LayerId::Video).is_none() {
            return;
        }
        std::thread::sleep(FRAME);
    }
    panic!("the video layer outlived the item: the panel is still showing a finished cast");
}

#[test]
fn a_frame_of_the_next_item_still_calls_off_the_clear() {
    let (tx, rx) = render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(W, H, rx) else {
        return;
    };

    tx.send(RenderCommand::Video(frame()));
    render.pump();

    // A scrub, as a control point that cannot seek in-band performs one: stop, then load
    // again. The frame arrives *after* the clear rather than behind it in the lane, and
    // that is the difference the fix has to keep — dropping stale frames must not turn
    // into ignoring live ones, or every scrub bares the idle screen.
    tx.send(RenderCommand::ClearVideo);
    render.pump();
    tx.send(RenderCommand::Video(frame()));
    render.pump();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        render.pump();
        render.tick_motion(FRAME);
        assert!(
            render.layer_size(LayerId::Video).is_some(),
            "the next item's frame should have called the clear off"
        );
        std::thread::sleep(FRAME);
    }
}
