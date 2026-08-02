//! # crypto-playfair
//!
//! The FairPlay v3 key derivation: turning the 72-byte `ekey` an AirPlay sender puts in
//! its RTSP `SETUP` into the 16-byte AES key that decrypts a mirroring stream.
//!
//! ## Why this is a crate of its own
//!
//! Two reasons, and both are about keeping a boundary visible.
//!
//! The first is provenance. This is transcribed from the published `playfair`
//! implementation carried by UxPlay and RPiPlay — roughly a hundred kilobytes of
//! constants recovered from Apple's binary, plus twelve hundred lines of algorithm whose
//! own author wrote that he did not know what it was doing. That material is GPL where
//! this workspace is MIT, and its status with respect to Apple is, in UxPlay's own
//! words, unclear. Keeping it behind one crate boundary means the question has one
//! place to be answered rather than being spread through `proto-airplay`.
//!
//! The second is that nothing else needs it. AirPlay 1 audio — the path this receiver
//! serves by default — never touches FairPlay: its session key arrives RSA-wrapped in
//! the `ANNOUNCE` body and `crypto-raop` unwraps it. This crate exists only for
//! mirroring.
//!
//! ## How it is known to be right
//!
//! Not by reading it. The transcription is mechanical on purpose, because a slip in
//! four hundred lines of bit manipulation would not fail loudly — it would produce a
//! wrong key, and a mirroring session that connects and then shows static.
//!
//! What settles it is `tests/vectors.rs`: twenty published
//! `(key message, ekey, expected key)` triples covering all four modes. They are the
//! reason this could be written at all without a live capture.
#![forbid(unsafe_code)]

mod cint;
mod garble;
mod tables;

use tables::{
    DEFAULT_SAP, INDEX_MANGLE, INITIAL_SESSION_KEY, MESSAGE_IV, MESSAGE_KEY, STATIC_SOURCE_1,
    STATIC_SOURCE_2, TABLE_S1, TABLE_S10, TABLE_S2, TABLE_S3, TABLE_S4, TABLE_S5, TABLE_S6,
    TABLE_S7, TABLE_S8, TABLE_S9, T_KEY, X_KEY, Z_KEY,
};

/// The length of the `ekey` a `SETUP` carries.
pub const EKEY_LEN: usize = 72;
/// The length of the SETUP2 key message retained from `/fp-setup`.
pub const KEY_MESSAGE_LEN: usize = 164;

/// How many derivation modes the recovered tables hold.
const MODE_COUNT: usize = 4;

/// Why a key message could not be unwrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlayFairError {
    /// The key message's derivation-mode byte names a mode the tables do not hold.
    #[error("unknown fairplay derivation mode {0}")]
    UnknownMode(u8),
}

/// Which of the four recovered derivation tables a key message selects.
///
/// A newtype because the raw byte is offset 12 of a body a sender POSTs to `/fp-setup`,
/// and every use of it is an unchecked index into a four-entry table. Validating it here
/// is what stops `MESSAGE_KEY[255]` from aborting the session actor — the same treatment
/// `crypto_fairplay::Mode` already gives SETUP1's selector at offset 14.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DerivationMode(usize);

impl DerivationMode {
    /// The mode a key message selects.
    ///
    /// # Errors
    /// [`PlayFairError::UnknownMode`] if byte 12 does not name one of the four.
    fn of(message_in: &[u8; KEY_MESSAGE_LEN]) -> Result<Self, PlayFairError> {
        let raw = message_in[12];
        if usize::from(raw) < MODE_COUNT {
            Ok(Self(usize::from(raw)))
        } else {
            Err(PlayFairError::UnknownMode(raw))
        }
    }

    const fn get(self) -> usize {
        self.0
    }
}

/// Read a little-endian `u32`. The transcribed C aliases byte arrays as `uint32_t*`
/// throughout, which is native-endian; every machine this targets is little-endian.
fn rd32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Write a little-endian `u32`, for the same reason.
fn wr32(b: &mut [u8], i: usize, v: u32) {
    b[i..i + 4].copy_from_slice(&v.to_le_bytes());
}

/// XOR a run of 16-byte blocks with a fixed key.
fn key_xor(input: &[u8], out: &mut [u8], key: &[u8; 16], blocks: usize) {
    for j in 0..blocks {
        for i in 0..16 {
            out[j * 16 + i] = input[j * 16 + i] ^ key[i];
        }
    }
}

