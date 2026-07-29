//! A minimal PEM reader.
//!
//! Only enough to turn the checked-in certificates and the network path's `pri`
//! field into DER. Deliberately not a dependency: the whole grammar this needs is
//! "base64 between two labelled markers", and the alternative crates each pull a
//! parser stack in for that.

use base64::Engine as _;

use crate::CksError;

/// Decode every block carrying `label` from a PEM document, in order.
///
/// # Errors
/// [`CksError::Pem`] if a block is unterminated or its body is not base64.
pub fn decode_all(pem: &str, label: &str) -> Result<Vec<Vec<u8>>, CksError> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(&begin) {
        let body_start = start + begin.len();
        let body_end = rest[body_start..]
            .find(&end)
            .ok_or_else(|| CksError::Pem(format!("unterminated {label} block")))?
            + body_start;
        let body: String = rest[body_start..body_end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        out.push(
            base64::engine::general_purpose::STANDARD
                .decode(body.as_bytes())
                .map_err(|e| CksError::Pem(format!("{label} block is not base64: {e}")))?,
        );
        rest = &rest[body_end + end.len()..];
    }
    Ok(out)
}

/// Decode a PEM document expected to hold exactly one block of `label`.
///
/// # Errors
/// [`CksError::Pem`] if the count is not one, or a block fails to decode.
pub fn decode_one(pem: &str, label: &str) -> Result<Vec<u8>, CksError> {
    let mut blocks = decode_all(pem, label)?;
    match blocks.len() {
        1 => Ok(blocks.remove(0)),
        n => Err(CksError::Pem(format!(
            "expected exactly one {label} block, found {n}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const TWO: &str = "\
leading noise
-----BEGIN CERTIFICATE-----
aGVsbG8=
-----END CERTIFICATE-----
between
-----BEGIN CERTIFICATE-----
d29ybGQ=
-----END CERTIFICATE-----
trailing
";

    #[test]
    fn decodes_each_block_in_order() {
        let blocks = decode_all(TWO, "CERTIFICATE").unwrap();
        assert_eq!(blocks, vec![b"hello".to_vec(), b"world".to_vec()]);
    }

    #[test]
    fn decode_one_rejects_a_document_with_two_blocks() {
        assert!(decode_one(TWO, "CERTIFICATE").is_err());
    }

    #[test]
    fn a_missing_end_marker_is_an_error_not_a_silent_truncation() {
        let bad = "-----BEGIN CERTIFICATE-----\naGk=\n";
        assert!(decode_all(bad, "CERTIFICATE").is_err());
    }

    #[test]
    fn an_absent_label_yields_nothing() {
        assert!(decode_all(TWO, "PRIVATE KEY").unwrap().is_empty());
    }
}
