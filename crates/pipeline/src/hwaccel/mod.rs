//! Hardware-accelerated decode: which path decode is on, and how it gives up.
//!
//! The framing question for hwaccel is not "turn on VAAPI" — it is *who owns the decoded
//! surface*. Decoding to a GPU surface and then reading it back to system memory so the
//! existing swscale → RGBA → `write_texture` path can use it trades CPU decode cycles for
//! a GPU→CPU→GPU round trip; at 4K the readback usually costs more than it saved. So this
//! is a zero-copy *import* project: the surface is produced on the GPU
//! ([`super::hwaccel`] backends) and sampled on the GPU (the compositor's NV12 pipeline),
//! and never becomes bytes in between.
//!
//! ## Why the choice is made at runtime
//!
//! The `hwaccel` cargo feature gates whether a backend is *compiled*, never whether it is
//! *used*. A `--features vaapi` that turns a working mirror into a black screen on the
//! wrong GPU is precisely the failure mode to avoid: hwaccel fails routinely in the field
//! — unsupported profile, too many reference frames, 10-bit on an older card, a VM with
//! no render node at all — and it fails *mid-session* as often as at startup. So every
//! give-up is recoverable and lands on software decode without dropping the mirror, and
//! every one of them is logged, because a silent downgrade to software is a performance
//! bug nobody can see.
//!
//! [`FallbackPolicy`] is that decision, as a pure state machine over metadata: no ffmpeg,
//! no GPU, unit-tested in every build (ground rule 6). The platform backends live in the
//! sibling modules and only compile with the `hwaccel` feature.

use tracing::{info, warn};

#[cfg(feature = "render")]
mod import;
#[cfg(feature = "render")]
pub use import::{import_capability, mark_import_broken, GpuImporter, SurfaceImport};

#[cfg(feature = "hwaccel")]
pub mod export;
#[cfg(feature = "hwaccel")]
pub mod ffmpeg_hw;
#[cfg(feature = "hwaccel")]
pub use export::SurfaceExporter;

// The browser frame path (D36): getting a GPU handle out of the browser process is the
// same problem on both platforms, so it is one module with a `cfg` pair inside rather
// than a per-platform module like the decode backends below.
#[cfg(feature = "hwaccel")]
pub mod remote_handle;

/// What a browser frame *is*, independent of how the platform hands it over.
///
/// Exists so the two `import_single_plane` entry points agree on their common half. What
/// differs between them is only the platform's description of *where the pixels are* — a
/// DRM modifier and plane layout on Linux, an NT handle on Windows — and keeping the
/// shared half in one type makes that the only difference.
#[cfg(feature = "render")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameGeometry {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Channel order as the producer wrote it. Browser output is BGRA on both platforms
    /// in practice, but reading it from the producer rather than assuming is what keeps a
    /// silent R/B swap from being possible.
    pub format: wgpu::TextureFormat,
}

#[cfg(feature = "render")]
impl FrameGeometry {
    /// Reject anything the single-plane import cannot describe.
    ///
    /// # Errors
    /// [`crate::error::PipelineError::GpuImport`] for a non-RGBA-family format or a
    /// degenerate size — both of which import *successfully* if waved through and then
    /// render garbage.
    pub fn validate(self) -> Result<Self, crate::error::PipelineError> {
        if !matches!(
            self.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
        ) {
            return Err(crate::error::PipelineError::GpuImport(format!(
                "single-plane import supports BGRA8/RGBA8, not {:?}",
                self.format
            )));
        }
        if self.width == 0 || self.height == 0 {
            return Err(crate::error::PipelineError::GpuImport(format!(
                "browser frame has unusable dimensions {}x{}",
                self.width, self.height
            )));
        }
        Ok(self)
    }

    /// The `wgpu` extent for this frame.
    #[must_use]
    pub const fn extent(self) -> wgpu::Extent3d {
        wgpu::Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        }
    }
}

#[cfg(all(feature = "hwaccel", unix))]
pub mod dmabuf;
#[cfg(all(feature = "hwaccel", unix))]
pub mod vaapi;
#[cfg(all(feature = "hwaccel", unix))]
pub mod vulkan_import;

