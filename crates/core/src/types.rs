//! Value types shared across the receiver. Parse-don't-validate lives here: wire
//! bytes become rich types at the boundary and flow inward (ground rule 1).

use std::fmt;
use std::num::{NonZeroU16, NonZeroU32};

use tokio::sync::mpsc;

use crate::error::CoreError;

/// A casting protocol family. Used to tag sources and pick advertisement records.
///
/// Modeled as a closed enum so a `match` over protocols is exhaustive — adding a
/// protocol forces every dispatch site to be updated (ground rule 1). Deliberately
/// *not* `#[non_exhaustive]`: that attribute would force downstream crates to carry
/// `_` arms, which is exactly the escape hatch the network-surface registry
/// (`crates/app/src/surface.rs`) must not have — a new protocol has to fail to
/// compile until its network surface is declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Bluetooth A2DP sink (+ AVRCP). Audio-only, and the only source that needs no LAN.
    Bluetooth,
    /// GameStream / Sunshine client (Moonlight role) — the one protocol where the
    /// panel dials out to a host instead of being discovered by a sender.
    GameStream,
    /// Matter Casting — the Casting Video Player role. The one protocol where the panel
    /// is the *commissioner*: a phone joins a fabric we administer before it can speak.
    MatterCast,
    /// FCast — FUTO's open casting protocol (the cast button in Grayjay). A
    /// media-URL protocol in the DLNA shape: length-prefixed JSON over one TCP
    /// session, and the receiver fetches the media itself.
    FCast,
}

impl ProtocolKind {
    /// Every protocol, in the order the network-surface registry documents them.
    ///
    /// Adding a variant fails the exhaustive matches over this enum first (the
    /// registry's among them, `crates/app/src/surface.rs`); update this list in the
    /// same change — `all_lists_every_variant` below holds you to the count.
    pub const ALL: [Self; 10] = [
        Self::AirPlay,
        Self::Cast,
        Self::Miracast,
        Self::Dlna,
        Self::YouTubeLounge,
        Self::Spotify,
        Self::Bluetooth,
        Self::GameStream,
        Self::MatterCast,
        Self::FCast,
    ];

    /// A short, stable, lowercase identifier used in logs and source ids.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            ProtocolKind::AirPlay => "airplay",
            ProtocolKind::Cast => "cast",
            ProtocolKind::Miracast => "miracast",
            ProtocolKind::Dlna => "dlna",
            // "youtube", not "youtube-lounge": the Lounge protocol is how we get there,
            // but the name is what a person reads in a picker and on the idle screen.
            ProtocolKind::YouTubeLounge => "youtube",
            ProtocolKind::Spotify => "spotify",
            ProtocolKind::Bluetooth => "bluetooth",
            ProtocolKind::GameStream => "gamestream",
            // "matter", not "matter-cast": the panel speaks one Matter role and the
            // shorter word is what a person reads in a picker.
            ProtocolKind::MatterCast => "matter",
            ProtocolKind::FCast => "fcast",
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

/// What castaway calls itself to an HTTP server when fetching media.
///
/// The UPnP shape — `OS/version UPnP/1.0 product/version` — because a DLNA server may use
/// it to decide what it will serve, and several are known to serve a reduced set to a
/// client they cannot place.
///
/// It lives here, next to [`MediaUri`], rather than beside the fetch that sends it,
/// because more than one crate now talks to a media server about the same resource: the
/// decoder fetches it, and `proto-dlna` HEADs it first to decide whether to accept the
/// item at all (#99). A server that varies its answer by client — and varying is the
/// reason to send this at all — must be asked both questions as the same client, or the
/// probe describes a resource that is not the one we would go on to fetch.
pub const MEDIA_USER_AGENT: &str =
    concat!("Linux/1.0 UPnP/1.0 castaway/", env!("CARGO_PKG_VERSION"));

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

