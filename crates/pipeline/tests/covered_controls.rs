//! A control that cannot be seen must not answer to touches.
//!
//! The transport strip is drawn *below* video, so a session that publishes both metadata
//! and pixels — a DLNA video with tags — leaves the strip composited but invisible. It
//! kept claiming the bottom-centre 62%×20% of the panel anyway, which is a press that
//! lands on the picture and pauses it, or lands on the picture and does nothing while
//! being swallowed before it can reach anything else.
//!
//! Reachability was confirmed before the fix: `transport_owns` answered `true` with a
//! full-surface video layer over the strip.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use castaway_core::{ControlCapabilities, DecodedFrame, FrameImage, NowPlaying, PlaybackState};
use pipeline::nowplaying_card::NowPlayingCard;
use pipeline::render_pipeline::{FrameEpoch, RenderCommand, RenderLoop};

/// A point inside the strip: bottom-centre.
const ON_STRIP: (f32, f32) = (0.5, 0.93);

fn card() -> NowPlayingCard {
    let mut track = NowPlaying::default().with_title("a video that also published metadata");
    track.state = PlaybackState::Playing;
    NowPlayingCard {
        track,
        source: castaway_core::SourceDescription::default(),
        up_next: Vec::new(),
        controls: ControlCapabilities::PLAY | ControlCapabilities::PAUSE,
    }
}

fn video_frame(width: u32, height: u32) -> DecodedFrame {
    DecodedFrame {
        width,
        height,
        pts: std::time::Duration::ZERO,
        image: FrameImage::Cpu {
            format: castaway_core::PixelFormat::Rgba8,
            data: bytes::Bytes::from(vec![0xff; (width * height * 4) as usize]),
        },
    }
}

#[test]
fn a_strip_hidden_under_video_neither_owns_nor_acts() {
    let (tx, rx) = pipeline::render_channel(8);
    let mut render = RenderLoop::offscreen(1280, 720, rx).unwrap();

    tx.send(RenderCommand::NowPlaying(Box::new(card())));
    render.pump();
    assert!(
        render.transport_owns(ON_STRIP.0, ON_STRIP.1),
        "with nothing over it the strip should take the press — otherwise this test \
         proves nothing about coverage"
    );

    tx.send(RenderCommand::Video(
        video_frame(1280, 720),
        FrameEpoch::ALWAYS_FRESH,
    ));
    render.pump();

    assert!(
        !render.transport_owns(ON_STRIP.0, ON_STRIP.1),
        "the strip is under a full-screen video and invisible; it must not swallow the press"
    );
    assert!(
        render
            .transport_action(
                ON_STRIP.0,
                ON_STRIP.1,
                pipeline::transport::TouchPhase::Press
            )
            .is_none(),
        "a covered strip must not act either — a press on the picture would pause it"
    );
}