#[cfg(all(feature = "hwaccel", windows))]
pub mod d3d11va;
#[cfg(all(feature = "hwaccel", windows))]
pub mod dx12_import;

/// A hardware decode family this build knows how to drive.
///
/// Deliberately a closed enum over *families*, not over ffmpeg's `AVHWDeviceType`: a
/// `match` here has to stay exhaustive when a third platform lands (ground rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HwBackendKind {
    /// VA-API decode, exported as DMA-BUF and imported into Vulkan. Linux.
    Vaapi,
    /// D3D11VA decode, copied into a shared NV12 texture and opened by D3D12. Windows.
    D3d11Va,
}

impl HwBackendKind {
    /// A short stable identifier for logs.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Vaapi => "vaapi",
            Self::D3d11Va => "d3d11va",
        }
    }

    /// The backend this platform would use, if any is compiled in.
    ///
    /// This is a *build* fact, not an availability check — whether the device actually
    /// opens is discovered by trying, which is the only honest way to find out.
    #[must_use]
    pub const fn for_this_platform() -> Option<Self> {
        #[cfg(all(feature = "hwaccel", unix))]
        {
            Some(Self::Vaapi)
        }
        #[cfg(all(feature = "hwaccel", windows))]
        {
            Some(Self::D3d11Va)
        }
        #[cfg(not(all(feature = "hwaccel", any(unix, windows))))]
        {
            None
        }
    }
}

impl std::fmt::Display for HwBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// What the operator asked for. The default is [`Self::Auto`]; the other two exist so a
/// regression can be pinned down without recompiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwPreference {
    /// Try hardware, fall back to software on any give-up. What ships.
    #[default]
    Auto,
    /// Never attempt hardware.
    SoftwareOnly,
    /// Attempt hardware and never fall back — a give-up is a hard decode error.
    ///
    /// This is a *diagnostic* mode. Without it a broken hwaccel path is invisible:
    /// everything still plays, just on the CPU, and the only symptom is a fan.
    HardwareOnly,
}

/// Where decode is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodePath {
    /// Frames come out as GPU surfaces from this backend.
    Hardware(HwBackendKind),
    /// Frames come out as packed RGBA in system memory.
    Software,
}

/// Why a hardware decode attempt gave up.
///
/// The variants are the real failure modes, each carrying enough to make the log line
/// actionable — "hwaccel fell back" with no reason is not a diagnosis.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HwGiveUp {
    /// No backend is compiled for this platform/feature set.
    NotCompiled,
    /// The hardware device could not be opened: no render node, no driver, a VM.
    DeviceUnavailable(String),
    /// The decoder exists but has no hardware config for this codec/profile.
    CodecUnsupported(HwBackendKind),
    /// `get_format` was offered no hardware pixel format — the usual shape of "this
    /// profile, bit depth, or reference-frame count is beyond the fixed-function block".
    FormatRejected(HwBackendKind),
    /// The hardware decoder opened but then failed while decoding.
    DecodeFailed(String),
    /// A decoded surface could not be exported to a shareable handle. Often transient
    /// (a pool exhausted for a frame), which is why it gets a budget rather than an
    /// immediate fallback.
    ExportFailed(String),
}

impl HwGiveUp {
    /// Whether this failure is worth tolerating for a few frames before giving up on
    /// hardware entirely. Setup failures are permanent by nature; a failed export of one
    /// surface usually is not.
    const fn is_transient(&self) -> bool {
        matches!(self, Self::ExportFailed(_))
    }
}

impl std::fmt::Display for HwGiveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCompiled => f.write_str("no hardware decode backend compiled for this build"),
            Self::DeviceUnavailable(why) => write!(f, "hardware device unavailable: {why}"),
            Self::CodecUnsupported(kind) => write!(f, "{kind} has no config for this codec"),
            Self::FormatRejected(kind) => {
                write!(f, "{kind} offered no hardware pixel format for this stream")
            }
            Self::DecodeFailed(why) => write!(f, "hardware decode failed: {why}"),
            Self::ExportFailed(why) => write!(f, "surface export failed: {why}"),
        }
    }
}

