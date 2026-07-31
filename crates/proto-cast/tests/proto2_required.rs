//! Google's `cast_channel.proto` is proto2. Its `required` fields are written even when
//! they hold their default value, and a real sender rejects the *whole message* if one is
//! missing — C++ `ParseFromString` returns false, which Chrome reports as "Error parsing
//! packet body." and turns into a torn-down channel and a receiver that never appears.
//!
//! prost defaults to proto3 semantics, where a plain scalar equal to its default is
//! omitted. Two of `CastMessage`'s required fields have 0 as their meaningful value —
//! `ProtocolVersion::CASTV2_1_0` and `PayloadType::STRING` — so the two encodings differ
//! exactly where it is fatal and nowhere else. Nothing in Rust catches it: the message
//! round-trips through prost perfectly, because prost does not enforce requiredness
//! either. Only a proto2 parser can tell, which is why this is a wire-level assertion and
//! not an equality check against a decoded struct.
//!
//! Note that the `openscreen-device-auth` vectors do *not* cover this: they judge the
//! device-auth payload, and the envelope carrying it is rebuilt on the C++ side by the
//! oracle itself. The envelope this receiver actually writes had never been read by a
//! proto2 parser at all, which is how the defect survived a check built to catch exactly
//! this class of thing. Reproduced against Chromium 148 before the fix — "Error parsing
//! packet body." on every connection — and gone after it.
#![allow(clippy::unwrap_used)]

use proto_cast::proto::{
    auth_error, AuthError, AuthResponse, CastMessage, DeviceAuthMessage, PayloadType,
    ProtocolVersion,
};

/// The protobuf field numbers present at the top level of `buf`, in wire order.
///
/// Hand-decoded rather than taken from a parse, because the thing under test is whether a
/// tag reaches the wire at all — and every Rust-side decoder that could report it is the
/// same one that omitted it.
fn field_numbers(buf: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let (key, read) = varint(buf, i);
        i += read;
        let field = u32::try_from(key >> 3).unwrap();
        match key & 7 {
            // varint
            0 => {
                let (_, read) = varint(buf, i);
                i += read;
            }
            // length-delimited
            2 => {
                let (len, read) = varint(buf, i);
                i += read + usize::try_from(len).unwrap();
            }
            other => panic!("unexpected wire type {other} for field {field}"),
        }
        out.push(field);
    }
    out
}

fn varint(buf: &[u8], mut i: usize) -> (u64, usize) {
    let (mut value, mut shift, start) = (0u64, 0u32, i);
    loop {
        let byte = buf[i];
        i += 1;
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return (value, i - start);
        }
    }
}

fn encode(msg: &impl prost::Message) -> Vec<u8> {
    let mut buf = Vec::new();
    msg.encode(&mut buf).unwrap();
    buf
}

/// A JSON message is the worst case: *both* of its enum-valued required fields are zero.
#[test]
fn a_json_cast_message_carries_every_required_field() {
    let msg = CastMessage::json(
        "receiver-0",
        "sender-0",
        "urn:x-cast:com.google.cast.tp.connection",
        "{}".to_string(),
    );
    assert_eq!(msg.protocol_version, ProtocolVersion::Castv210 as i32);
    assert_eq!(msg.payload_type, PayloadType::String as i32);

    let fields = field_numbers(&encode(&msg));
    for (tag, name) in [
        (1, "protocol_version"),
        (2, "source_id"),
        (3, "destination_id"),
        (4, "namespace"),
        (5, "payload_type"),
    ] {
        assert!(
            fields.contains(&tag),
            "required field {name} (tag {tag}) is missing from the encoding; a proto2 \
             sender rejects the whole message. Present: {fields:?}"
        );
    }
}

/// The device-auth reply, which is the *first* thing an official sender parses — and so
/// the message whose omission costs the entire connection rather than one exchange.
#[test]
fn a_binary_cast_message_carries_every_required_field() {
    let msg = CastMessage::binary(
        "receiver-0",
        "sender-0",
        "urn:x-cast:com.google.cast.tp.deviceauth",
        vec![0x08, 0x00],
    );
    let fields = field_numbers(&encode(&msg));
    for tag in [1, 2, 3, 4, 5] {
        assert!(
            fields.contains(&tag),
            "required tag {tag} missing; present: {fields:?}"
        );
    }
}

/// An empty `source_id` is still a *written* empty string in proto2. This is the case that
/// would slip past a test that only checked a populated message.
#[test]
fn required_strings_are_written_even_when_empty() {
    let msg = CastMessage {
        protocol_version: ProtocolVersion::Castv210 as i32,
        source_id: String::new(),
        destination_id: String::new(),
        namespace: String::new(),
        payload_type: PayloadType::String as i32,
        payload_utf8: None,
        payload_binary: None,
    };
    let fields = field_numbers(&encode(&msg));
    assert_eq!(
        fields,
        vec![1, 2, 3, 4, 5],
        "every required field must appear even when all of them are default-valued"
    );
}

/// `AuthResponse`'s two required fields, and `AuthError`'s one — whose only meaningful
/// value, `INTERNAL_ERROR`, is zero, so an unmarked field would encode to nothing at all
/// and produce an error reply a sender cannot read.
#[test]
fn device_auth_bodies_carry_their_required_fields() {
    let response = AuthResponse {
        signature: Vec::new(),
        client_auth_certificate: Vec::new(),
        intermediate_certificate: Vec::new(),
        signature_algorithm: None,
        sender_nonce: None,
        hash_algorithm: None,
        crl: None,
    };
    assert_eq!(
        field_numbers(&encode(&response)),
        vec![1, 2],
        "signature and client_auth_certificate are required"
    );

    let error = AuthError {
        error_type: auth_error::ErrorType::InternalError as i32,
    };
    assert_eq!(
        field_numbers(&encode(&error)),
        vec![1],
        "error_type is required and INTERNAL_ERROR is zero"
    );

    // The enclosing message is all-optional, so an absent branch stays absent.
    let empty = DeviceAuthMessage {
        challenge: None,
        response: None,
        error: None,
    };
    assert!(field_numbers(&encode(&empty)).is_empty());
}
