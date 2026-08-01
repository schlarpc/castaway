//! The handful of things every path that reaches raw libav needs.
//!
//! `ffmpeg-next` wraps a lot, but not the hardware contexts the decoder needs
//! ([`crate::hwaccel::ffmpeg_hw`]) and not encoder setup ([`crate::stream::encoder`]), so
//! both go through `ffmpeg-sys-next`. What they share is small and lives here rather than
//! in whichever of them happened to be written first — which is how the encoder ended up
//! reaching into a module gated on the *decoder's* feature flag.
#![allow(unsafe_code)]

use std::ffi::{c_int, CString};

use ffmpeg_sys_next as sys;

/// Render a libav return code as something a log line can carry.
#[must_use]
pub fn av_error(code: c_int) -> String {
    let mut buf = [0_i8; sys::AV_ERROR_MAX_STRING_SIZE];
    // SAFETY: `av_strerror` writes at most `buf.len()` bytes including the NUL into the
    // buffer we own, and returns <0 without writing if the code is unknown.
    let ok = unsafe { sys::av_strerror(code, buf.as_mut_ptr(), buf.len()) } == 0;
    if !ok {
        return format!("error {code}");
    }
    // SAFETY: `av_strerror` NUL-terminates on success, and the buffer outlives the slice.
    let bytes = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    bytes.to_string_lossy().into_owned()
}

/// Set a private option on a libav object, reporting only whether it took.
///
/// Encoder tuning is a bag of vendor-specific names — `zerolatency` is libx264's,
/// `p1`/`ll` are NVENC's, `usage` is AMF's — and asking for one an encoder has never
/// heard of is *not* an error: it is how you find out which encoder you got. So this
/// answers `bool` and the caller logs at debug, rather than every candidate open failing
/// on the first option that did not apply.
///
/// # Safety
/// `obj` must point at a live struct whose first field is an `AVClass*` — an
/// `AVCodecContext`, or the `priv_data` of one.
#[must_use]
pub unsafe fn try_set_opt(obj: *mut std::ffi::c_void, name: &str, value: &str) -> bool {
    let (Ok(name), Ok(value)) = (CString::new(name), CString::new(value)) else {
        return false;
    };
    // SAFETY: caller guarantees an AVClass-prefixed object; both strings are NUL
    // terminated and outlive the call, which copies whatever it uses.
    unsafe { sys::av_opt_set(obj, name.as_ptr(), value.as_ptr(), 0) == 0 }
}