/// How many transient export failures to absorb before concluding that hardware is not
/// working. Small on purpose: a mirror that drops every third frame is worse than one
/// decoded on the CPU.
const TRANSIENT_BUDGET: u32 = 3;

/// The hw/sw decision for one decode session.
///
/// Once it falls back it stays fallen back for the rest of the session. Re-probing a
/// device that just refused a profile means re-paying the setup cost on every keyframe
/// and stuttering while it happens; the stream's parameters are what failed, and those do
/// not usually improve mid-session.
#[derive(Debug)]
pub struct FallbackPolicy {
    preference: HwPreference,
    path: DecodePath,
    transient_seen: u32,
    /// Set once we have logged the fallback, so a per-frame failure cannot become a
    /// per-frame log line.
    announced: bool,
}

/// What the decode loop should do about a give-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction {
    /// Drop this frame and stay on hardware — the failure was within budget.
    DropFrame,
    /// Rebuild the decoder in software and carry on. The mirror survives.
    FallBackToSoftware,
    /// Surface the failure to the caller: the operator asked for hardware only.
    Fail(HwGiveUp),
}

impl FallbackPolicy {
    /// Start a session with the given preference, on the backend this build has.
    #[must_use]
    pub fn new(preference: HwPreference) -> Self {
        let path = match (preference, HwBackendKind::for_this_platform()) {
            (HwPreference::SoftwareOnly, _) | (_, None) => DecodePath::Software,
            (_, Some(kind)) => DecodePath::Hardware(kind),
        };
        Self {
            preference,
            path,
            transient_seen: 0,
            announced: false,
        }
    }

    /// The path decode should currently take.
    #[must_use]
    pub const fn path(&self) -> DecodePath {
        self.path
    }

    /// Whether the decode loop should attempt a hardware decoder right now.
    #[must_use]
    pub const fn wants_hardware(&self) -> bool {
        matches!(self.path, DecodePath::Hardware(_))
    }

    /// Record a hardware give-up and decide what happens next. Logs the transition —
    /// exactly once per session for the fallback itself, and at debug volume for the
    /// tolerated ones, so a live mirror cannot turn the log into a firehose.
    pub fn give_up(&mut self, reason: HwGiveUp) -> Reaction {
        if self.preference == HwPreference::HardwareOnly {
            warn!(%reason, "hardware decode failed and HardwareOnly forbids falling back");
            return Reaction::Fail(reason);
        }
        if matches!(self.path, DecodePath::Software) {
            // Already software; nothing to give up on. Shouldn't happen, but a stray
            // report must not re-log or re-decide.
            return Reaction::FallBackToSoftware;
        }

        if reason.is_transient() && self.transient_seen < TRANSIENT_BUDGET {
            self.transient_seen += 1;
            tracing::debug!(
                %reason,
                seen = self.transient_seen,
                budget = TRANSIENT_BUDGET,
                "hardware decode hiccup; dropping the frame and staying on hardware",
            );
            return Reaction::DropFrame;
        }

        self.path = DecodePath::Software;
        if !self.announced {
            self.announced = true;
            // The whole point of the exercise: a silent downgrade to software is a
            // performance regression with no symptom but heat. Say it once, loudly.
            warn!(
                %reason,
                "falling back to SOFTWARE decode for the rest of this session",
            );
        }
        Reaction::FallBackToSoftware
    }