/// `table_index`: a 256-byte slice of `table_s1`.
fn table_index(i: usize) -> &'static [u8] {
    let off = ((31 * i) % 0x28) << 8;
    &TABLE_S1[off..off + 256]
}

/// `message_table_index`: a 256-byte slice of `table_s2`.
fn message_table_index(i: usize) -> &'static [u8] {
    let off = ((97 * i) % 144) << 8;
    &TABLE_S2[off..off + 256]
}

/// `permute_table_2`: a 256-byte slice of `table_s4`.
fn permute_table_2(i: usize) -> &'static [u8] {
    let off = ((71 * i) % 144) << 8;
    &TABLE_S4[off..off + 256]
}

/// The AES-shaped byte permutation driven by `table_s3`.
fn permute_block_1(block: &mut [u8; 16]) {
    let t = |off: usize, v: u8| TABLE_S3[off + usize::from(v)];
    block[0] = t(0x000, block[0]);
    block[4] = t(0x400, block[4]);
    block[8] = t(0x800, block[8]);
    block[12] = t(0xc00, block[12]);

    let tmp = block[13];
    block[13] = t(0x100, block[9]);
    block[9] = t(0xd00, block[5]);
    block[5] = t(0x900, block[1]);
    block[1] = t(0x500, tmp);

    let tmp = block[2];
    block[2] = t(0xa00, block[10]);
    block[10] = t(0x200, tmp);
    let tmp = block[6];
    block[6] = t(0xe00, block[14]);
    block[14] = t(0x600, tmp);

    let tmp = block[3];
    block[3] = t(0xf00, block[7]);
    block[7] = t(0x300, block[11]);
    block[11] = t(0x700, block[15]);
    block[15] = t(0xb00, tmp);
}

/// The same permutation, but with a per-round table selection.
fn permute_block_2(block: &mut [u8; 16], round: usize) {
    let b = round * 16;
    let t = |i: usize, v: u8| permute_table_2(b + i)[usize::from(v)];
    block[0] = t(0, block[0]);
    block[4] = t(4, block[4]);
    block[8] = t(8, block[8]);
    block[12] = t(12, block[12]);

    let tmp = block[13];
    block[13] = t(13, block[9]);
    block[9] = t(9, block[5]);
    block[5] = t(5, block[1]);
    block[1] = t(1, tmp);

    let tmp = block[2];
    block[2] = t(2, block[10]);
    block[10] = t(10, tmp);
    let tmp = block[6];
    block[6] = t(6, block[14]);
    block[14] = t(14, tmp);

    let tmp = block[3];
    block[3] = t(3, block[7]);
    block[7] = t(7, block[11]);
    block[11] = t(11, block[15]);
    block[15] = t(15, tmp);
}

/// Rijndael-shaped key expansion over a different set of S-boxes.
fn generate_key_schedule(key_material: &[u8; 16]) -> [[u32; 4]; 11] {
    let mut schedule = [[0u32; 4]; 11];
    let mut buffer = [0u8; 16];
    key_xor(key_material, &mut buffer, &T_KEY, 1);

    let mut ti = 0usize;
    for (round, slot) in schedule.iter_mut().enumerate() {
        slot[0] = rd32(&buffer, 0);

        let (t1, t2, t3, t4) = (
            table_index(ti),
            table_index(ti + 1),
            table_index(ti + 2),
            table_index(ti + 3),
        );
        ti += 4;
        buffer[0] ^= t1[usize::from(buffer[0x0d])] ^ INDEX_MANGLE[round];
        buffer[1] ^= t2[usize::from(buffer[0x0e])];
        buffer[2] ^= t3[usize::from(buffer[0x0f])];
        buffer[3] ^= t4[usize::from(buffer[0x0c])];

        // Each word is folded into the next, in place, so the schedule entry is taken
        // *before* the fold and the next fold sees the folded value.
        slot[1] = rd32(&buffer, 4);
        let folded = rd32(&buffer, 4) ^ rd32(&buffer, 0);
        wr32(&mut buffer, 4, folded);
        slot[2] = rd32(&buffer, 8);
        let folded = rd32(&buffer, 8) ^ rd32(&buffer, 4);
        wr32(&mut buffer, 8, folded);
        slot[3] = rd32(&buffer, 12);
        let folded = rd32(&buffer, 12) ^ rd32(&buffer, 8);
        wr32(&mut buffer, 12, folded);
    }
    schedule
}

