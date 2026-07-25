//! Reading the openscreen-generated RTP fixtures.
//!
//! Shared by the pure differential test (`openscreen_stream.rs`) and the UDP actor test
//! (`mirror_udp.rs`) so both are checked against the same reference bytes. See
//! `fixtures/rtp-stream/README.md` for the file formats and their provenance.

#![allow(clippy::unwrap_used, dead_code)]

use std::num::NonZeroU32;

use bytes::Bytes;
use proto_cast::mirror::{Codec, MirrorConfig, StreamConfig};
use proto_cast::rtp::Dependency;

pub const SENDER_SSRC: u32 = 0x0102_0304;
pub const RECEIVER_SSRC: u32 = 0x0a0b_0c0d;

/// Must match the constants in `fixtures/rtp-stream/generator/gen_rtp_fixtures.cc`.
pub const AES_KEY: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];
pub const AES_IV_MASK: [u8; 16] = [
    0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1, 0xf0,
];

const PACKETS_BIN: &[u8] = include_bytes!("../fixtures/rtp-stream/packets.bin");
const FRAMES_BIN: &[u8] = include_bytes!("../fixtures/rtp-stream/frames.bin");

/// One frame as openscreen encoded it, before encryption.
#[derive(Debug, PartialEq, Eq)]
pub struct ExpectedFrame {
    pub dependency: Dependency,
    pub frame_id: i64,
    pub referenced_frame_id: i64,
    pub rtp_timestamp: i64,
    pub new_playout_delay_ms: u16,
    pub payload: Vec<u8>,
}

pub fn config() -> StreamConfig {
    StreamConfig {
        index: 0,
        sender_ssrc: SENDER_SSRC,
        receiver_ssrc: RECEIVER_SSRC,
        payload_type: 100,
        codec: Codec::Vp8,
        // openscreen's packetizer is timebase-agnostic — it copies the timestamps it is
        // given — so this only has to be the video default the generator implies.
        rtp_timebase: NonZeroU32::new(90_000).unwrap(),
        aes_key: AES_KEY,
        aes_iv_mask: AES_IV_MASK,
    }
}

/// The fixture stream as a negotiated session, for driving the UDP actor.
pub fn mirror_config(udp_port: u16) -> MirrorConfig {
    MirrorConfig {
        udp_port,
        video: config(),
        audio: None,
    }
}

/// Split `packets.bin` into its u16-length-prefixed datagrams.
pub fn datagrams() -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 2 <= PACKETS_BIN.len() {
        let len = usize::from(u16::from_be_bytes([PACKETS_BIN[at], PACKETS_BIN[at + 1]]));
        at += 2;
        out.push(Bytes::copy_from_slice(&PACKETS_BIN[at..at + len]));
        at += len;
    }
    assert_eq!(at, PACKETS_BIN.len(), "trailing bytes in packets.bin");
    out
}

pub fn expected_frames() -> Vec<ExpectedFrame> {
    fn u32_at(buf: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
    }

    let mut out = Vec::new();
    let mut at = 0;
    while at < FRAMES_BIN.len() {
        // openscreen's EncodedFrame::Dependency, as its underlying int8_t.
        let dependency = match FRAMES_BIN[at] {
            1 => Dependency::Dependent,
            2 => Dependency::Independent,
            3 => Dependency::KeyFrame,
            other => panic!("unknown dependency {other} in frames.bin"),
        };
        at += 1;
        let frame_id = i64::from(u32_at(FRAMES_BIN, at));
        at += 4;
        let referenced_frame_id = i64::from(u32_at(FRAMES_BIN, at));
        at += 4;
        let rtp_timestamp = i64::from(u32_at(FRAMES_BIN, at));
        at += 4;
        let new_playout_delay_ms = u16::from_be_bytes([FRAMES_BIN[at], FRAMES_BIN[at + 1]]);
        at += 2;
        let length = u32_at(FRAMES_BIN, at) as usize;
        at += 4;
        let payload = FRAMES_BIN[at..at + length].to_vec();
        at += length;
        out.push(ExpectedFrame {
            dependency,
            frame_id,
            referenced_frame_id,
            rtp_timestamp,
            new_playout_delay_ms,
            payload,
        });
    }
    out
}