    /// Announce that hardware decode is actually running. Called once the decoder has
    /// opened and produced its first surface, not when it was merely attempted.
    pub fn confirm_hardware(&self, kind: HwBackendKind) {
        info!(backend = %kind, "hardware decode active (zero-copy GPU surfaces)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy that believes hardware is available, regardless of what this build
    /// actually compiled — the state machine is what's under test, not the platform.
    fn hw_policy(preference: HwPreference) -> FallbackPolicy {
        FallbackPolicy {
            preference,
            path: DecodePath::Hardware(HwBackendKind::Vaapi),
            transient_seen: 0,
            announced: false,
        }
    }

    #[test]
    fn software_only_never_starts_on_hardware() {
        let p = FallbackPolicy::new(HwPreference::SoftwareOnly);
        assert_eq!(p.path(), DecodePath::Software);
        assert!(!p.wants_hardware());
    }

    #[test]
    fn a_setup_failure_falls_back_immediately() {
        // No budget for these: if the device won't open or the codec has no hw config,
        // it will not open on the next frame either.
        let mut p = hw_policy(HwPreference::Auto);
        assert_eq!(
            p.give_up(HwGiveUp::DeviceUnavailable("no /dev/dri".into())),
            Reaction::FallBackToSoftware,
        );
        assert_eq!(p.path(), DecodePath::Software);
        assert!(!p.wants_hardware());
    }

    #[test]
    fn a_rejected_format_falls_back_immediately() {
        let mut p = hw_policy(HwPreference::Auto);
        assert_eq!(
            p.give_up(HwGiveUp::FormatRejected(HwBackendKind::Vaapi)),
            Reaction::FallBackToSoftware,
        );
        assert_eq!(p.path(), DecodePath::Software);
    }

    #[test]
    fn transient_export_failures_are_tolerated_then_are_not() {
        // One surface that won't export is a dropped frame on a live mirror — invisible.
        // A steady drip of them is worse than software decode, so the budget is small
        // and, once spent, the decision is permanent.
        let mut p = hw_policy(HwPreference::Auto);
        for i in 0..TRANSIENT_BUDGET {
            assert_eq!(
                p.give_up(HwGiveUp::ExportFailed(format!("pool empty {i}"))),
                Reaction::DropFrame,
                "failure {i} should have been absorbed",
            );
            assert!(p.wants_hardware());
        }
        assert_eq!(
            p.give_up(HwGiveUp::ExportFailed("pool empty again".into())),
            Reaction::FallBackToSoftware,
        );
        assert!(!p.wants_hardware());
    }

    #[test]
    fn fallback_is_permanent_for_the_session() {
        // Re-probing on every keyframe would re-pay the setup cost and stutter while it
        // happens, and the stream parameters that were refused have not changed.
        let mut p = hw_policy(HwPreference::Auto);
        p.give_up(HwGiveUp::DecodeFailed("profile".into()));
        assert_eq!(p.path(), DecodePath::Software);
        assert_eq!(
            p.give_up(HwGiveUp::ExportFailed("late report".into())),
            Reaction::FallBackToSoftware,
            "a straggling failure must not resurrect hardware or re-decide",
        );
        assert_eq!(p.path(), DecodePath::Software);
    }

    #[test]
    fn hardware_only_refuses_to_fall_back() {
        // The diagnostic mode: a broken hwaccel path is otherwise invisible, because
        // everything still plays — just on the CPU.
        let mut p = hw_policy(HwPreference::HardwareOnly);
        let reason = HwGiveUp::DeviceUnavailable("no render node".into());
        assert_eq!(p.give_up(reason.clone()), Reaction::Fail(reason));
        assert!(
            p.wants_hardware(),
            "HardwareOnly must not silently switch paths",
        );
    }

    #[test]
    fn hardware_only_does_not_absorb_transient_failures_either() {
        // Tolerating drops here would hide exactly what the mode exists to expose.
        let mut p = hw_policy(HwPreference::HardwareOnly);
        assert!(matches!(
            p.give_up(HwGiveUp::ExportFailed("pool".into())),
            Reaction::Fail(_),
        ));
    }

    #[test]
    fn a_build_with_no_backend_starts_in_software() {
        // Feature off, or a platform with no backend: `Auto` must still produce a
        // working decoder rather than an error.
        if HwBackendKind::for_this_platform().is_none() {
            let p = FallbackPolicy::new(HwPreference::Auto);
            assert_eq!(p.path(), DecodePath::Software);
        }
    }

    #[test]
    fn give_up_reasons_say_something_useful() {
        // These strings are the whole diagnostic surface when a mirror is slow on a box
        // nobody can attach a debugger to.
        let rendered = HwGiveUp::DeviceUnavailable("no /dev/dri/renderD128".into()).to_string();
        assert!(rendered.contains("no /dev/dri/renderD128"), "{rendered}");
        let rendered = HwGiveUp::FormatRejected(HwBackendKind::D3d11Va).to_string();
        assert!(rendered.contains("d3d11va"), "{rendered}");
    }
}
