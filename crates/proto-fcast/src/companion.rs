//! FCompanion: media bytes served **by the sender**, over the control connection (#249).
//!
//! The one plane in FCast where the arrow points backwards. Everywhere else the receiver
//! fetches a URL; here the sender says "the file is on my phone" — a local video Grayjay
//! is casting, or a manifest a terminal sender piped in — and hands out
//! `fcomp://<provider>.fcast/<resource>` URLs that only mean anything to the connection
//! that issued them.
//!
//! This module is the pure half (ground rule 3): the URL grammar, the `Resource` opcode's
//! custom binary body, and the part-reassembly rules. The reads themselves are the
//! adapter's, and what the *decoder* opens is an ordinary `http://` URL on our own shared
//! host — see [`crate::content`] — because libavformat has never heard of `fcomp://` and
//! teaching it would mean an AVIO callback where a loopback socket does.
//!
//! Wire format, from the v4 spec's FCompanion section: every multi-byte integer is
//! little-endian, and a `Resource` body is
//!
//! ```text
//! U32LE request_id | U8 part | U8 total_parts | U8 variant | [payload]
//! ```
//!
//! where `variant` is 0 for "not found" and 1 for success, the payload running to the end
//! of the body. Parts start at 0 and `total_parts` is a `U8`, which is what caps a single
//! read: the requester must keep each range small enough to arrive in at most 255 of them.

use crate::error::FCastError;

/// The scheme senders use for resources they serve themselves.
pub const COMPANION_SCHEME: &str = "fcomp://";

/// The host suffix the spec fixes: `fcomp://<provider-id>.fcast/<resource-id>`.
const COMPANION_HOST_SUFFIX: &str = ".fcast";

/// A resource one connected sender is offering.
///
/// Two integers rather than a string because that is what they are on the wire, and
/// because the provider id is a *routing* decision — the read is issued to the connection
/// that owns it, which may not be the connection that asked us to play it. The spec
/// requires supporting exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompanionUrl {
    /// The provider id the receiver handed out in `CompanionHelloResponse`.
    pub provider: u16,
    /// The resource id, chosen by that sender.
    pub resource: u32,
}

impl CompanionUrl {
    /// Parse `fcomp://<provider-id>.fcast/<resource-id>`.
    ///
    /// Strict, deliberately: both ids are rendered as ASCII decimal digits and neither is
    /// allowed to be a different width than its type. A URL we misparse is a read routed
    /// to the wrong sender.
    ///
    /// # Errors
    /// [`FCastError::MalformedCompanionUrl`] for a URL of any other shape.
    pub fn parse(url: &str) -> Result<Self, FCastError> {
        let malformed = || FCastError::MalformedCompanionUrl(url.to_owned());
        let rest = url.strip_prefix(COMPANION_SCHEME).ok_or_else(malformed)?;
        let (host, resource) = rest.split_once('/').ok_or_else(malformed)?;
        let provider = host
            .strip_suffix(COMPANION_HOST_SUFFIX)
            .ok_or_else(malformed)?;
        // Digits and nothing else. `u16::from_str` accepts a leading `+`, which the spec's
        // "rendered as ASCII decimal digits" does not — and a URL we accept in a spelling
        // the sender cannot have written is one more shape to reason about for nothing.
        if !decimal(provider) || !decimal(resource) {
            return Err(malformed());
        }
        Ok(Self {
            provider: provider.parse().map_err(|_| malformed())?,
            resource: resource.parse().map_err(|_| malformed())?,
        })
    }

    /// Render this back as the URL a sender would have written.
    #[must_use]
    pub fn to_url(self) -> String {
        format!(
            "{COMPANION_SCHEME}{}{COMPANION_HOST_SUFFIX}/{}",
            self.provider, self.resource
        )
    }
}

/// Non-empty and all ASCII digits — the spec's rendering, exactly.
fn decimal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// What a sender answered a resource read with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceResult {
    /// The sender has no such resource. Terminal for the whole request.
    NotFound,
    /// One part's worth of bytes.
    Data(Vec<u8>),
}

/// One decoded `Resource` packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePart {
    /// Which read this answers.
    pub request_id: u32,
    /// This part's index, counting from 0.
    pub part: u8,
    /// How many parts the answer has in total.
    pub total: u8,
    /// The part itself.
    pub result: ResourceResult,
}

/// The fixed header of a `Resource` body: request id, part, total, variant.
const RESOURCE_HEADER: usize = 4 + 1 + 1 + 1;

