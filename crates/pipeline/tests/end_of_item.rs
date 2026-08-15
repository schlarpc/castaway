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
use pipeline::render_pipeline::FrameEpoch;
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
    let (clock, time) = pipeline::render_clock::RenderClock::manual();
    let Some(render) = pipeline::test_gpu::render_loop(W, H, rx) else {
        return;
    };
    let mut render = render.with_clock(clock);

    // An item, playing: the layer is up.
    tx.send(RenderCommand::Video(frame(), FrameEpoch::ALWAYS_FRESH));
    render.pump();
    assert!(
        render.layer_size(LayerId::Video).is_some(),
        "a frame should put the video layer on the panel"
    );

    // The item ends the way it really ends — with frames still in the lane behind the
    // clear, because the decoder ran ahead of the render loop. Filling the lane is the
    // whole point of the test; a clear sent to an empty lane never had this bug.
    for _ in 0..8 {
        tx.send(RenderCommand::Video(frame(), FrameEpoch::ALWAYS_FRESH));
    }
    tx.send(RenderCommand::ClearVideo);

    // Now run the loop the way the kiosk does, for long enough to cover the clear's grace
    // and the layer's exit — in virtual time, because the grace is scheduled against the
    // injected clock and there is nothing here that needs a wall (#236). Five virtual
    // seconds at a frame per step.
    for _ in 0..320 {
        render.pump();
        render.tick_motion(FRAME);
        if render.layer_size(LayerId::Video).is_none() {
            return;
        }
        time.advance(FRAME);
    }
    panic!("the video layer outlived the item: the panel is still showing a finished cast");
}

#[test]
fn a_frame_of_the_next_item_still_calls_off_the_clear() {
    let (tx, rx) = render_channel(8);
    let (clock, time) = pipeline::render_clock::RenderClock::manual();
    let Some(render) = pipeline::test_gpu::render_loop(W, H, rx) else {
        return;
    };
    let mut render = render.with_clock(clock);

    tx.begin_epoch();
    tx.send_frame(frame());
    render.pump();

    // A scrub, as a control point that cannot seek in-band performs one: stop, then load
    // again. The frame arrives *after* the clear rather than behind it in the lane, and
    // that is the difference the fix has to keep — dropping stale frames must not turn
    // into ignoring live ones, or every scrub bares the idle screen. The new item begins
    // a session, so its frames outrank the clear (#358); this goes through the real epoch
    // path rather than a fixture constant, because that ordering is the thing under test.
    tx.send(RenderCommand::ClearVideo);
    render.pump();
    tx.begin_epoch();
    tx.send_frame(frame());
    render.pump();

    // Three virtual seconds — well past the grace a live clear would have fired at.
    for _ in 0..190 {
        render.pump();
        render.tick_motion(FRAME);
        assert!(
            render.layer_size(LayerId::Video).is_some(),
            "the next item's frame should have called the clear off"
        );
        time.advance(FRAME);
    }
}

#[test]
fn a_straggler_out_of_the_decoder_does_not_call_off_the_clear() {
    // #358, seen on the panel: stopping an AirPlay mirror left the last frame frozen on
    // the glass. `ClearVideo` drains the frame lane, but the mirror path is `encoded
    // frames → decode → compositor` and a frame still *inside the decoder* is not in the
    // lane to be drained. It arrived afterwards, cancelled the clear meant for the
    // session it belonged to, and nothing re-armed it — so the picture stayed until the
    // idle return two minutes later.
    //
    // The distinguishing fact is which session the frame speaks for, and nothing about
    // its timing: this straggler is delivered exactly like the next item's frame in the
    // test above, and must be treated the opposite way purely because no new session
    // began.
    let (tx, rx) = render_channel(8);
    let (clock, time) = pipeline::render_clock::RenderClock::manual();
    let Some(render) = pipeline::test_gpu::render_loop(W, H, rx) else {
        return;
    };
    let mut render = render.with_clock(clock);

    tx.begin_epoch();
    tx.send_frame(frame());
    render.pump();
    assert!(
        render.layer_size(LayerId::Video).is_some(),
        "a frame should put the video layer on the panel"
    );

    // The session ends. No `begin_epoch` follows, because nothing new started.
    tx.send(RenderCommand::ClearVideo);
    render.pump();

    // The decoder finishes the frame it was holding and pushes it at a loop that has
    // already cleared. Before the fix this line alone kept a finished mirror on screen.
    tx.send_frame(frame());
    render.pump();

    for _ in 0..320 {
        render.pump();
        render.tick_motion(FRAME);
        if render.layer_size(LayerId::Video).is_none() {
            return;
        }
        time.advance(FRAME);
    }
    panic!(
        "a frame from a cleared session kept the video layer up: the mirror is frozen on the panel"
    );
}