    /// The whole URI as a string slice — what a fetcher hands its library.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for MediaUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// One HTTP request header a sender asked us to send when fetching its media.
///
/// A validated type rather than a `(String, String)` because the thing it feeds is a
/// **CRLF-joined blob**: libavformat's `headers` option, and every other HTTP client's
/// header block, is one string with `\r\n` between entries. A value carrying its own
/// `\r\n` therefore injects whole headers — or a request body — into a fetch aimed at
/// somebody else's server, and the only place that can be caught once and for all is
/// where wire bytes become a type (ground rule 1).
///
/// The grammar is RFC 9110's: a name is one or more `tchar`, a value is visible ASCII
/// plus space and horizontal tab. Nothing here canonicalises case — a server that cares
/// gets what the sender wrote — but [`RequestHeader::is`] compares case-insensitively,
/// which is what the field-name rules actually say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHeader {
    name: String,
    value: String,
}

impl RequestHeader {
    /// Parse one header from what a sender sent.
    ///
    /// # Errors
    /// [`CoreError::InvalidHeader`] for an empty or non-token name, or a value holding a
    /// control character — `\r` and `\n` above all.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Result<Self, CoreError> {
        let name = name.into();
        let value = value.into();
        if name.is_empty() || !name.bytes().all(is_tchar) {
            return Err(CoreError::InvalidHeader(format!(
                "{name:?} is not a header name"
            )));
        }
        // Leading/trailing whitespace is not part of a field value (RFC 9110 §5.5), and
        // a sender that padded one should not have that padding sent back out.
        let value = value.trim_matches([' ', '\t']).to_owned();
        if !value.bytes().all(is_field_vchar) {
            return Err(CoreError::InvalidHeader(format!(
                "the value of {name} holds a control character"
            )));
        }
        Ok(Self { name, value })
    }

    /// The field name, as the sender spelled it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The field value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether this is the named header, compared the way field names actually compare.
    #[must_use]
    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

impl fmt::Display for RequestHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.value)
    }
}

/// RFC 9110 `tchar`: what a field name may be built from.
const fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// RFC 9110 field-value byte: visible ASCII, space, tab, or obs-text (0x80..=0xFF).
const fn is_field_vchar(b: u8) -> bool {
    matches!(b, b'\t' | b' ' | 0x21..=0x7e | 0x80..=0xff)
}

/// A media fetch: what to open, and what to say while opening it.
///
/// The seam issue #251 asked for, and deliberately **not** FCast-shaped even though
/// FCast is its only current customer: a sender that knows the media is behind an
/// `Authorization` header has told us how to fetch it, and before this the pipeline went
/// out bare and the load failed honestly but pointlessly. DLNA and Cast simply carry no
/// headers, so they build one of these from a bare [`MediaUri`] and nothing about them
/// changes.
///
/// Headers travel *with the URI* rather than as a pipeline setting because they are a
/// property of the one fetch: a queue whose second item is on another host with another
/// token must not inherit the first item's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRequest {
    uri: MediaUri,
    headers: Vec<RequestHeader>,
}

impl MediaRequest {
    /// A bare fetch — the URI and nothing else. What every protocol that carries no
    /// headers emits.
    #[must_use]
    pub const fn new(uri: MediaUri) -> Self {
        Self {
            uri,
            headers: Vec::new(),
        }
    }

    /// A fetch carrying request headers.
    #[must_use]
    pub const fn with_headers(uri: MediaUri, headers: Vec<RequestHeader>) -> Self {
        Self { uri, headers }
    }

    /// What to open.
    #[must_use]
    pub const fn uri(&self) -> &MediaUri {
        &self.uri
    }

    /// What to say while opening it.
    #[must_use]
    pub fn headers(&self) -> &[RequestHeader] {
        &self.headers
    }

    /// The value of one header, if the sender set it.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.is(name))
            .map(RequestHeader::value)
    }

    /// Every header *except* the named one, in order.
    ///
    /// For the fetcher that has to lift one out and pass it separately: libavformat wants
    /// `User-Agent` as its own option, and a build that put it in the blob as well sent
    /// the field twice.
    pub fn headers_except<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a RequestHeader> + 'a {
        self.headers.iter().filter(move |h| !h.is(name))
    }

    /// Drop the headers and keep the URI.
    #[must_use]
    pub fn into_uri(self) -> MediaUri {
        self.uri
    }
}

impl From<MediaUri> for MediaRequest {
    fn from(uri: MediaUri) -> Self {
        Self::new(uri)
    }
}

