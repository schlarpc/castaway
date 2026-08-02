//! What a test that needs a GPU does when there isn't one (#98).
//!
//! Two dozen tests in this workspace open an offscreen compositor and, failing that,
//! `eprintln!` a skip and return. Each one was written on a box with a GPU, where the
//! skip is unreachable; the sandbox `nix flake check` builds in has no render node, so
//! there the skip is the *only* path — and a skipped test reports `ok`. Measured on the
//! development box by pointing the Vulkan loader at nothing: the same file that renders
//! for 0.47 s passes in 0.013 s having composited not one pixel.
//!
//! That is the failure shape #98 is about, arriving one level down. The gate is not the
//! only thing that has to exist; what the gate runs has to actually run.
//!
//! So the skip becomes a promise a build can make. `nix flake check`'s `render-pixels`
//! check puts Mesa's lavapipe — a software Vulkan ICD, no hardware, no display — in the
//! sandbox and sets [`REQUIRE_GPU`]. With it set, "no GPU" is a failure rather than a
//! skip, so a mesa bump or an ICD path that stops resolving goes red instead of going
//! quietly back to proving nothing. Off a CI box, where a developer may genuinely have no
//! usable adapter, it still skips.
//!
//! This lives in the library rather than beside the tests because the tests that need it
//! are in two crates (`pipeline` and `app`), and a `tests/` module cannot be shared across
//! that boundary. Nothing in production calls any of it — the assertion below is not a
//! panic on a runtime-reachable path (ground rule 7).

use crate::error::PipelineError;
use crate::render_pipeline::{RenderLoop, RenderRx};
use crate::wgpu_compositor::WgpuCompositor;

/// The environment variable by which a build promises that a GPU is present.
///
/// Set it in any harness that has supplied an adapter. Its value is not read — only
/// whether it is there.
pub const REQUIRE_GPU: &str = "CASTAWAY_REQUIRE_GPU";

/// Turn "the GPU could not be opened" into either a skip or a failure, per [`REQUIRE_GPU`].
fn resolve<T>(what: &str, opened: Result<T, PipelineError>) -> Option<T> {
    match opened {
        Ok(open) => Some(open),
        Err(e) => {
            assert!(
                std::env::var_os(REQUIRE_GPU).is_none(),
                "{REQUIRE_GPU} is set, so this build promised an adapter, and {what} \
                 failed anyway: {e}. Either the software Vulkan ICD is no longer reachable \
                 (VK_DRIVER_FILES) or the backend selection moved; skipping here would \
                 mean the check goes green having rendered nothing."
            );
            eprintln!("skipping: no usable GPU ({e})");
            None
        }
    }
}

/// An offscreen render loop for a test, or `None` where there is honestly no GPU.
///
/// # Panics
/// If [`REQUIRE_GPU`] is set and the adapter still cannot be opened.
pub fn render_loop(width: u32, height: u32, rx: RenderRx) -> Option<RenderLoop> {
    resolve(
        "opening an offscreen render loop",
        RenderLoop::offscreen(width, height, rx),
    )
}

/// An offscreen compositor for a test, or `None` where there is honestly no GPU.
///
/// # Panics
/// If [`REQUIRE_GPU`] is set and the adapter still cannot be opened.
pub fn compositor(width: u32, height: u32) -> Option<WgpuCompositor> {
    resolve(
        "opening an offscreen compositor",
        WgpuCompositor::new_offscreen(width, height),
    )
}