/// Parse a `Resource` (opcode 21) body.
///
/// # Errors
/// [`FCastError::MalformedResource`] for a body too short to hold the header, or one
/// whose variant byte is not a value the spec defines. Both are session-fatal in the
/// reference: this is not a message with an error reply, it is a framing fault on a plane
/// we are mid-read on.
pub fn parse_resource(body: &[u8]) -> Result<ResourcePart, FCastError> {
    let Some(header) = body.first_chunk::<RESOURCE_HEADER>() else {
        return Err(FCastError::MalformedResource(format!(
            "a Resource body of {} bytes cannot hold its header",
            body.len()
        )));
    };
    let request_id = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let result = match header[6] {
        0x00 => ResourceResult::NotFound,
        0x01 => ResourceResult::Data(body[RESOURCE_HEADER..].to_vec()),
        other => {
            return Err(FCastError::MalformedResource(format!(
                "GetResourceResult variant {other:#04x} is not one this version defines"
            )))
        }
    };
    Ok(ResourcePart {
        request_id,
        part: header[4],
        total: header[5],
        result,
    })
}

/// Encode a `Resource` body — the sender's side of the wire.
///
/// Only a test sender ever needs this (the receiver reads these, it does not write them),
/// and it is here rather than in the tests so the encoder and the parser cannot drift:
/// a round-trip fixture that builds its own bytes proves only that the parser agrees with
/// itself.
#[must_use]
pub fn encode_resource(part: &ResourcePart) -> Vec<u8> {
    let mut out = Vec::with_capacity(RESOURCE_HEADER + 16);
    out.extend_from_slice(&part.request_id.to_le_bytes());
    out.push(part.part);
    out.push(part.total);
    match &part.result {
        ResourceResult::NotFound => out.push(0x00),
        ResourceResult::Data(bytes) => {
            out.push(0x01);
            out.extend_from_slice(bytes);
        }
    }
    out
}

/// Accumulates the parts of one read.
///
/// A type rather than a `Vec<u8>` and some care at the call site, because the ordering
/// rule is the whole content: parts start at 0 and arrive in sequence, so a part out of
/// order means the stream is not the answer we think it is — and silently accepting it
/// would splice a later chunk of the file into an earlier position, which decodes as
/// corruption rather than as an error.
#[derive(Debug, Default)]
pub struct ResourceRead {
    total: Option<u8>,
    next: u8,
    data: Vec<u8>,
}

/// What feeding a part to a [`ResourceRead`] concluded.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadProgress {
    /// More parts to come.
    More,
    /// Every part arrived; here is the range.
    Complete(Vec<u8>),
    /// The sender says there is no such resource.
    NotFound,
}

