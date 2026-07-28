//! # moonlight-sys
//!
//! Raw FFI bindings to [moonlight-common-c], the GameStream client core castaway links
//! rather than reimplements (DECISION-LOG D37). The library owns the RTSP handshake, the
//! ENet control stream, video depacketization + Reed-Solomon FEC, audio decryption, and
//! input encoding; castaway owns everything on the LAN-facing side of it (discovery,
//! NVHTTP, pairing) plus the safe wrapper in `proto-gamestream`.
//!
//! `src/bindings.rs` is **pregenerated and checked in**, pinned to the same revision the
//! Nix derivation builds (`nix/moonlight-common-c.nix`). The `moonlight-bindings` flake
//! check regenerates it with bindgen against the pinned source and fails on any diff, so
//! a version bump that changes the ABI cannot land silently. Struct layouts are guarded
//! by bindgen's compile-time size/offset assertions inside the generated file.
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