/// The block cipher itself — AES-shaped, over the recovered T-tables.
fn cycle(block: &mut [u8; 16], schedule: &[[u32; 4]; 11]) {
    for (i, word) in schedule[10].iter().enumerate() {
        let v = rd32(block, i * 4) ^ word;
        wr32(block, i * 4, v);
    }
    permute_block_1(block);

    for round in 0..9 {
        let key = schedule[9 - round];
        let kb = |w: usize| key[w].to_le_bytes();

        let k0 = kb(0);
        let ab = TABLE_S5[usize::from(block[3] ^ k0[3])]
            ^ TABLE_S6[usize::from(block[2] ^ k0[2])]
            ^ TABLE_S8[usize::from(block[0] ^ k0[0])]
            ^ TABLE_S7[usize::from(block[1] ^ k0[1])];
        wr32(block, 0, ab);

        let k1 = kb(1);
        let ab = TABLE_S6[usize::from(block[6] ^ k1[2])]
            ^ TABLE_S5[usize::from(block[7] ^ k1[3])]
            ^ TABLE_S8[usize::from(block[4] ^ k1[0])]
            ^ TABLE_S7[usize::from(block[5] ^ k1[1])];
        wr32(block, 4, ab);

        let k2 = kb(2);
        let w2 = TABLE_S5[usize::from(block[11] ^ k2[3])]
            ^ TABLE_S6[usize::from(block[10] ^ k2[2])]
            ^ TABLE_S7[usize::from(block[9] ^ k2[1])]
            ^ TABLE_S8[usize::from(block[8] ^ k2[0])];
        wr32(block, 8, w2);

        let k3 = kb(3);
        let w3 = TABLE_S5[usize::from(block[15] ^ k3[3])]
            ^ TABLE_S6[usize::from(block[14] ^ k3[2])]
            ^ TABLE_S7[usize::from(block[13] ^ k3[1])]
            ^ TABLE_S8[usize::from(block[12] ^ k3[0])];
        wr32(block, 12, w3);

        permute_block_2(block, 8 - round);
    }
    for (i, word) in schedule[0].iter().enumerate() {
        let v = rd32(block, i * 4) ^ word;
        wr32(block, i * 4, v);
    }
}

