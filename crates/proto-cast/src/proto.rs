//! The CASTv2 wire messages, hand-written as `prost` types.
//!
//! We derive these by hand rather than compiling `cast_channel.proto` with
//! `prost-build` so the build needs no `protoc` binary — a win for reproducible Nix
//! cross-builds (DECISION-LOG D9). The tags/types match Google's `cast_channel.proto`
//! exactly, so the encoding is wire-compatible with real senders.

/// Protocol version enum (only `CASTV2_1_0` exists).
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum ProtocolVersion {
    /// The only version.
    Castv210 = 0,
}

/// Whether the payload is in `payload_utf8` (JSON) or `payload_binary` (protobuf).
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum PayloadType {
    /// UTF-8 JSON in `payload_utf8`.
    String = 0,
    /// Protobuf bytes in `payload_binary`.
    Binary = 1,
}

/// The CASTv2 envelope: every message on the TLS channel is one of these,
/// length-prefixed (see [`crate::framing`]).
#[derive(Clone, PartialEq, prost::Message)]
pub struct CastMessage {
    /// Protocol version.
    #[prost(enumeration = "ProtocolVersion", tag = "1")]
    pub protocol_version: i32,
    /// Virtual-connection source id (`sender-0`, `receiver-0`, a transport id…).
    #[prost(string, tag = "2")]
    pub source_id: String,
    /// Virtual-connection destination id.
    #[prost(string, tag = "3")]
    pub destination_id: String,
    /// The namespace URN this message belongs to.
    #[prost(string, tag = "4")]
    pub namespace: String,
    /// Which payload field carries the body.
    #[prost(enumeration = "PayloadType", tag = "5")]
    pub payload_type: i32,
    /// JSON body (when `payload_type == String`).
    #[prost(string, optional, tag = "6")]
    pub payload_utf8: Option<String>,
    /// Binary body (when `payload_type == Binary`).
    #[prost(bytes = "vec", optional, tag = "7")]
    pub payload_binary: Option<Vec<u8>>,
}

/// Signature algorithms for device auth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum SignatureAlgorithm {
    /// Unspecified.
    Unspecified = 0,
    /// RSASSA PKCS#1 v1.5.
    RsassaPkcs1v15 = 1,
    /// RSASSA-PSS.
    RsassaPss = 2,
}

/// Hash algorithms for device auth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum HashAlgorithm {
    /// SHA-1 (legacy).
    Sha1 = 0,
    /// SHA-256.
    Sha256 = 1,
}

/// The sender's challenge (sent on the `deviceauth` namespace, first thing).
#[derive(Clone, PartialEq, prost::Message)]
pub struct AuthChallenge {
    /// Requested signature algorithm.
    #[prost(enumeration = "SignatureAlgorithm", optional, tag = "1")]
    pub signature_algorithm: Option<i32>,
    /// Sender nonce to be echoed/signed.
    #[prost(bytes = "vec", optional, tag = "2")]
    pub sender_nonce: Option<Vec<u8>>,
    /// Requested hash algorithm.
    #[prost(enumeration = "HashAlgorithm", optional, tag = "3")]
    pub hash_algorithm: Option<i32>,
}

/// The receiver's device-auth response: cert chain + signature.
#[derive(Clone, PartialEq, prost::Message)]
pub struct AuthResponse {
    /// Signature over the TLS-cert-hash (+ nonce), per the chosen algorithm.
    #[prost(bytes = "vec", tag = "1")]
    pub signature: Vec<u8>,
    /// The device (leaf) certificate, DER.
    #[prost(bytes = "vec", tag = "2")]
    pub client_auth_certificate: Vec<u8>,
    /// Intermediate certificates, DER.
    #[prost(bytes = "vec", repeated, tag = "3")]
    pub intermediate_certificate: Vec<Vec<u8>>,
    /// Signature algorithm actually used.
    #[prost(enumeration = "SignatureAlgorithm", optional, tag = "4")]
    pub signature_algorithm: Option<i32>,
    /// Echoed sender nonce.
    #[prost(bytes = "vec", optional, tag = "5")]
    pub sender_nonce: Option<Vec<u8>>,
    /// Hash algorithm actually used.
    #[prost(enumeration = "HashAlgorithm", optional, tag = "6")]
    pub hash_algorithm: Option<i32>,
    /// Optional CRL.
    #[prost(bytes = "vec", optional, tag = "7")]
    pub crl: Option<Vec<u8>>,
}

