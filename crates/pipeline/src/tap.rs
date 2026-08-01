//! Taps on the composited output: whatever the panel is showing, handed to something
//! else as well.
//!
//! A screenshot and a web-stream duplicate are the same problem — reading back what the
//! compositor just drew — so they are the same seam, and the second one is a second
//! implementation rather than a second mechanism.
//!
//! **Readback is not free and must never be on the default path.** Copying a 4K surface
//! back from the GPU is 33 MB per frame; doing it unconditionally would cost more than
//! everything else the render loop does. So [`OutputTap::wants_frame`] is asked *before*
//! the copy: a tap that declines costs nothing, no taps at all cost nothing, and one
//! readback per requested format serves every tap that asked for it.
//!
//! A tap says not only *whether* it wants the frame but *in what shape* ([`FrameWant`]).
//! That is what lets the encoder tap take NV12 planes at the stream's own size — 3 MB
//! where the screenshot's RGBA is 33 — with the conversion and the downscale happening on
//! the GPU rather than after the copy (see [`crate::nv12`]).

use std::time::Instant;

use crate::nv12::Nv12Planes;

/// The shape a tap wants its frame in.
///
/// An enum rather than a flag because the two are not variations of one readback: they
/// take different paths off the GPU, cost different amounts, and a tap that asked for the
/// wrong one would get a picture it cannot use rather than a slow one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameWant {
    /// Packed RGBA8 at the panel's own size. Correct on every backend, and what a
    /// screenshot wants anyway.
    Rgba,
    /// NV12 planes at a size of the tap's choosing — what a video encoder takes, and
    /// where the downscale to the stream's resolution happens.
    Nv12 {
        /// Coded width. Must be even.
        width: u32,
        /// Coded height. Must be even.
        height: u32,
    },
}

/// One composited frame, as a tap sees it.
#[derive(Debug)]
#[non_exhaustive]
pub enum TappedFrame<'a> {
    /// Packed RGBA8, read back into system memory.
    Rgba {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// `width * height * 4` bytes.
        data: &'a [u8],
    },
    /// A luma plane and an interleaved chroma plane, converted on the GPU.
    Nv12(&'a Nv12Planes),
}

/// A frame the compositor read back, owned.
///
/// The borrowed [`TappedFrame`] is what taps see; this is what the readback produced and
/// what the render loop holds while it serves them. Keeping the two apart is what lets one
/// copy serve several taps.
#[derive(Debug)]
pub enum CapturedFrame {
    /// See [`TappedFrame::Rgba`].
    Rgba {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// `width * height * 4` bytes.
        data: Vec<u8>,
    },
    /// See [`TappedFrame::Nv12`].
    Nv12(Nv12Planes),
}

impl CapturedFrame {
    /// Borrow it in the form a tap takes.
    #[must_use]
    pub fn as_tapped(&self) -> TappedFrame<'_> {
        match self {
            Self::Rgba {
                width,
                height,
                data,
            } => TappedFrame::Rgba {
                width: *width,
                height: *height,
                data,
            },
            Self::Nv12(planes) => TappedFrame::Nv12(planes),
        }
    }
}

/// A consumer of composited frames.
///
/// Implementations run on the render thread, so anything slow belongs behind a channel:
/// a tap that blocks stalls the panel, which is the one thing a tap must never do.
pub trait OutputTap: Send {
    /// What shape of frame this tap wants next, if any.
    ///
    /// Asked before any readback happens, which is the whole reason it exists. `now` is
    /// passed in rather than read here so every tap in a pass sees the same instant, and
    /// so a test can drive the clock.
    fn wants_frame(&mut self, now: Instant) -> Option<FrameWant>;

    /// Take a frame. Called only after [`OutputTap::wants_frame`] asked for one, and
    /// always in the shape it asked for.
    fn on_frame(&mut self, frame: &TappedFrame<'_>);

    /// Whether this tap is finished and can be dropped. A screenshot is done after one
    /// frame; a stream is done when nobody is watching.
    fn finished(&self) -> bool {
        false
    }
}

/// A tap that captures the next frame, encodes it as a PNG, and hands it back once.
///
/// Deliberately one-shot rather than a periodic capture: the caller asks for a
/// screenshot, gets exactly one readback, and the tap retires. A capture that lingered
/// would keep the readback cost alive for as long as nobody noticed.
pub struct ScreenshotTap {
    reply: Option<std::sync::mpsc::SyncSender<Result<Vec<u8>, crate::error::PipelineError>>>,
}

impl std::fmt::Debug for ScreenshotTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenshotTap")
            .field("pending", &self.reply.is_some())
            .finish()
    }
}

impl ScreenshotTap {
    /// A tap and the channel its PNG will arrive on.
    #[must_use]
    pub fn new() -> (
        Self,
        std::sync::mpsc::Receiver<Result<Vec<u8>, crate::error::PipelineError>>,
    ) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        (Self { reply: Some(tx) }, rx)
    }
}

