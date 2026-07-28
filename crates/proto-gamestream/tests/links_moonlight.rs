//! Proves the linked GameStream core is really *linked* (D37).
//!
//! Everything else about `proto-gamestream` compiles and tests without the C library,
//! which is the point of the `stream` feature — but it also means a broken link is
//! invisible until the first real session, on the panel, at the worst moment. These
//! tests call into moonlight-common-c and read values only it can produce, so a
//! missing archive, a symbol-name mismatch, or an ABI drift in the checked-in bindings
//! fails here instead.
//!
//! Nothing here touches the network: these are the library's pure query functions.
#![cfg(feature = "stream")]
#![allow(unsafe_code, clippy::unwrap_used)]

use std::ffi::CStr;

use moonlight_sys as sys;

#[test]
fn the_library_is_linked_and_answers_its_own_queries() {
    // SAFETY: LiGetStageName is pure and returns a static string for any int.
    let name = unsafe { CStr::from_ptr(sys::LiGetStageName(sys::STAGE_RTSP_HANDSHAKE)) };
    assert_eq!(
        name.to_str().unwrap(),
        "RTSP handshake",
        "linked moonlight-common-c did not name its own stage — wrong library, or the \
         checked-in bindings drifted from the pinned source"
    );
}

#[test]
fn the_launch_query_parameters_ask_for_the_encrypted_protocol() {
    // The library owns this string, and Sunshine keys encrypted RTSP off it. If a
    // version bump ever stops emitting corever, `/launch` starts getting 403s from
    // hosts in mandatory-encryption mode — a failure that looks like a host problem.
    // SAFETY: returns a static string owned by the library.
    let params = unsafe { CStr::from_ptr(sys::LiGetLaunchUrlQueryParameters()) };
    let params = params.to_str().unwrap();
    assert!(
        params.contains("corever="),
        "launch parameters no longer carry corever: {params:?}"
    );
}

#[test]
fn the_bindings_agree_with_the_librarys_own_port_table() {
    // A cheap ABI canary: these come out of the library at runtime, while the
    // constants come from the checked-in bindings. They must agree, or the bindings
    // are stale against the archive we linked.
    // SAFETY: both are pure lookups over an int the header defines.
    let (video, control, audio) = unsafe {
        (
            sys::LiGetPortFromPortFlagIndex(sys::ML_PORT_INDEX_UDP_47998),
            sys::LiGetPortFromPortFlagIndex(sys::ML_PORT_INDEX_UDP_47999),
            sys::LiGetPortFromPortFlagIndex(sys::ML_PORT_INDEX_UDP_48000),
        )
    };
    assert_eq!((video, control, audio), (47_998, 47_999, 48_000));
}

#[test]
fn stream_configuration_zeroes_the_way_the_library_expects() {
    // LiInitializeStreamConfiguration is the library's own initializer; running it
    // over our binding's struct proves the layout is at least writable at the size
    // the library believes, which is what a silently-wrong bindgen run would break.
    let mut config: sys::STREAM_CONFIGURATION = unsafe { std::mem::zeroed() };
    config.width = 1234;
    // SAFETY: a valid, uniquely-owned struct of the type the function takes.
    unsafe { sys::LiInitializeStreamConfiguration(&raw mut config) };
    assert_eq!(
        config.width, 0,
        "the library did not zero a field our binding says is there"
    );
}