/// Decrypt the 164-byte key message into 128 bytes.
fn decrypt_message(message_in: &[u8; KEY_MESSAGE_LEN], mode: DerivationMode) -> [u8; 128] {
    let mut out = [0u8; 128];
    let mode = mode.get();
    let mut buffer = [0u8; 16];

    for i in 0..8 {
        for j in 0..16 {
            buffer[j] = if mode == 3 {
                message_in[(0x80 - 0x10 * i) + j]
            } else {
                message_in[(0x10 * (i + 1)) + j]
            };
        }

        for j in 0..9 {
            let base = 0x80 - 0x10 * j;
            let mk = &MESSAGE_KEY[mode];
            let t = |i: usize, v: u8| message_table_index(base + i)[usize::from(v)] ^ mk[base + i];

            buffer[0x0] = t(0x0, buffer[0x0]);
            buffer[0x4] = t(0x4, buffer[0x4]);
            buffer[0x8] = t(0x8, buffer[0x8]);
            buffer[0xc] = t(0xc, buffer[0xc]);

            let tmp = buffer[0x0d];
            buffer[0xd] = t(0xd, buffer[0x9]);
            buffer[0x9] = t(0x9, buffer[0x5]);
            buffer[0x5] = t(0x5, buffer[0x1]);
            buffer[0x1] = t(0x1, tmp);

            let tmp = buffer[0x2];
            buffer[0x2] = t(0x2, buffer[0xa]);
            buffer[0xa] = t(0xa, tmp);
            let tmp = buffer[0x6];
            buffer[0x6] = t(0x6, buffer[0xe]);
            buffer[0xe] = t(0xe, tmp);

            let tmp = buffer[0x3];
            buffer[0x3] = t(0x3, buffer[0x7]);
            buffer[0x7] = t(0x7, buffer[0xb]);
            buffer[0xb] = t(0xb, buffer[0xf]);
            buffer[0xf] = t(0xf, tmp);

            let s9 = |q: usize, v: u8| TABLE_S9[q + usize::from(v)];
            let w0 = s9(0x000, buffer[0x0])
                ^ s9(0x100, buffer[0x1])
                ^ s9(0x200, buffer[0x2])
                ^ s9(0x300, buffer[0x3]);
            let w1 = s9(0x000, buffer[0x4])
                ^ s9(0x100, buffer[0x5])
                ^ s9(0x200, buffer[0x6])
                ^ s9(0x300, buffer[0x7]);
            let w2 = s9(0x000, buffer[0x8])
                ^ s9(0x100, buffer[0x9])
                ^ s9(0x200, buffer[0xa])
                ^ s9(0x300, buffer[0xb]);
            let w3 = s9(0x000, buffer[0xc])
                ^ s9(0x100, buffer[0xd])
                ^ s9(0x200, buffer[0xe])
                ^ s9(0x300, buffer[0xf]);
            wr32(&mut buffer, 0, w0);
            wr32(&mut buffer, 4, w1);
            wr32(&mut buffer, 8, w2);
            wr32(&mut buffer, 12, w3);
        }

        let t10 = |i: usize, v: u8| TABLE_S10[(i << 8) + usize::from(v)];
        buffer[0x0] = t10(0x0, buffer[0x0]);
        buffer[0x4] = t10(0x4, buffer[0x4]);
        buffer[0x8] = t10(0x8, buffer[0x8]);
        buffer[0xc] = t10(0xc, buffer[0xc]);

        let tmp = buffer[0x0d];
        buffer[0xd] = t10(0xd, buffer[0x9]);
        buffer[0x9] = t10(0x9, buffer[0x5]);
        buffer[0x5] = t10(0x5, buffer[0x1]);
        buffer[0x1] = t10(0x1, tmp);

        let tmp = buffer[0x2];
        buffer[0x2] = t10(0x2, buffer[0xa]);
        buffer[0xa] = t10(0xa, tmp);
        let tmp = buffer[0x6];
        buffer[0x6] = t10(0x6, buffer[0xe]);
        buffer[0xe] = t10(0xe, tmp);

        let tmp = buffer[0x3];
        buffer[0x3] = t10(0x3, buffer[0x7]);
        buffer[0x7] = t10(0x7, buffer[0xb]);
        buffer[0xb] = t10(0xb, buffer[0xf]);
        buffer[0xf] = t10(0xf, tmp);

        // Chain against the previous ciphertext block — forwards for modes 0..2 and
        // backwards for mode 3, which walks the message in the other direction.
        if mode == 3 {
            let dst = 0x70 - 0x10 * i;
            for j in 0..16 {
                out[dst + j] = buffer[j]
                    ^ if i < 7 {
                        message_in[0x70 - 0x10 * i + j]
                    } else {
                        MESSAGE_IV[mode][j]
                    };
            }
        } else {
            let dst = 0x10 * i;
            for j in 0..16 {
                out[dst + j] = buffer[j]
                    ^ if i > 0 {
                        message_in[0x10 * i + j]
                    } else {
                        MESSAGE_IV[mode][j]
                    };
            }
        }
    }
    out
}

/// The MD5 round constants, `floor(abs(sin(i + 1)) * 2^32)`.
const MD5_K: [u32; 64] = tables::MD5_K;
/// MD5's per-round rotation amounts.
const MD5_SHIFT: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// MD5, but the message block is shuffled mid-way through by the state itself.
///
/// That shuffle is the whole modification, and it is why a stock MD5 cannot stand in.
fn modified_md5(original: &[u8; 64], key_in: &[u8; 16]) -> [u8; 16] {
    let mut block = *original;
    let (mut a, mut b, mut c, mut d) = (
        rd32(key_in, 0),
        rd32(key_in, 4),
        rd32(key_in, 8),
        rd32(key_in, 12),
    );

    for i in 0..64usize {
        let j = match i {
            0..=15 => i,
            16..=31 => (5 * i + 1) % 16,
            32..=47 => (3 * i + 5) % 16,
            _ => (7 * i) % 16,
        };
        // Big-endian here, unlike the word view used for the shuffle below.
        let input = u32::from_be_bytes([
            block[4 * j],
            block[4 * j + 1],
            block[4 * j + 2],
            block[4 * j + 3],
        ]);
        let mixed = match i {
            0..=15 => (b & c) | (!b & d),
            16..=31 => (b & d) | (c & !d),
            32..=47 => b ^ c ^ d,
            _ => c ^ (b | !d),
        };
        let z = a
            .wrapping_add(input)
            .wrapping_add(MD5_K[i])
            .wrapping_add(mixed)
            .rotate_left(MD5_SHIFT[i])
            .wrapping_add(b);
        let tmp = d;
        d = c;
        c = b;
        b = z;
        a = tmp;

        if i == 31 {
            let mut swap = |x: u32, y: u32| {
                let (x, y) = ((x & 15) as usize, (y & 15) as usize);
                let (vx, vy) = (rd32(&block, x * 4), rd32(&block, y * 4));
                wr32(&mut block, x * 4, vy);
                wr32(&mut block, y * 4, vx);
            };
            swap(a, b);
            swap(c, d);
            swap((a & (15 << 4)) >> 4, (b & (15 << 4)) >> 4);
            swap((a & (15 << 8)) >> 8, (b & (15 << 8)) >> 8);
            swap((a & (15 << 12)) >> 12, (b & (15 << 12)) >> 12);
        }
    }

    let mut out = [0u8; 16];
    wr32(&mut out, 0, rd32(key_in, 0).wrapping_add(a));
    wr32(&mut out, 4, rd32(key_in, 4).wrapping_add(b));
    wr32(&mut out, 8, rd32(key_in, 8).wrapping_add(c));
    wr32(&mut out, 12, rd32(key_in, 12).wrapping_add(d));
    out
}

