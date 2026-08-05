//! What a test that needs ffmpeg does when there isn't any (#182).
//!
//! The sibling of [`crate::test_gpu`], and it exists because the same failure happened
//! again one dependency over. A dozen tests in this workspace shell out to the `ffmpeg`
//! binary to make a clip, or ask libav for an encoder, and on failure `eprintln!` a skip
//! and return. A skipped test reports `ok`.
//!
//! That is not hypothetical here. `checks.audio` — the check whose entire reason for
//! existing is that a missing decoder is the one failure that is *silent*, since a
//! receiver with no decoder pairs, streams and plays nothing — listed `pkgs.ffmpeg_7` in
//! `buildInputs` only. `commonArgs` sets `strictDeps = true`, so the libraries were there
//! and the **binary** was not, and every test that needed a clip skipped. The check was
//! green from the day it landed until 2026-08-05 having decoded, in those tests, nothing.
//!
//! So the skip becomes a promise a build can make, exactly as [`crate::test_gpu`] does for
//! an adapter. A harness that has supplied ffmpeg sets [`REQUIRE_FFMPEG`]; with it set,
//! "no ffmpeg" is a failure rather than a skip. Off a CI box, where a developer may
//! genuinely be outside the devShell, it still skips.
//!
//! Ungated, unlike `test_gpu`: the checks that need it (`audio`) do not turn on `render`,
//! and a `pub const` plus one function costs a build nothing. Nothing in production calls
//! any of it — the assertion is not on a runtime-reachable path (ground rule 7).

/// The environment variable by which a build promises that ffmpeg is present.
///
/// Set it in any harness that has put the binary on `PATH` and the codecs in the library.
/// Its value is not read — only whether it is there.
pub const REQUIRE_FFMPEG: &str = "CASTAWAY_REQUIRE_FFMPEG";

/// Turn "ffmpeg could not do this" into either a skip or a failure, per [`REQUIRE_FFMPEG`].
///
/// `what` names the thing that could not be produced, so a failure says which capability
/// is missing rather than only that one is.
#[must_use]
pub fn resolve<T>(what: &str, produced: Option<T>) -> Option<T> {
    if produced.is_none() {
        assert!(
            std::env::var_os(REQUIRE_FFMPEG).is_none(),
            "{REQUIRE_FFMPEG} is set, so this build promised ffmpeg, and {what} failed \
             anyway. Either the binary left `PATH` (`strictDeps` puts `buildInputs` out of \
             reach — it must be in `nativeBuildInputs`) or the ffmpeg build lost a codec; \
             skipping here would mean the check goes green having decoded nothing."
        );
        eprintln!("skipping: ffmpeg unavailable for {what}");
    }
    produced
}

/// The `bool` form, for call sites that test a capability rather than produce a value.
#[must_use]
pub fn available(what: &str, ok: bool) -> bool {
    resolve(what, ok.then_some(())).is_some()
}