impl fmt::Display for MediaRequest {
    /// The URI alone. These strings reach logs, and a sender's `Authorization: Bearer …`
    /// is precisely the thing that must not be written to a file on a wall panel.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.uri, f)
    }
}

/// Video codecs the mirroring/decode path understands.
///
/// Deliberately **not** `#[non_exhaustive]`, same reasoning as [`ProtocolKind`]: every
/// consumer is a sibling crate that recompiles with core, so the attribute buys no
/// compatibility and costs exhaustiveness — it forces the `_` arms that let a new codec
/// slip through a framing or decode decision silently (#213). Adding a variant must
/// fail to compile at every site that has to consider it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 / AVC (AirPlay mirror, Cast, Miracast).
    H264,
    /// H.265 / HEVC (newer AirPlay).
    Hevc,
    /// VP8 (Cast mirroring — Chrome offers it alongside H.264 and a sender may only
    /// have it).
    Vp8,
    /// VP9 (Cast mirroring — Chrome 148+ offers it above VP8; hardware decodes it
    /// nearly everywhere VP8 is software-only).
    Vp9,
}

/// Audio codecs the decode path understands.
///
/// Not `#[non_exhaustive]` — see [`VideoCodec`] for why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// Apple Lossless (AirPlay 1 RAOP).
    Alac,
    /// AAC / AAC-ELD (AirPlay 2, and the A2DP codec every iPhone offers).
    Aac,
    /// Opus.
    Opus,
    /// Raw PCM.
    Pcm,
    /// Low-complexity subband coding — A2DP's mandatory baseline, so every sender has it.
    Sbc,
    /// Qualcomm aptX (A2DP vendor codec `0x004F/0x0001`).
    AptX,
    /// Qualcomm aptX HD (A2DP vendor codec `0x004F/0x0024`).
    AptXHd,
    /// Sony LDAC (A2DP vendor codec `0x012D/0x00AA`). The only one libav cannot decode.
    Ldac,
}

/// The shape of an audio stream, as the sender's negotiation settled it.
///
/// Deliberately has **no `Default`**. aptX and aptX HD carry no in-band configuration at
/// all, so a decoder handed the wrong rate plays the stream at the wrong pitch with
/// nothing in any log to say so — which is exactly what a defaultable format invites
/// (#70). The only way to obtain one is to state both fields, so the
/// negotiated values have to be carried from the protocol to the decoder rather than
/// re-guessed at the far end.
///
/// Both fields are non-zero because neither zero is a stream: a zero rate divides by zero
/// in every duration calculation and zero channels has no samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    sample_rate: NonZeroU32,
    channels: NonZeroU16,
}

impl AudioFormat {
    /// A format from already-checked parts.
    #[must_use]
    pub const fn new(sample_rate: NonZeroU32, channels: NonZeroU16) -> Self {
        Self {
            sample_rate,
            channels,
        }
    }

    /// A format from raw wire values, or `None` if either is zero.
    ///
    /// The boundary constructor: protocol code parses a rate out of a capability and
    /// converts here, so a nonsense negotiation is refused where the peer can still be
    /// told rather than several layers later inside a decoder.
    #[must_use]
    pub const fn from_hz(sample_rate: u32, channels: u16) -> Option<Self> {
        match (NonZeroU32::new(sample_rate), NonZeroU16::new(channels)) {
            (Some(sample_rate), Some(channels)) => Some(Self::new(sample_rate, channels)),
            _ => None,
        }
    }

    /// Sample rate in Hz.
    #[must_use]
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate.get()
    }

    /// Channel count.
    #[must_use]
    pub const fn channels(self) -> u16 {
        self.channels.get()
    }
}

impl std::fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} Hz × {}", self.sample_rate(), self.channels())
    }
}

/// How far behind delivery a sender intends the receiver to play — its declared
/// playout latency, converted out of wire units at the protocol boundary.
///
/// AirPlay is the first source: RTSP sync packets carry the figure in sample frames at
/// the stream's own rate (77175 frames of 44.1 kHz ALAC is 1.75 s; 7497 of AAC-ELD is
/// 170 ms), and it arrives *after* the session's audio event, on the timing plane, so
/// it travels as [`crate::SessionEvent::AudioLatency`] rather than as a field of the
/// registration. A duration rather than frames because the frame count only means
/// anything against the sender's rate, and the protocol boundary is the one place that
/// knows it (parse, don't validate — ground rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeclaredLatency(std::time::Duration);

