//! Value types shared across the receiver. Parse-don't-validate lives here: wire
//! bytes become rich types at the boundary and flow inward (ground rule 1).

use std::fmt;

use tokio::sync::mpsc;

use crate::error::CoreError;

/// A casting protocol family. Used to tag sources and pick advertisement records.
///
/// Modeled as a closed enum so a `match` over protocols is exhaustive — adding a
/// protocol forces every dispatch site to be updated (ground rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProtocolKind {
    /// Apple AirPlay (mirroring, audio, video handoff).
    AirPlay,
    /// Google Cast (CASTv2) — mirroring and media-URL.
    Cast,
    /// Miracast / Wi-Fi Display sink.
    Miracast,
    /// DLNA MediaRenderer.
    Dlna,
    /// DIAL → YouTube Lounge.
    YouTubeLounge,
    /// Spotify Connect.
    Spotify,
}

impl ProtocolKind {
    /// A short, stable, lowercase identifier used in logs and source ids.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            ProtocolKind::AirPlay => "airplay",
            ProtocolKind::Cast => "cast",
            ProtocolKind::Miracast => "miracast",
            ProtocolKind::Dlna => "dlna",
            ProtocolKind::YouTubeLounge => "youtube-lounge",
            ProtocolKind::Spotify => "spotify",
        }
    }
}

impl fmt::Display for ProtocolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// A human-facing receiver name (the "Hackerspace TV" a sender sees).
///
/// Newtype guarantees non-empty and within the 63-byte mDNS/UPnP label ceiling, so
/// downstream code never re-checks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FriendlyName(String);

impl FriendlyName {
    /// Maximum length in bytes (mDNS instance labels cap at 63 octets).
    pub const MAX_LEN: usize = 63;

    /// Parse a friendly name, enforcing non-empty and length invariants.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidName`] if empty or longer than [`Self::MAX_LEN`] bytes.
    pub fn new(s: impl Into<String>) -> Result<Self, CoreError> {
        let s = s.into();
        if s.is_empty() {
            return Err(CoreError::InvalidName("name is empty"));
        }
        if s.len() > Self::MAX_LEN {
            return Err(CoreError::InvalidName("name exceeds 63 bytes"));
        }
        Ok(Self(s))
    }

    /// Borrow the name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FriendlyName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A media source URI the pipeline will fetch and decode (Cast LOAD, DLNA, HLS…).
///
/// Wraps a parsed [`url::Url`] restricted to schemes the pipeline can actually open,
/// so an unsupported scheme is rejected at the boundary rather than deep in ffmpeg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaUri(url::Url);

impl MediaUri {
    /// Parse and validate a media URI.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidUri`] on a malformed URL or an unsupported scheme.
    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let url = url::Url::parse(s).map_err(|e| CoreError::InvalidUri(e.to_string()))?;
        match url.scheme() {
            "http" | "https" | "rtsp" | "rtp" | "udp" | "file" | "data" => Ok(Self(url)),
            other => Err(CoreError::InvalidUri(format!(
                "unsupported scheme: {other}"
            ))),
        }
    }

    /// The underlying parsed URL.
    #[must_use]
    pub fn url(&self) -> &url::Url {
        &self.0
    }

    /// The URI scheme (already validated to be one the pipeline supports).
    #[must_use]
    pub fn scheme(&self) -> &str {
        self.0.scheme()
    }
}

impl fmt::Display for MediaUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// Video codecs the mirroring/decode path understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoCodec {
    /// H.264 / AVC (AirPlay mirror, Cast, Miracast).
    H264,
    /// H.265 / HEVC (newer AirPlay).
    Hevc,
    /// VP8 (Cast mirroring — Chrome offers it alongside H.264 and a sender may only
    /// have it).
    Vp8,
}

/// Audio codecs the decode path understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioCodec {
    /// Apple Lossless (AirPlay 1 RAOP).
    Alac,
    /// AAC / AAC-ELD (AirPlay 2).
    Aac,
    /// Opus.
    Opus,
    /// Raw PCM.
    Pcm,
}

/// Decoded-frame pixel layout. `Decoded` frames carry one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PixelFormat {
    /// Planar YUV 4:2:0, 8-bit (typical ffmpeg decode output).
    Yuv420p,
    /// Packed BGRA, 8-bit (CEF OnPaint output).
    Bgra8,
    /// Packed RGBA, 8-bit.
    Rgba8,
}

