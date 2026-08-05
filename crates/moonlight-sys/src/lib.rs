//! # moonlight-sys
//!
//! Raw FFI bindings to [moonlight-common-c], the GameStream client core castaway links
//! rather than reimplements (DECISION-LOG D37). The library owns the RTSP handshake, the
//! ENet control stream, video depacketization + Reed-Solomon FEC, audio decryption, and
//! input encoding; castaway owns everything on the LAN-facing side of it (discovery,
//! NVHTTP, pairing) plus the safe wrapper in `proto-gamestream`.
//!
//! `src/bindings.rs` is **pregenerated and checked in**, pinned to the same revision the
//! Nix derivation builds (`nix/moonlight-common-c.nix`).
//!
//! The drift guard is `checks.moonlight-bindings` (`nix/moonlight-bindings.nix`), which
//! regenerates this file from the pinned header and fails on any diff. It exists because
//! the compile-time size/offset assertions *inside* the generated file are
//! **self-referential**: bindgen produced both the struct and the assertion from the same
//! header, so they catch a hand-edit or a target-ABI change and can never catch upstream
//! drift — a revision bump that moved a field inside `STREAM_CONFIGURATION` changes
//! neither of them, only the library the struct is passed to. That failure is not a
//! compile error and not a link error; it is a live session reading its parameters from
//! the wrong offsets (#191).
//!
//! Everything here is `unsafe extern "C"`; the safe boundary is
//! `proto_gamestream::stream`, which is the only permitted consumer (rule 8: FFI surface
//! thin, wrapped in safe types at the crate boundary).
//!
//! moonlight-common-c is **a singleton**: one connection per process, global internal
//! state, and `LiStartConnection`/`LiStopConnection` are documented not thread-safe. The
//! safe wrapper serializes them.
//!
//! [moonlight-common-c]: https://github.com/moonlight-stream/moonlight-common-c

pub mod bindings;

pub use bindings::*;