/// An 8-bit rotate that stores back as a byte.
fn rol8_u8(x: u8, count: u32) -> u8 {
    x.rotate_left(count % 8)
}

/// The bespoke 210-byte scrambler, and the `garble` step it wraps.
fn sap_hash(block_in: &[u8; 64], key_out: &mut [u8; 16]) {
    let mut b0: [u8; 20] = [
        0x96, 0x5F, 0xC6, 0x53, 0xF8, 0x46, 0xCC, 0x18, 0xDF, 0xBE, 0xB2, 0xF8, 0x38, 0xD7, 0xEC,
        0x22, 0x03, 0xD1, 0x20, 0x8F,
    ];
    let mut b1 = [0u8; 210];
    let mut b2: [u8; 35] = [
        0x43, 0x54, 0x62, 0x7A, 0x18, 0xC3, 0xD6, 0xB3, 0x9A, 0x56, 0xF6, 0x1C, 0x14, 0x3F, 0x0C,
        0x1D, 0x3B, 0x36, 0x83, 0xB1, 0x39, 0x51, 0x4A, 0xAA, 0x09, 0x3E, 0xFE, 0x44, 0xAF, 0xDE,
        0xC3, 0x20, 0x9D, 0x42, 0x3A,
    ];
    let mut b3 = [0u8; 132];
    let mut b4: [u8; 21] = [
        0xED, 0x25, 0xD1, 0xBB, 0xBC, 0x27, 0x9F, 0x02, 0xA2, 0xA9, 0x11, 0x00, 0x0C, 0xB3, 0x52,
        0xC0, 0xBD, 0xE3, 0x1B, 0x49, 0xC7,
    ];
    const I0_INDEX: [usize; 11] = [18, 22, 23, 0, 5, 19, 32, 31, 10, 21, 30];

    // Load, byte-swapped within each word: the original reads the input through a
    // `uint32_t*` and then picks bytes most-significant-first.
    for (i, slot) in b1.iter_mut().enumerate() {
        let word = rd32(block_in, ((i % 64) >> 2) << 2);
        *slot = u8::try_from((word >> ((3 - (i % 4)) << 3)) & 0xff).unwrap_or(0);
    }

    // 840 rounds of a four-tap scramble. The indices are computed modulo 210 on
    // *unsigned* 32-bit values, so the negative offsets wrap enormous and land
    // somewhere quite different from a signed remainder.
    for i in 0..840u32 {
        let at = |k: u32| usize::try_from(k % 210).unwrap_or(0);
        let x = b1[at(i.wrapping_sub(155))];
        let y = b1[at(i.wrapping_sub(57))];
        let z = b1[at(i.wrapping_sub(13))];
        let w = b1[at(i)];
        b1[at(i)] = rol8_u8(y, 5)
            .wrapping_add(rol8_u8(z, 3) ^ w)
            .wrapping_sub(rol8_u8(x, 7));
    }

    garble::garble(&mut b0, &mut b1, &mut b2, &mut b3, &mut b4);

    key_out.fill(0xE1);
    for (i, &idx) in I0_INDEX.iter().enumerate() {
        // Index 3 is a constant in the original, not a computed value.
        key_out[i] = if i == 3 {
            0x3d
        } else {
            key_out[i].wrapping_add(b3[idx * 4])
        };
    }
    for (i, &v) in b0.iter().enumerate() {
        key_out[i % 16] ^= v;
    }
    for (i, &v) in b2.iter().enumerate() {
        key_out[i % 16] ^= v;
    }
    for (i, &v) in b1.iter().enumerate() {
        key_out[i % 16] ^= v;
    }

    for _ in 0..16 {
        for i in 0..16u32 {
            let at = |k: u32| usize::try_from(k % 16).unwrap_or(0);
            let x = key_out[at(i.wrapping_sub(7))];
            let y = key_out[at(i)];
            let z = key_out[at(i.wrapping_sub(37))];
            let w = key_out[at(i.wrapping_sub(177))];
            key_out[usize::try_from(i).unwrap_or(0)] =
                rol8_u8(x, 1) ^ y ^ rol8_u8(z, 6) ^ rol8_u8(w, 5);
        }
    }
}