/// The YUV→RGB matrix a luma/chroma surface's samples were encoded against.
///
/// Only meaningful for a surface that is still YUV — which is exactly why it hangs off
/// [`GpuSurface`] and not off [`DecodedFrame`]: a frame swscale already converted to RGBA
/// cannot carry a colorspace that means anything, so it is not given a field to get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorSpace {
    /// ITU-R BT.601 — SD, and what an unlabelled small-format stream is assumed to be.
    Bt601,
    /// ITU-R BT.709 — HD, and what every mirroring sender we care about emits.
    Bt709,
    /// ITU-R BT.2020 non-constant luminance — UHD/HDR sources.
    Bt2020Ncl,
}

/// Whether samples span the full 0..=255 code range or the studio-limited sub-range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRange {
    /// Studio/"TV" range: luma 16..=235, chroma 16..=240.
    Limited,
    /// Full/"PC" range: 0..=255.
    Full,
}

/// Everything the compositor needs to turn YUV samples into linear RGB correctly.
///
/// Getting this wrong is washed-out or oversaturated video rather than an obvious
/// failure, so it travels *with* the surface instead of being assumed downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorInfo {
    /// Which matrix the samples were encoded with.
    pub space: ColorSpace,
    /// Which code range the samples occupy.
    pub range: ColorRange,
}

impl Default for ColorInfo {
    /// BT.709 limited — what an HD mirroring sender emits when it labels nothing.
    fn default() -> Self {
        Self {
            space: ColorSpace::Bt709,
            range: ColorRange::Limited,
        }
    }
}

/// A decoded picture that never left the GPU.
///
/// `core` deliberately cannot name what is inside. The concrete surface is a platform
/// handle — a Vulkan image bound to imported DMA-BUF memory on Linux, a shared D3D
/// texture on Windows — owned by the render backend, which recovers its own type via
/// [`GpuSurface::as_any`]. Keeping the *variant* portable and the *handle* opaque is what
/// lets a Windows `MiracastReceiver` hand the pipeline a GPU surface without `core`
/// growing a `cfg` (ground rule 5).
pub trait GpuSurface: fmt::Debug + Send + Sync {
    /// Colorimetry of the samples in this surface.
    fn color(&self) -> ColorInfo;

    /// Downcast hook so the render backend can recover the concrete surface type.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Where a decoded frame's pixels actually live.
///
/// The split is the whole point of hardware decode: a `Gpu` frame is one the decoder
/// produced straight into video memory and that the compositor samples without a
/// round-trip through system memory. A `Cpu` frame is the software path, and the
/// fallback every hardware path must be able to degrade to mid-session.
#[derive(Debug, Clone)]
pub enum FrameImage {
    /// Tightly-packed pixels in system memory, in a layout the compositor uploads
    /// directly.
    Cpu {
        /// Layout of `data`.
        format: PixelFormat,
        /// One frame's worth of pixels, `width * height * bytes_per_pixel`.
        data: bytes::Bytes,
    },
    /// A surface already resident on the GPU; the compositor imports rather than uploads.
    Gpu(std::sync::Arc<dyn GpuSurface>),
}

/// A single encoded (compressed) video/audio frame handed from an adapter to the
/// pipeline. The adapter has already depacketized and decrypted; the pipeline decodes.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Video codec this frame is encoded with, if it is video.
    pub video_codec: Option<VideoCodec>,
    /// Audio codec this frame is encoded with, if it is audio.
    pub audio_codec: Option<AudioCodec>,
    /// Presentation timestamp, in the source's timebase (nanoseconds since stream start).
    pub pts: std::time::Duration,
    /// Whether this frame is an IDR/keyframe (video) — lets the pipeline drop until sync.
    pub keyframe: bool,
    /// The compressed payload (NALUs, TS packets already stripped, etc.).
    pub data: bytes::Bytes,
}

/// A single already-decoded frame (Windows `MiracastReceiver`, or a decoded CPU frame).
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Presentation timestamp (nanoseconds since stream start).
    pub pts: std::time::Duration,
    /// The pixels — in system memory, or already on the GPU.
    pub image: FrameImage,
}

