//! # ldac-sys
//!
//! Raw FFI bindings to `libldacBT` — Sony's own LDAC library, as forked by open-vela and
//! built by `nix/ldacbt.nix`. This is the one A2DP codec libav cannot decode, and the
//! reason the LDAC endpoint went unadvertised for as long as it did (OPEN-QUESTIONS Q22).
//!
//! The original premise for this crate was wrong in a way worth recording: AOSP's
//! `libldac` is encoder-only, so the plan was the reverse-engineered `libldacdec` over
//! FFI. That was never necessary. The open-vela fork ships Sony's **complete** decode
//! path — `ldacBT_init_handle_decode` and `ldacBT_decode` — so what we link is the
//! reference implementation, not somebody's reconstruction of it.
//!
//! What *is* true is a narrower version of the same trap, and it is why the library is
//! built here rather than taken from nixpkgs: `pkgs.ldacbt` under this flake's pin is
//! EHfive/ldacBT built `_ENCODE_ONLY`. It installs `libldacBT_enc.so` and a header with no
//! `ldacBT_decode` in it. Linking that would have failed; generating bindings from that
//! header would have produced a file that compiles and is missing the entire decode API.
//! See nix/ldacbt.nix.
//!
//! `src/bindings.rs` is **pregenerated and checked in**, from the header the pinned build
//! installs. The `ldac-bindings` flake check regenerates it with bindgen against that same
//! header and fails on any diff, so a version bump that changes the ABI cannot land
//! silently. That check is doing real work: nothing else in the build sees a declaration
//! for these functions, so a wrong signature links cleanly and is wrong *silently*.
//! It matters, too, because the header and the implementation do not quite agree.
//! `inc/ldacBT.h` declares
//!
//! ```c
//! int ldacBT_init_handle_decode(HANDLE_LDAC_BT, int cm, int sf, int var0, int var1, int var2);
//! ```
//!
//! while `src/ldacBT_api.c` *defines* it as `(…, int cm, int sf, int nshift, int var0,
//! int var1)`. Same arity, so the ABI is the same and the header's "reserved, must be 0"
//! covers the third integer either way — but it is exactly the kind of disagreement that
//! makes writing the declaration from memory a bad idea.
//!
//! Everything here is `unsafe extern "C"`; the safe boundary is
//! `pipeline::ldac_decode`, which is the only permitted consumer (rule 8: FFI surface
//! thin, wrapped in safe types at the crate boundary).
//!
//! Unlike `moonlight-sys`, this library is **not** a singleton: all state hangs off the
//! opaque `HANDLE_LDAC_BT`, so one handle per stream is fine and two streams do not
//! contend. Nothing here is thread-safe *per handle*, which the wrapper gets for free by
//! owning it.
//!
//! ## What the decode API needs from a caller
//!
//! Three properties that are documented only in prose or not at all, and that the safe
//! wrapper exists to enforce:
//!
//! - **The input buffer needs two bytes of slack past the frame.** `read_unpack_ldac`
//!   fetches three bytes at a time (`p[0]<<16 | p[1]<<8 | p[2]`) from wherever the bit
//!   cursor sits, so reading the last byte of a frame touches two bytes past it. The
//!   header says to allocate [`LDACBT_MAX_NBYTES`]` + 2`; handing `ldacBT_decode` a bare
//!   slice pointer is a two-byte overread.
//! - **`cm` is the A2DP channel *mode*, not the channel config index.** It must be one of
//!   [`LDACBT_CHANNEL_MODE_STEREO`], [`LDACBT_CHANNEL_MODE_DUAL_CHANNEL`] or
//!   [`LDACBT_CHANNEL_MODE_MONO`] — the same 3-bit field the AVDTP capability carries —
//!   and `ldacBT_assert_cm` rejects anything else. The `LDAC_CCI_*` values are the
//!   *other* numbering and passing one here fails init.
//! - **A `-1` return does not always mean "no audio".** When the frame header disagrees
//!   with the handle's configuration, `ldacBT_decode` re-initialises itself, decodes the
//!   frame anyway, sets the error code to [`LDACBT_ERR_DEC_CONFIG_UPDATED`], and still
//!   returns failure. Treating that as a dropped frame throws away good audio at exactly
//!   the moment a sender changes sample rate.

pub mod bindings;

pub use bindings::*;

/// What `ldacBT_get_error_code` packs into the integer it returns.
///
/// Hand-written because these are function-like C macros and bindgen emits nothing for
/// them — the one part of this crate that is not generated, and therefore the one part
/// that has to be read against the header rather than trusted. From `ldacBT.h`:
///
/// ```c
/// #define LDACBT_API_ERR(err)    ((err >> 20) & 0x0FFF)
/// #define LDACBT_HANDLE_ERR(err) ((err >> 10) & 0x03FF)
/// #define LDACBT_BLOCK_ERR(err)  ( err & 0x03FF)
/// #define LDACBT_ERROR(err)      ((LDACBT_ERR_NON_FATAL) <= LDACBT_API_ERR(err) ? 1 : 0)
/// #define LDACBT_FATAL(err)      ((LDACBT_ERR_FATAL) <= LDACBT_API_ERR(err) ? 1 : 0)
/// ```
///
/// The three levels are not alternatives to pick between — a single returned code carries
/// up to all three at once, which is why they are shifts rather than an enum. The one that
/// matters to a caller is [`api_err`]: `ldacBT_get_error_code` composes its return as
/// `error_code_api << 20 | error_code`, so the API-level codes — including
/// [`LDACBT_ERR_DEC_CONFIG_UPDATED`], the one that means "reconfigured, and there is audio
/// for you anyway" — are only visible through that shift. Reading the raw integer against
/// the constants directly never matches.
pub mod err {
    /// The API-level error code: what the library's own entry point recorded.
    #[must_use]
    pub const fn api_err(code: i32) -> i32 {
        (code >> 20) & 0x0FFF
    }

    /// The handle-level error code.
    #[must_use]
    pub const fn handle_err(code: i32) -> i32 {
        (code >> 10) & 0x03FF
    }

    /// The block-level error code, from inside the codec.
    #[must_use]
    pub const fn block_err(code: i32) -> i32 {
        code & 0x03FF
    }

    /// Whether anything at all went wrong.
    #[must_use]
    pub const fn is_error(code: i32) -> bool {
        // The constants are `u32` because bindgen sees unsigned `#define`s; the codes
        // themselves are small and the comparison is against a signed return value.
        api_err(code) >= super::LDACBT_ERR_NON_FATAL.cast_signed()
    }

    /// Whether the failure is one the handle cannot continue from.
    ///
    /// The distinction is load-bearing: a non-fatal code means the frame is lost and the
    /// next one is fine, while a fatal one means the handle has to be re-initialised
    /// before it will decode anything again.
    #[must_use]
    pub const fn is_fatal(code: i32) -> bool {
        api_err(code) >= super::LDACBT_ERR_FATAL.cast_signed()
    }
}