/// Five rounds of MD5-plus-scramble over the reassembled SAP.
fn generate_session_key(
    old_sap: &[u8],
    message_in: &[u8; KEY_MESSAGE_LEN],
    mode: DerivationMode,
) -> [u8; 16] {
    let decrypted = decrypt_message(message_in, mode);

    let mut new_sap = [0u8; 320];
    new_sap[0x000..0x011].copy_from_slice(&STATIC_SOURCE_1);
    new_sap[0x011..0x091].copy_from_slice(&decrypted);
    new_sap[0x091..0x111].copy_from_slice(&old_sap[0x80..0x100]);
    new_sap[0x111..0x140].copy_from_slice(&STATIC_SOURCE_2);

    let mut session_key = INITIAL_SESSION_KEY;
    for round in 0..5 {
        let mut base = [0u8; 64];
        base.copy_from_slice(&new_sap[round * 64..round * 64 + 64]);
        let md5 = modified_md5(&base, &session_key);
        // `sap_hash` overwrites the key, and then the MD5 is added back onto it.
        sap_hash(&base, &mut session_key);
        for i in 0..4 {
            let sum = rd32(&session_key, i * 4).wrapping_add(rd32(&md5, i * 4));
            wr32(&mut session_key, i * 4, sum);
        }
    }

    for i in (0..16).step_by(4) {
        session_key.swap(i, i + 3);
        session_key.swap(i + 1, i + 2);
    }
    for byte in &mut session_key {
        *byte ^= 121;
    }
    session_key
}

/// Unwrap a mirroring session key.
///
/// `key_message` is the 164-byte SETUP2 body retained from `/fp-setup`; `cipher_text` is
/// the 72-byte `ekey` from the RTSP `SETUP` plist.
///
/// # Errors
/// [`PlayFairError::UnknownMode`] if the key message's derivation-mode byte is not one
/// of the four the tables hold. It is sender-controlled and unauthenticated, so this is
/// a refusal rather than an assertion.
pub fn decrypt_key(
    key_message: &[u8; KEY_MESSAGE_LEN],
    cipher_text: &[u8; EKEY_LEN],
) -> Result<[u8; 16], PlayFairError> {
    let mode = DerivationMode::of(key_message)?;
    let sap_key = generate_session_key(&DEFAULT_SAP, key_message, mode);
    let schedule = generate_key_schedule(&sap_key);

    let mut block = [0u8; 16];
    key_xor(&cipher_text[56..72], &mut block, &Z_KEY, 1);
    cycle(&mut block, &schedule);

    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = block[i] ^ cipher_text[16 + i];
    }
    let mut tmp = [0u8; 16];
    key_xor(&out, &mut tmp, &X_KEY, 1);
    key_xor(&tmp, &mut out, &Z_KEY, 1);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_are_the_sizes_the_algorithm_indexes() {
        // Cheap insurance against a mis-generated table file: every one of these is
        // indexed without a bounds check in the original, so a short table would be a
        // panic here and was a buffer overrun there.
        assert_eq!(TABLE_S1.len(), 40 * 256);
        assert_eq!(TABLE_S2.len(), 144 * 256);
        assert_eq!(TABLE_S3.len(), 16 * 256);
        assert_eq!(TABLE_S4.len(), 144 * 256);
        assert_eq!(TABLE_S9.len(), 1024);
        assert_eq!(TABLE_S10.len(), 16 * 256);
        assert_eq!(DEFAULT_SAP.len(), 276);
    }

    #[test]
    fn a_key_schedule_is_produced_for_every_round() {
        let schedule = generate_key_schedule(&INITIAL_SESSION_KEY);
        assert_eq!(schedule.len(), 11);
        // The placeholder the original fills with is 0xdeadbeef; every slot must have
        // been overwritten.
        assert!(schedule.iter().flatten().all(|&w| w != 0xdead_beef));
    }
}