impl DeclaredLatency {
    /// From a latency counted in sample frames at `format`'s rate — the shape AirPlay
    /// sync packets carry.
    ///
    /// Exact: `format`'s rate is non-zero by construction, and `u32` frames times a
    /// nanosecond scale fits `u64` with room to spare.
    #[must_use]
    pub fn from_frames(frames: u32, format: AudioFormat) -> Self {
        let nanos =
            u64::from(frames).saturating_mul(1_000_000_000) / u64::from(format.sample_rate());
        Self(std::time::Duration::from_nanos(nanos))
    }

    /// The declared latency as a duration.
    #[must_use]
    pub const fn duration(self) -> std::time::Duration {
        self.0
    }
}

impl std::fmt::Display for DeclaredLatency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ms", self.0.as_millis())
    }
}

/// Decoded-frame pixel layout. `Decoded` frames carry one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PixelFormat {
    /// Planar YUV 4:2:0, 8-bit (typical ffmpeg decode output).
    Yuv420p,
    /// Packed BGRA, 8-bit (browser paint output).
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

/// A block of already-decoded interleaved audio.
///
/// The audio counterpart of [`DecodedFrame`], and it exists for the same reason: some
/// sources hand over samples rather than a bitstream, and re-encoding them just to run
/// them back through a decoder would be pure loss. Spotify is the first — librespot owns
/// the Vorbis decode and normalisation, so what reaches us is PCM (DECISION-LOG D30).
///
/// Samples are `f32` in `-1.0..=1.0`, interleaved by channel, because that is what the
/// output stage already speaks.
#[derive(Debug, Clone, PartialEq)]
pub struct PcmFrame {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Interleaved samples in `-1.0..=1.0`.
    pub samples: Vec<f32>,
    /// Presentation timestamp from the start of the stream.
    pub pts: std::time::Duration,
}

impl PcmFrame {
    /// How many sample frames (one per channel-group) this block holds.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    /// How long this block plays for.
    #[must_use]
    pub fn duration(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(
            (self.frame_count() as u64)
                .saturating_mul(1_000_000_000)
                .checked_div(u64::from(self.sample_rate.max(1)))
                .unwrap_or(0),
        )
    }
}

/// How an adapter delivers media to the pipeline.
///
/// The split is load-bearing for cross-platform Miracast: Linux gives us
/// [`FrameSource::Encoded`], Windows `MiracastReceiver` decodes for us and yields
/// [`FrameSource::Decoded`]. Baking this in from day one means the backend swap is a
/// new impl, not a core-trait change (ground rule 5).
///
/// [`FrameSource::Pcm`] is the audio-only member of that family: an adapter that has
/// already decoded to samples. Keeping it distinct from [`FrameSource::Decoded`] rather
/// than widening that variant means the pipeline cannot be handed audio where it expects
/// pixels, and the `match` that routes them stays exhaustive (ground rule 1).
#[derive(Debug)]
pub enum FrameSource {
    /// A URL the pipeline opens with libav itself.
    Url(MediaUri),
    /// The adapter pushes encoded frames it has already depacketized/decrypted.
    Encoded(mpsc::Receiver<EncodedFrame>),
    /// The adapter (or the OS) pushes already-decoded frames / GPU surfaces.
    Decoded(mpsc::Receiver<DecodedFrame>),
    /// The adapter pushes already-decoded audio samples. There is nothing to decode and
    /// no codec to name — see [`PcmFrame`].
    ///
    /// A **std** channel, unlike its siblings, because both ends of this one are ordinary
    /// threads: the producer is a decoder running off-runtime and the consumer is the
    /// output thread. A tokio channel here looks harmless and is not — `blocking_send`
    /// panics with "Cannot block the current thread from within a runtime" if the
    /// producer happens to sit inside *any* runtime context, which librespot's player
    /// thread does (it builds its own runtime and blocks on it). Sending on a std channel
    /// blocks the producer, which is exactly the backpressure an audio sink wants.
    Pcm(std::sync::mpsc::Receiver<PcmFrame>),
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