impl ResourceRead {
    /// A read with nothing accumulated.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            total: None,
            next: 0,
            data: Vec::new(),
        }
    }

    /// Feed one part.
    ///
    /// # Errors
    /// [`FCastError::MalformedResource`] if the part is out of sequence, if the total
    /// changed mid-answer, or if the answer claims zero parts.
    pub fn push(&mut self, part: ResourcePart) -> Result<ReadProgress, FCastError> {
        if let ResourceResult::NotFound = part.result {
            return Ok(ReadProgress::NotFound);
        }
        if part.total == 0 {
            return Err(FCastError::MalformedResource(
                "a Resource answer with zero parts".into(),
            ));
        }
        match self.total {
            None => self.total = Some(part.total),
            Some(total) if total != part.total => {
                return Err(FCastError::MalformedResource(format!(
                    "the answer changed its part count from {total} to {}",
                    part.total
                )))
            }
            Some(_) => {}
        }
        if part.part != self.next {
            return Err(FCastError::MalformedResource(format!(
                "part {} arrived where part {} was expected",
                part.part, self.next
            )));
        }
        if let ResourceResult::Data(bytes) = part.result {
            self.data.extend_from_slice(&bytes);
        }
        self.next = self.next.saturating_add(1);
        if self.next == part.total {
            return Ok(ReadProgress::Complete(std::mem::take(&mut self.data)));
        }
        Ok(ReadProgress::More)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The grammar the spec fixes, and the shapes that are not it.
    #[test]
    fn companion_urls_parse_exactly_the_documented_shape() {
        let url = CompanionUrl::parse("fcomp://3.fcast/17").unwrap();
        assert_eq!(
            url,
            CompanionUrl {
                provider: 3,
                resource: 17
            }
        );
        assert_eq!(url.to_url(), "fcomp://3.fcast/17");
        // The widths are the wire's: a provider past u16 or a resource past u32 is a URL
        // we would truncate, and a truncated provider routes the read to another sender.
        assert!(CompanionUrl::parse("fcomp://65536.fcast/1").is_err());
        assert!(CompanionUrl::parse("fcomp://1.fcast/4294967296").is_err());
        // …and everything else.
        assert!(CompanionUrl::parse("fcomp://3/17").is_err(), "no .fcast");
        assert!(CompanionUrl::parse("fcomp://3.fcast").is_err(), "no path");
        assert!(CompanionUrl::parse("http://3.fcast/17").is_err());
        assert!(CompanionUrl::parse("fcomp://+3.fcast/17").is_err());
        assert!(CompanionUrl::parse("fcomp://.fcast/17").is_err());
    }

    #[test]
    fn a_resource_body_round_trips() {
        let part = ResourcePart {
            request_id: 0xdead_beef,
            part: 2,
            total: 5,
            result: ResourceResult::Data(b"hello".to_vec()),
        };
        let bytes = encode_resource(&part);
        // Little-endian, as the spec says for every multi-byte integer here.
        assert_eq!(&bytes[..4], &[0xef, 0xbe, 0xad, 0xde]);
        assert_eq!(&bytes[4..7], &[2, 5, 0x01]);
        assert_eq!(parse_resource(&bytes).unwrap(), part);

        let missing = ResourcePart {
            request_id: 1,
            part: 0,
            total: 1,
            result: ResourceResult::NotFound,
        };
        assert_eq!(parse_resource(&encode_resource(&missing)).unwrap(), missing);
    }

    #[test]
    fn a_truncated_or_unknown_resource_body_is_a_fault() {
        assert!(matches!(
            parse_resource(&[1, 2, 3, 4, 5, 6]),
            Err(FCastError::MalformedResource(_))
        ));
        // Variant 2 is not defined by this version; guessing at it would be inventing a
        // payload length.
        assert!(matches!(
            parse_resource(&[1, 0, 0, 0, 0, 1, 2]),
            Err(FCastError::MalformedResource(_))
        ));
    }

    /// The happy path: three parts, in order, concatenated in the order they arrived.
    #[test]
    fn parts_reassemble_in_order() {
        let mut read = ResourceRead::new();
        for (index, chunk) in [b"one".as_slice(), b"two", b"three"].iter().enumerate() {
            let part = ResourcePart {
                request_id: 9,
                part: u8::try_from(index).unwrap(),
                total: 3,
                result: ResourceResult::Data(chunk.to_vec()),
            };
            match read.push(part).unwrap() {
                ReadProgress::More => assert!(index < 2),
                ReadProgress::Complete(data) => {
                    assert_eq!(index, 2);
                    assert_eq!(data, b"onetwothree");
                }
                ReadProgress::NotFound => panic!("not what was sent"),
            }
        }
    }

    /// A part out of sequence is an error, not a hole to paper over: accepting it would
    /// splice a later chunk of the file into an earlier position, and the failure would
    /// then surface as a corrupt picture with nothing in any log.
    #[test]
    fn an_out_of_order_part_is_refused() {
        let mut read = ResourceRead::new();
        let part = |part: u8| ResourcePart {
            request_id: 9,
            part,
            total: 3,
            result: ResourceResult::Data(vec![part]),
        };
        assert_eq!(read.push(part(0)).unwrap(), ReadProgress::More);
        assert!(matches!(
            read.push(part(2)),
            Err(FCastError::MalformedResource(_))
        ));
    }

    /// A total that changes mid-answer, and a total of zero. Neither is a length we
    /// could act on.
    #[test]
    fn a_nonsense_part_count_is_refused() {
        let mut read = ResourceRead::new();
        assert!(matches!(
            read.push(ResourcePart {
                request_id: 1,
                part: 0,
                total: 0,
                result: ResourceResult::Data(vec![]),
            }),
            Err(FCastError::MalformedResource(_))
        ));

        let mut read = ResourceRead::new();
        read.push(ResourcePart {
            request_id: 1,
            part: 0,
            total: 3,
            result: ResourceResult::Data(vec![1]),
        })
        .unwrap();
        assert!(matches!(
            read.push(ResourcePart {
                request_id: 1,
                part: 1,
                total: 4,
                result: ResourceResult::Data(vec![2]),
            }),
            Err(FCastError::MalformedResource(_))
        ));
    }

    /// "No such resource" ends the read wherever it arrives, and does not become bytes.
    #[test]
    fn not_found_ends_the_read() {
        let mut read = ResourceRead::new();
        assert_eq!(
            read.push(ResourcePart {
                request_id: 1,
                part: 0,
                total: 1,
                result: ResourceResult::NotFound,
            })
            .unwrap(),
            ReadProgress::NotFound
        );
    }
}
