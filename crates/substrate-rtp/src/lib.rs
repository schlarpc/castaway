//! # substrate-rtp
//!
//! Shared RTP plumbing: the [`packet`] header parser and a [`reorder`] buffer that
//! restores sequence order and drops stale packets. What it deliberately does **not**
//! do is depacketize payloads — MPEG-TS (Miracast), ALAC/AAC (RAOP), and AirPlay's
//! bespoke mirror framing all differ, so that lives in each `proto-*` crate
//! (architecture §1b). This crate is pure and unit-tested against constructed packets.
#![forbid(unsafe_code)]

pub mod packet;
pub mod reorder;

pub use packet::{RtpError, RtpHeader, RtpPacket};
pub use reorder::ReorderBuffer;