    /// The header a sender really sends (#251's fixture has one of these on a plain
    /// `play`) survives the boundary intact, padding trimmed.
    #[test]
    fn a_bearer_token_parses_and_keeps_its_spelling() {
        let header = RequestHeader::new("Authorization", "  Bearer abc.def  ").unwrap();
        assert_eq!(header.name(), "Authorization");
        assert_eq!(header.value(), "Bearer abc.def");
        assert!(
            header.is("authorization"),
            "field names compare ASCII-caselessly"
        );
        assert!(!header.is("authorisation"));
    }

    /// The whole reason this is a type: the block it feeds is CRLF-joined, so a value
    /// carrying its own CRLF would forge headers on a request aimed at somebody else's
    /// server. Refused where the wire becomes a type, not deeper in.
    #[test]
    fn a_value_cannot_smuggle_a_second_header() {
        assert!(matches!(
            RequestHeader::new("X-Thing", "a\r\nAuthorization: Bearer stolen"),
            Err(CoreError::InvalidHeader(_))
        ));
        assert!(RequestHeader::new("X-Thing", "a\nb").is_err());
        assert!(RequestHeader::new("X-Thing", "a\0b").is_err());
        // And a name has to be a token — `:` in one would split the field itself.
        assert!(RequestHeader::new("X: Y", "z").is_err());
        assert!(RequestHeader::new("", "z").is_err());
    }

    /// A request is a URI plus headers, and the headers belong to *that* fetch.
    #[test]
    fn a_request_carries_its_headers_and_hides_them_from_logs() {
        let uri = MediaUri::parse("https://example.com/a.mp4").unwrap();
        let request = MediaRequest::with_headers(
            uri.clone(),
            vec![
                RequestHeader::new("Authorization", "Bearer secret").unwrap(),
                RequestHeader::new("User-Agent", "Grayjay/1.0").unwrap(),
            ],
        );
        assert_eq!(request.uri(), &uri);
        assert_eq!(request.header("authorization"), Some("Bearer secret"));
        assert_eq!(request.header("Referer"), None);
        assert_eq!(
            request
                .headers_except("user-agent")
                .map(RequestHeader::name)
                .collect::<Vec<_>>(),
            ["Authorization"],
            "the fetcher lifts User-Agent out to its own option"
        );
        // The Display impl is what reaches the log file on an unattended panel.
        assert_eq!(request.to_string(), "https://example.com/a.mp4");
        assert!(!format!("{request}").contains("secret"));
    }

    /// Every protocol that carries no headers builds one of these and nothing about it
    /// changes.
    #[test]
    fn a_bare_uri_is_a_request_with_no_headers() {
        let uri = MediaUri::parse("https://example.com/a.mp4").unwrap();
        let request: MediaRequest = uri.clone().into();
        assert!(request.headers().is_empty());
        assert_eq!(request.into_uri(), uri);
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
        // "youtube", not the transport that gets us there — this string ends up in a
        // picker and on the idle screen.
        assert_eq!(ProtocolKind::YouTubeLounge.to_string(), "youtube");
    }

    #[test]
    fn all_lists_every_variant() {
        // Adding a variant makes this match non-exhaustive, which is the compile error
        // that walks you here; the count assertion is what makes forgetting ALL fail.
        let noted = |k: ProtocolKind| match k {
            ProtocolKind::AirPlay
            | ProtocolKind::Cast
            | ProtocolKind::Miracast
            | ProtocolKind::Dlna
            | ProtocolKind::YouTubeLounge
            | ProtocolKind::Spotify
            | ProtocolKind::Bluetooth
            | ProtocolKind::GameStream
            | ProtocolKind::MatterCast
            | ProtocolKind::FCast => (),
        };
        for kind in ProtocolKind::ALL {
            noted(kind);
        }
        assert_eq!(ProtocolKind::ALL.len(), 10);
        // No duplicates: every slug appears once.
        let mut slugs: Vec<_> = ProtocolKind::ALL.iter().map(|k| k.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), ProtocolKind::ALL.len());
    }
}