impl DecodedFrame {
    /// Build a frame from packed pixels in system memory.
    #[must_use]
    pub fn cpu(
        width: u32,
        height: u32,
        format: PixelFormat,
        pts: std::time::Duration,
        data: bytes::Bytes,
    ) -> Self {
        Self {
            width,
            height,
            pts,
            image: FrameImage::Cpu { format, data },
        }
    }

    /// Build a frame from a surface that is already resident on the GPU.
    #[must_use]
    pub fn gpu(
        width: u32,
        height: u32,
        pts: std::time::Duration,
        surface: std::sync::Arc<dyn GpuSurface>,
    ) -> Self {
        Self {
            width,
            height,
            pts,
            image: FrameImage::Gpu(surface),
        }
    }
}

/// How an adapter delivers media to the pipeline.
///
/// The three-way split is load-bearing for cross-platform Miracast: Linux gives us
/// [`FrameSource::Encoded`], Windows `MiracastReceiver` decodes for us and yields
/// [`FrameSource::Decoded`]. Baking this in from day one means the backend swap is a
/// new impl, not a core-trait change (ground rule 5).
#[derive(Debug)]
pub enum FrameSource {
    /// A URL the pipeline opens with libav itself.
    Url(MediaUri),
    /// The adapter pushes encoded frames it has already depacketized/decrypted.
    Encoded(mpsc::Receiver<EncodedFrame>),
    /// The adapter (or the OS) pushes already-decoded frames / GPU surfaces.
    Decoded(mpsc::Receiver<DecodedFrame>),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn friendly_name_rejects_empty() {
        assert!(FriendlyName::new("").is_err());
    }

    #[test]
    fn friendly_name_rejects_overlong() {
        let long = "x".repeat(FriendlyName::MAX_LEN + 1);
        assert!(FriendlyName::new(long).is_err());
        let ok = "x".repeat(FriendlyName::MAX_LEN);
        assert!(FriendlyName::new(ok).is_ok());
    }

    #[test]
    fn media_uri_accepts_supported_schemes() {
        assert!(MediaUri::parse("https://example.com/a.mp4").is_ok());
        assert!(MediaUri::parse("rtsp://10.0.0.1/stream").is_ok());
        assert_eq!(MediaUri::parse("https://x/y").unwrap().scheme(), "https");
    }

    #[test]
    fn media_uri_rejects_unsupported_scheme() {
        assert!(MediaUri::parse("ftp://example.com/a").is_err());
        assert!(MediaUri::parse("not a url").is_err());
    }

    /// A stand-in for what `pipeline` really puts behind the trait (a DMA-BUF descriptor
    /// or a shared D3D handle) — enough to prove the seam works without a GPU.
    #[derive(Debug)]
    struct FakeSurface(u32);

    impl GpuSurface for FakeSurface {
        fn color(&self) -> ColorInfo {
            ColorInfo {
                space: ColorSpace::Bt2020Ncl,
                range: ColorRange::Full,
            }
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn a_gpu_frame_round_trips_through_the_opaque_handle() {
        // The point of the `dyn GpuSurface` seam: `core` carries the frame without
        // knowing what a DMA-BUF or a DXGI handle is, and the render backend gets its
        // own type back on the other side.
        let frame = DecodedFrame::gpu(
            1920,
            1080,
            std::time::Duration::ZERO,
            std::sync::Arc::new(FakeSurface(7)),
        );
        let FrameImage::Gpu(surface) = &frame.image else {
            panic!("expected a GPU frame");
        };
        assert_eq!(surface.color().space, ColorSpace::Bt2020Ncl);
        assert_eq!(
            surface.as_any().downcast_ref::<FakeSurface>().map(|s| s.0),
            Some(7),
        );
    }

    #[test]
    fn unlabelled_video_is_treated_as_bt709_limited() {
        // Nearly every mirroring sender emits HD and labels nothing; guessing 601 or
        // full-range here is the washed-out/oversaturated failure, so the default is
        // stated once and asserted.
        assert_eq!(
            ColorInfo::default(),
            ColorInfo {
                space: ColorSpace::Bt709,
                range: ColorRange::Limited
            }
        );
    }

    #[test]
    fn protocol_slug_is_stable() {
        assert_eq!(ProtocolKind::Cast.slug(), "cast");
        assert_eq!(ProtocolKind::YouTubeLounge.to_string(), "youtube-lounge");
    }
}