/// A device-auth error the receiver can return instead of a response.
#[derive(Clone, PartialEq, prost::Message)]
pub struct AuthError {
    /// The error type.
    #[prost(enumeration = "auth_error::ErrorType", tag = "1")]
    pub error_type: i32,
}

/// Nested types for [`AuthError`].
pub mod auth_error {
    /// Device-auth error kinds.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
    #[repr(i32)]
    pub enum ErrorType {
        /// Internal error.
        InternalError = 0,
        /// No TLS.
        NoTls = 1,
        /// Signature algorithm unavailable.
        SignatureAlgorithmUnavailable = 2,
    }
}

/// The `deviceauth` namespace envelope: exactly one of challenge/response/error.
#[derive(Clone, PartialEq, prost::Message)]
pub struct DeviceAuthMessage {
    /// Present on the sender→receiver challenge.
    #[prost(message, optional, tag = "1")]
    pub challenge: Option<AuthChallenge>,
    /// Present on the receiver→sender response.
    #[prost(message, optional, tag = "2")]
    pub response: Option<AuthResponse>,
    /// Present on an error response.
    #[prost(message, optional, tag = "3")]
    pub error: Option<AuthError>,
}

impl CastMessage {
    /// Build a JSON (`String` payload-type) message.
    #[must_use]
    pub fn json(source_id: &str, destination_id: &str, namespace: &str, json: String) -> Self {
        Self {
            protocol_version: ProtocolVersion::Castv210 as i32,
            source_id: source_id.to_string(),
            destination_id: destination_id.to_string(),
            namespace: namespace.to_string(),
            payload_type: PayloadType::String as i32,
            payload_utf8: Some(json),
            payload_binary: None,
        }
    }

    /// Build a binary (`Binary` payload-type) message.
    #[must_use]
    pub fn binary(source_id: &str, destination_id: &str, namespace: &str, bytes: Vec<u8>) -> Self {
        Self {
            protocol_version: ProtocolVersion::Castv210 as i32,
            source_id: source_id.to_string(),
            destination_id: destination_id.to_string(),
            namespace: namespace.to_string(),
            payload_type: PayloadType::Binary as i32,
            payload_utf8: None,
            payload_binary: Some(bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use prost::Message;

    #[test]
    fn cast_message_roundtrips() {
        let msg = CastMessage::json("sender-0", "receiver-0", "urn:x-cast:test", "{}".into());
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let back = CastMessage::decode(&buf[..]).unwrap();
        assert_eq!(back, msg);
        assert_eq!(back.payload_utf8.as_deref(), Some("{}"));
    }

    #[test]
    fn device_auth_message_roundtrips() {
        let dam = DeviceAuthMessage {
            challenge: Some(AuthChallenge {
                signature_algorithm: Some(SignatureAlgorithm::RsassaPkcs1v15 as i32),
                sender_nonce: Some(vec![1, 2, 3]),
                hash_algorithm: Some(HashAlgorithm::Sha256 as i32),
            }),
            response: None,
            error: None,
        };
        let mut buf = Vec::new();
        dam.encode(&mut buf).unwrap();
        let back = DeviceAuthMessage::decode(&buf[..]).unwrap();
        assert_eq!(back.challenge.unwrap().sender_nonce, Some(vec![1, 2, 3]));
    }
}