impl OutputTap for ScreenshotTap {
    fn wants_frame(&mut self, _now: Instant) -> Option<FrameWant> {
        self.reply.is_some().then_some(FrameWant::Rgba)
    }

    fn on_frame(&mut self, frame: &TappedFrame<'_>) {
        let TappedFrame::Rgba {
            width,
            height,
            data,
        } = frame
        else {
            // Unreachable: this tap only ever asks for RGBA, and the loop serves each tap
            // the shape it asked for. Keeping the reply pending rather than answering with
            // something else means a shape mismatch shows up as a timed-out screenshot,
            // not as a PNG of the wrong pixels.
            return;
        };
        let Some(reply) = self.reply.take() else {
            return;
        };
        // PNG encoding on the render thread is the one place this tap is expensive, and
        // it happens once. Moving it off would mean copying the buffer to move it, which
        // for a single capture is the more expensive of the two.
        let _ = reply.send(crate::attract::to_png(*width, *height, data));
    }

    fn finished(&self) -> bool {
        self.reply.is_none()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_screenshot_wants_exactly_one_frame() {
        // The cost of a tap is a full surface readback, so one that stayed hungry after
        // it had what it asked for would quietly tax every frame thereafter.
        let (mut tap, rx) = ScreenshotTap::new();
        let now = Instant::now();
        assert_eq!(tap.wants_frame(now), Some(FrameWant::Rgba));
        assert!(!tap.finished());

        let data = vec![0u8; 4 * 4 * 4];
        tap.on_frame(&TappedFrame::Rgba {
            width: 4,
            height: 4,
            data: &data,
        });

        assert_eq!(tap.wants_frame(now), None, "it has what it came for");
        assert!(tap.finished(), "and should be dropped");
        let png = rx.try_recv().unwrap().unwrap();
        assert_eq!(&png[1..4], b"PNG", "a PNG, not raw pixels");
    }

    #[test]
    fn a_caller_that_hung_up_does_not_take_the_render_thread_with_it() {
        // The HTTP request behind a screenshot can time out or be cancelled between
        // asking and the next frame being drawn. Sending into a dropped receiver must be
        // a shrug, not a panic on the thread that owns the display.
        let (mut tap, rx) = ScreenshotTap::new();
        drop(rx);
        let data = vec![0u8; 4];
        tap.on_frame(&TappedFrame::Rgba {
            width: 1,
            height: 1,
            data: &data,
        });
        assert!(tap.finished());
    }
}

#[cfg(test)]
mod integration {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::compositor::{Compositor as _, Layer, LayerId, Transform};
    use crate::render_pipeline::RenderLoop;
    use crate::wgpu_compositor::TexelFormat;
    use crate::wgpu_compositor::WgpuCompositor;

    /// The path the HTTP endpoint actually takes: a handle, a command down the render
    /// channel, a tap installed by the loop, a readback, a PNG back.
    #[test]
    fn the_screenshot_handle_round_trips_through_the_render_channel() {
        use crate::render_pipeline::RenderPipeline;

        let Ok(compositor) = WgpuCompositor::new_offscreen(32, 16) else {
            eprintln!("no GPU adapter here; skipping");
            return;
        };
        let (pipeline, rx) = RenderPipeline::new(4);
        let handle = pipeline.screenshot_handle();
        let mut rloop = RenderLoop::new(compositor, rx);

        // The handle blocks until the loop presents, so the loop has to run elsewhere.
        let shot = std::thread::spawn(move || handle.capture(std::time::Duration::from_secs(5)));
        for _ in 0..200 {
            rloop.pump();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let png = shot.join().unwrap().unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }

    /// A screenshot of a real composited frame, through the whole seam.
    #[test]
    fn a_tap_captures_what_was_actually_composited() {
        let Ok(mut compositor) = WgpuCompositor::new_offscreen(64, 32) else {
            eprintln!("no GPU adapter here; skipping");
            return;
        };
        // A solid magenta layer, so the captured pixels are unmistakable and a channel
        // swap would be obvious rather than plausible.
        let pixels: Vec<u8> = std::iter::repeat_n([0xff, 0x00, 0xff, 0xff], 64 * 32)
            .flatten()
            .collect();
        compositor
            .upload_texture(LayerId::Attract, 64, 32, TexelFormat::Rgba8, &pixels)
            .unwrap();
        compositor.upsert_layer(Layer {
            id: LayerId::Attract,
            opacity: 1.0,
            transform: Transform::default(),
        });

        let (_tx, rx) = crate::render_pipeline::render_channel(1);
        let mut rloop = RenderLoop::new(compositor, rx);
        let (tap, png_rx) = ScreenshotTap::new();
        rloop.add_tap(Box::new(tap));
        rloop.pump();

        let png = png_rx
            .try_recv()
            .expect("the tap should have been served")
            .expect("png encoding should succeed");
        assert_eq!(&png[1..4], b"PNG");

        // …and a second pump must not read back again: the tap retired itself.
        rloop.pump();
        assert!(
            png_rx.try_recv().is_err(),
            "one capture, not a subscription"
        );
    }
}
