//! The Cast device CRL: fetching it, and deciding whether it is safe to serve.
//!
//! ## Why a receiver serves one at all
//!
//! `AuthResponse.crl` is optional by the letter of the protocol, and Google Chrome
//! treats it that way — a receiver that sends nothing is carried by Chrome's built-in
//! fallback CRL and authenticates fine. **Chromium does not.** Measured on one box,
//! minutes apart, against the same receiver with the field empty:
//!
//! ```text
//! Chromium 148  0 auth successes, 4 auth failures, 13 "CRL - Not time-valid"
//! Chrome   150  1 auth success,   0 auth failures,  0 "CRL - Not time-valid"
//! ```
//!
//! There, `ParseAndVerifyFallbackCRL` fails, `fallback_crl` is null, and
//! `cast_cert_validator.cc` returns `ERR_FALLBACK_CRL_INVALID` — the channel is dropped
//! and retried forever, which from the room is a receiver that does not exist. Supplying
//! a CRL skips that path entirely, because a non-null `crl` means the fallback is never
//! consulted. So this is what makes the panel usable from a Chromium-based sender.
//!
//! The bytes are public and unauthenticated: `clients3.google.com/cast/chromecast/device/crl`
//! redirects to `gstatic.com/cast-crl/latest`, and what it returns is byte-identical
//! (same SHA-256) to what real Cast hardware on the LAN answers the same challenge with.
//! Devices are mirroring gstatic; there is no per-device material here.
//!
//! ## Why serving one is not free, and what [`ServableCrl`] is for
//!
//! Sending a CRL changes what a revocation *means to us*. With the field empty, the only
//! revocation data a sender has is its built-in fallback — a 2023 snapshot that cannot
//! name an identity revoked since. We are, by accident, immune. Send the live CRL and
//! `CheckRevocation` runs against current data, and `ERR_CERTS_REVOKED` is fatal under
//! `CRL_REQUIRED_WITH_FALLBACK` (only `CRL_OPTIONAL` downgrades it to a flag). The day
//! Google lists the replayed identity, a receiver that helpfully attaches the CRL is
//! handing every sender the document that rejects it — and would go from *working in
//! Chrome* to *working nowhere*.
//!
//! That is D41's named risk, volunteered for. So the decision is made a type: a
//! [`CastCrl`] is just parsed bytes, and only [`CastCrl::servable_for`] — which is handed
//! the chain we actually present — can produce the [`ServableCrl`] that
//! `AuthResponse.crl` is filled from. There is no path that attaches a CRL without first
//! asking whether it revokes us, and when it does the answer is to withhold it and keep
//! the Chrome-only behaviour rather than lose both.
//!
//! It also closes the other half of D41. "No way for the receiver to see it coming"
//! stops being true: the check that withholds the CRL is the same check that can say, in
//! a log line, that the identity this panel presents has been revoked.
//!
//! ## The revocation rules, as the senders implement them
//!
//! From `cast_crl.cc`'s `CastCRLImpl::CheckRevocation`, which is what both openscreen and
//! Chrome run:
//!
//! * every certificate in the trusted chain (including the trust anchor) is hashed as
//!   `SHA-256(spki_tlv)` — the DER of the whole `SubjectPublicKeyInfo`, not the key bits
//!   — and a match in `revoked_public_key_hashes` revokes the chain;
//! * for each certificate after the first, that same hash is looked up in
//!   `revoked_serial_number_ranges`, and the *subordinate* certificate's serial number
//!   falling inside any range revokes the chain.
//!
//! Both are implemented here, against the chain we would present, before it can be
//! served.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::window::Window;
use crate::ReplayError;

/// Where the CRL comes from. The `clients3` URL is what the reference receivers use; it
/// answers with a 302 to gstatic, which is where the bytes actually live.
pub const CRL_URL: &str = "https://clients3.google.com/cast/chromecast/device/crl";

/// Filename under the state directory.
pub const CACHE_FILE: &str = "cast-crl.bin";

/// A ceiling on the response. The real document is a few kilobytes; this is a public
/// endpoint and an unbounded read would let a bad day upstream exhaust the panel.
const MAX_CRL_BYTES: u64 = 256 * 1024;

/// The wire messages, decoded with `prost` the same way `proto-cast` carries
/// `cast_channel.proto` — hand-written so the build needs no `protoc` (D9).
///
/// Everything is `optional` because everything here is *decoded* and never encoded. That
/// distinction is load-bearing: proto2 requiredness only matters on the encode side, and
/// getting it wrong there is what made every message this receiver sent unparseable to
/// Chrome (see `proto-cast/tests/proto2_required.rs`). Nothing in this module encodes a
/// CRL, so nothing here can repeat it.
mod wire {
    /// `CrlBundle` — the top-level document.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct CrlBundle {
        /// The CRLs in the bundle. Real ones carry exactly one.
        #[prost(message, repeated, tag = "1")]
        pub crls: Vec<Crl>,
    }

    /// One CRL: the signed body, its signer, and the signature over it.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Crl {
        /// The serialized [`TbsCrl`] the signature covers.
        #[prost(bytes = "vec", optional, tag = "1")]
        pub tbs_crl: Option<Vec<u8>>,
        /// The certificate that signed it.
        #[prost(bytes = "vec", optional, tag = "2")]
        pub signer_cert: Option<Vec<u8>>,
        /// RSASSA PKCS#1 v1.5 over SHA-256.
        #[prost(bytes = "vec", optional, tag = "3")]
        pub signature: Option<Vec<u8>>,
    }

    /// The signed body: a validity window and the revocations in force during it.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct TbsCrl {
        /// Format version. Senders reject anything but 0.
        #[prost(uint64, optional, tag = "1")]
        pub version: Option<u64>,
        /// Start of validity, Unix seconds.
        #[prost(uint64, optional, tag = "2")]
        pub not_before_seconds: Option<u64>,
        /// End of validity, Unix seconds.
        #[prost(uint64, optional, tag = "3")]
        pub not_after_seconds: Option<u64>,
        /// `SHA-256(spki_tlv)` of each revoked public key.
        #[prost(bytes = "vec", repeated, tag = "4")]
        pub revoked_public_key_hashes: Vec<Vec<u8>>,
        /// Serial ranges revoked under a given issuer.
        #[prost(message, repeated, tag = "5")]
        pub revoked_serial_number_ranges: Vec<SerialNumberRange>,
    }

    /// A contiguous run of revoked serial numbers under one issuer.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SerialNumberRange {
        /// `SHA-256(spki_tlv)` of the issuing key the range applies to.
        #[prost(bytes = "vec", optional, tag = "1")]
        pub issuer_public_key_hash: Option<Vec<u8>>,
        /// First revoked serial, inclusive.
        #[prost(uint64, optional, tag = "2")]
        pub first_serial_number: Option<u64>,
        /// Last revoked serial, inclusive.
        #[prost(uint64, optional, tag = "3")]
        pub last_serial_number: Option<u64>,
    }
}

/// A parsed Cast device CRL.
///
/// Holds the raw bytes because those — not a re-encoding — are what goes on the wire: the
/// signature covers the exact `tbs_crl` we were handed, and this module never rebuilds it.
#[derive(Debug, Clone)]
pub struct CastCrl {
    raw: Vec<u8>,
    window: Window,
    revoked_keys: HashSet<[u8; 32]>,
    revoked_serials: HashMap<[u8; 32], Vec<(u64, u64)>>,
}

/// Why a chain is revoked, for the log line that says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revocation {
    /// A certificate's public key is listed outright.
    PublicKey {
        /// Position in the chain, leaf first.
        index: usize,
    },
    /// A certificate's serial falls in a revoked range under its issuer.
    Serial {
        /// Position of the revoked (subordinate) certificate, leaf first.
        index: usize,
        /// The serial that matched.
        serial: u64,
    },
}

impl std::fmt::Display for Revocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PublicKey { index } => {
                write!(f, "the public key of chain certificate {index} is revoked")
            }
            Self::Serial { index, serial } => write!(
                f,
                "chain certificate {index} has serial {serial}, inside a revoked range"
            ),
        }
    }
}

/// A CRL that has been checked against the chain this receiver presents and found not to
/// revoke it — the only thing `AuthResponse.crl` is ever filled from.
///
/// Constructible only through [`CastCrl::servable_for`], so "did we check whether this
/// document rejects us" is answered by the type rather than by remembering to ask.
#[derive(Debug, Clone)]
pub struct ServableCrl(Vec<u8>);

impl ServableCrl {
    /// The bytes to put in `AuthResponse.crl`.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consume it, yielding the bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl CastCrl {
    /// Parse a fetched or cached CRL bundle.
    ///
    /// # Errors
    /// [`ReplayError::Response`] if the bundle is malformed, empty, carries a version this
    /// does not understand, or has no usable validity window.
    pub fn parse(raw: &[u8]) -> Result<Self, ReplayError> {
        use prost::Message as _;

        let bundle = wire::CrlBundle::decode(raw)
            .map_err(|e| ReplayError::Response(format!("CRL bundle: {e}")))?;
        let crl = bundle
            .crls
            .first()
            .ok_or_else(|| ReplayError::Response("CRL bundle carries no CRL".into()))?;
        let tbs_bytes = crl
            .tbs_crl
            .as_ref()
            .ok_or_else(|| ReplayError::Response("CRL carries no signed body".into()))?;
        let tbs = wire::TbsCrl::decode(&tbs_bytes[..])
            .map_err(|e| ReplayError::Response(format!("CRL body: {e}")))?;

        // Senders reject any other version outright, so a bundle we cannot read the same
        // way they do is one we must not serve.
        if tbs.version.unwrap_or(0) != 0 {
            return Err(ReplayError::Response(format!(
                "unsupported CRL version {:?}",
                tbs.version
            )));
        }

        let (Some(not_before), Some(not_after)) = (tbs.not_before_seconds, tbs.not_after_seconds)
        else {
            return Err(ReplayError::Response("CRL has no validity window".into()));
        };
        let window = Window::new(
            i64::try_from(not_before)
                .map_err(|e| ReplayError::Response(format!("CRL not_before: {e}")))?,
            i64::try_from(not_after)
                .map_err(|e| ReplayError::Response(format!("CRL not_after: {e}")))?,
        )?;

        // A hash that is not 32 bytes cannot match anything we compute, so dropping it
        // keeps the comparison total instead of carrying a shape that cannot be compared.
        let revoked_keys: HashSet<[u8; 32]> = tbs
            .revoked_public_key_hashes
            .iter()
            .filter_map(|h| <[u8; 32]>::try_from(h.as_slice()).ok())
            .collect();

        let mut revoked_serials: HashMap<[u8; 32], Vec<(u64, u64)>> = HashMap::new();
        for range in &tbs.revoked_serial_number_ranges {
            let (Some(hash), Some(first), Some(last)) = (
                range
                    .issuer_public_key_hash
                    .as_ref()
                    .and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok()),
                range.first_serial_number,
                range.last_serial_number,
            ) else {
                continue;
            };
            revoked_serials.entry(hash).or_default().push((first, last));
        }

        Ok(Self {
            raw: raw.to_vec(),
            window,
            revoked_keys,
            revoked_serials,
        })
    }

    /// The window the CRL is valid for. Senders hard-fail outside it, so a CRL past its
    /// end is worse than none.
    #[must_use]
    pub const fn window(&self) -> Window {
        self.window
    }

    /// How many public keys this CRL revokes.
    #[must_use]
    pub fn revoked_key_count(&self) -> usize {
        self.revoked_keys.len()
    }

    /// How many distinct issuers have revoked serial ranges.
    ///
    /// Not the same as [`Self::revoked_range_count`], and the difference is easy to
    /// misread: the document lists many ranges, but they group under a handful of
    /// issuing keys.
    #[must_use]
    pub fn revoked_issuer_count(&self) -> usize {
        self.revoked_serials.len()
    }

    /// How many revoked serial ranges the CRL lists, across all issuers.
    #[must_use]
    pub fn revoked_range_count(&self) -> usize {
        self.revoked_serials.values().map(Vec::len).sum()
    }

    /// Whether this CRL revokes `chain` — leaf first, trust anchor last, DER.
    ///
    /// Mirrors `CastCRLImpl::CheckRevocation`: a listed public key revokes outright, and a
    /// certificate whose serial falls in a range registered under its *issuer's* key hash
    /// is revoked by serial.
    ///
    /// # Errors
    /// [`ReplayError::Response`] if a certificate in the chain cannot be parsed — which
    /// must not be read as "not revoked", so it is an error rather than a `false`.
    pub fn revokes(&self, chain: &[&[u8]]) -> Result<Option<Revocation>, ReplayError> {
        let mut hashes = Vec::with_capacity(chain.len());
        let mut serials = Vec::with_capacity(chain.len());
        for der in chain {
            let (_, cert) = x509_parser::parse_x509_certificate(der)
                .map_err(|e| ReplayError::Response(format!("CRL chain certificate: {e}")))?;
            hashes.push(spki_hash(cert.tbs_certificate.subject_pki.raw));
            serials.push(serial_u64(cert.raw_serial()));
        }

        for (index, hash) in hashes.iter().enumerate() {
            if self.revoked_keys.contains(hash) {
                return Ok(Some(Revocation::PublicKey { index }));
            }
            // The issuer at `index` carries the ranges that revoke the subordinate below
            // it, which is the certificate at `index - 1`.
            if index > 0 {
                let Some(ranges) = self.revoked_serials.get(hash) else {
                    continue;
                };
                let Some(serial) = serials[index - 1] else {
                    continue;
                };
                if ranges.iter().any(|(lo, hi)| serial >= *lo && serial <= *hi) {
                    return Ok(Some(Revocation::Serial {
                        index: index - 1,
                        serial,
                    }));
                }
            }
        }
        Ok(None)
    }

    /// The CRL to serve alongside `chain`, if serving it is safe at `now`.
    ///
    /// `None` when the CRL is outside its window (a sender hard-fails on that, so it is
    /// worse than sending nothing) or when it revokes `chain` — in which case attaching it
    /// would hand every sender the reason to refuse us, and withholding it leaves the
    /// Chrome-only behaviour intact. The [`Revocation`] is returned so the caller can say
    /// so out loud; it is the only warning this receiver gets.
    ///
    /// # Errors
    /// [`ReplayError::Response`] if the chain cannot be parsed.
    pub fn servable_for(
        &self,
        chain: &[&[u8]],
        now_unix: i64,
    ) -> Result<Result<ServableCrl, ServeRefusal>, ReplayError> {
        if !self.window.contains(now_unix) {
            return Ok(Err(ServeRefusal::OutsideWindow));
        }
        if let Some(revocation) = self.revokes(chain)? {
            return Ok(Err(ServeRefusal::RevokesUs(revocation)));
        }
        Ok(Ok(ServableCrl(self.raw.clone())))
    }
}

/// Why a CRL was not served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeRefusal {
    /// The CRL is not valid at this instant. Senders hard-fail on a stale CRL, so an
    /// expired one must be withheld rather than attached.
    OutsideWindow,
    /// The CRL revokes the chain this receiver presents.
    RevokesUs(Revocation),
}

impl std::fmt::Display for ServeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideWindow => f.write_str("the CRL is outside its validity window"),
            Self::RevokesUs(r) => write!(f, "{r}"),
        }
    }
}

/// `SHA-256` over the `SubjectPublicKeyInfo` TLV, which is what the CRL lists.
fn spki_hash(spki_tlv: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(spki_tlv);
    hasher.finalize().into()
}

/// A certificate serial as the CRL compares them.
///
/// The CRL's ranges are `uint64`, so a serial too long to be one cannot be inside any
/// range; `None` says exactly that rather than truncating into a false match. Leading
/// zero padding (a DER positive-integer artefact) is skipped first.
fn serial_u64(raw: &[u8]) -> Option<u64> {
    let trimmed = raw.strip_prefix(&[0]).unwrap_or(raw);
    if trimmed.len() > 8 {
        return None;
    }
    let mut value = 0u64;
    for byte in trimmed {
        value = (value << 8) | u64::from(*byte);
    }
    Some(value)
}

/// Fetch the current CRL. Blocking; belongs on `spawn_blocking` (ground rule 4).
///
/// Uses the ordinary web trust store rather than the pinned roots the CKS path uses:
/// this is gstatic over public HTTPS, not a reverse-engineered backend, and the document
/// it returns is itself Google-signed and verified by the sender regardless.
///
/// # Errors
/// [`ReplayError::Http`] if the endpoint cannot be reached or read.
pub fn fetch_blocking(timeout: Duration) -> Result<Vec<u8>, ReplayError> {
    use std::io::Read as _;

    let agent = ureq::builder().timeout(timeout).build();
    let response = agent
        .get(CRL_URL)
        .call()
        // `ureq::Error` is large; flatten it rather than carry it outward.
        .map_err(|e| ReplayError::Http(format!("fetching the Cast CRL: {e}")))?;

    let mut body = Vec::new();
    let mut reader = response.into_reader().take(MAX_CRL_BYTES);
    reader
        .read_to_end(&mut body)
        .map_err(|e| ReplayError::Http(format!("reading the Cast CRL: {e}")))?;
    Ok(body)
}

/// The default cache path: `<state>/cast-crl.bin`.
#[must_use]
pub fn default_cache_path() -> std::path::PathBuf {
    castaway_paths::host().state().join(CACHE_FILE)
}

/// Read a cached CRL, if one is there and parses.
///
/// Stored as the raw bundle: the window and the revocations are all inside it, so there
/// is nothing to keep in sync alongside, and the file is the same bytes a sender is given.
///
/// # Errors
/// [`ReplayError::Cache`] if the file exists but cannot be read.
pub fn read_cache(path: &std::path::Path) -> Result<Option<CastCrl>, ReplayError> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ReplayError::Cache(format!(
                "reading {}: {e}",
                path.display()
            )))
        }
    };
    match CastCrl::parse(&raw) {
        Ok(crl) => Ok(Some(crl)),
        Err(e) => {
            // A corrupt cache is not fatal — it is a file we can refetch.
            tracing::warn!(error = %e, path = %path.display(), "ignoring an unreadable cached Cast CRL");
            Ok(None)
        }
    }
}

/// Write a fetched CRL to the cache.
///
/// # Errors
/// [`ReplayError::Cache`] if the file cannot be written.
pub fn write_cache(path: &std::path::Path, raw: &[u8]) -> Result<(), ReplayError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ReplayError::Cache(format!("creating {}: {e}", parent.display())))?;
    }
    std::fs::write(path, raw)
        .map_err(|e| ReplayError::Cache(format!("writing {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The real document, fetched from `gstatic.com/cast-crl/latest` and byte-identical
    /// (same SHA-256) to what two different Cast devices on the LAN answered a challenge
    /// with. Checked in so every assertion below runs offline.
    const REAL_CRL: &[u8] = include_bytes!("../fixtures/cast-crl-latest.bin");

    /// The chain this receiver presents when replaying the CKS identity.
    fn chain() -> Vec<Vec<u8>> {
        let device =
            crate::pem::decode_all(include_str!("../fixtures/device_cert.pem"), "CERTIFICATE")
                .unwrap();
        let ica =
            crate::pem::decode_all(include_str!("../fixtures/ica.pem"), "CERTIFICATE").unwrap();
        let mut chain = device;
        chain.extend(ica);
        chain
    }

    fn refs(chain: &[Vec<u8>]) -> Vec<&[u8]> {
        chain.iter().map(Vec::as_slice).collect()
    }

    #[test]
    fn the_real_crl_parses_with_the_shape_it_was_captured_with() {
        let crl = CastCrl::parse(REAL_CRL).unwrap();
        // 2026-07-28T20:24:28Z .. 2026-08-05T08:24:28Z — about seven and a half days,
        // which is why this is fetched and cached rather than compiled in.
        assert_eq!(crl.window().start_unix(), 1_785_270_268);
        assert_eq!(crl.window().end_unix(), 1_785_918_268);
        assert_eq!(crl.revoked_key_count(), 24);
        // 33 ranges, but only 4 issuing keys carry them — the two counts are different
        // questions and reading the range count as an issuer count is an easy mistake.
        assert_eq!(crl.revoked_range_count(), 33);
        assert_eq!(crl.revoked_issuer_count(), 4);
    }

    /// AirServer's chain: a different device, a different intermediate, and a different
    /// root (`Widevine Cast Subroot` rather than `Eureka Root CA`).
    fn airserver_chain() -> Vec<Vec<u8>> {
        vec![
            include_bytes!("../fixtures/airserver/airserver_device_crt.der").to_vec(),
            include_bytes!("../fixtures/airserver/airserver_chain0.der").to_vec(),
            include_bytes!("../fixtures/airserver/airserver_chain1.der").to_vec(),
        ]
    }

    /// Every identity this crate can present, against the CRL it would attach.
    ///
    /// One clean identity is not enough to know serving is safe, because which chain is
    /// in force is a runtime decision — `identity_order`, table coverage and whether the
    /// backends answer all move it. If the published CRL named any of them, attaching it
    /// would be how that identity stops working.
    ///
    /// Note what is *not* enumerated: the per-window peer certificate. Revocation is
    /// checked over the trusted device chain (`GetBestValidPath()->certs`), and the peer
    /// certificate is self-signed and no part of it — so the live paths, which differ
    /// from these tables only in the peer certificate they mint, are covered by the same
    /// two chains.
    #[test]
    fn the_real_crl_revokes_none_of_the_identities_we_can_present() {
        let crl = CastCrl::parse(REAL_CRL).unwrap();
        for (name, chain) in [("cks", chain()), ("airserver", airserver_chain())] {
            assert_eq!(
                crl.revokes(&refs(&chain)).unwrap(),
                None,
                "the published CRL revokes the {name} identity; attaching it would be \
                 how that identity stops working"
            );
        }
    }

    #[test]
    fn a_crl_outside_its_window_is_withheld() {
        let chain = chain();
        let crl = CastCrl::parse(REAL_CRL).unwrap();
        let after = crl.window().end_unix() + 1;
        assert_eq!(
            crl.servable_for(&refs(&chain), after).unwrap().unwrap_err(),
            ServeRefusal::OutsideWindow,
            "a sender hard-fails on a stale CRL, so it is worse than sending none"
        );
        let inside = crl.window().start_unix() + 1;
        assert!(crl.servable_for(&refs(&chain), inside).unwrap().is_ok());
    }

    /// The case the whole type exists for: if the CRL names us, attaching it is how a
    /// working receiver becomes a broken one.
    #[test]
    fn a_crl_that_revokes_our_own_public_key_is_withheld() {
        let chain = chain();
        let (_, device) = x509_parser::parse_x509_certificate(&chain[0]).unwrap();
        let ours = spki_hash(device.tbs_certificate.subject_pki.raw);

        let crl = CastCrl::parse(REAL_CRL).unwrap();
        let mut poisoned = crl.clone();
        poisoned.revoked_keys.insert(ours);

        assert_eq!(
            poisoned.revokes(&refs(&chain)).unwrap(),
            Some(Revocation::PublicKey { index: 0 })
        );
        let inside = poisoned.window().start_unix() + 1;
        assert_eq!(
            poisoned
                .servable_for(&refs(&chain), inside)
                .unwrap()
                .unwrap_err(),
            ServeRefusal::RevokesUs(Revocation::PublicKey { index: 0 }),
            "serving a CRL that names us would hand every sender its reason to refuse"
        );
    }

    /// The other half of `CheckRevocation`: a serial range registered under the *issuer's*
    /// key hash revokes the certificate beneath it.
    #[test]
    fn a_serial_range_under_our_issuer_revokes_the_certificate_below_it() {
        let chain = chain();
        let (_, device) = x509_parser::parse_x509_certificate(&chain[0]).unwrap();
        let (_, ica) = x509_parser::parse_x509_certificate(&chain[1]).unwrap();
        let serial = serial_u64(device.raw_serial()).unwrap();
        let issuer = spki_hash(ica.tbs_certificate.subject_pki.raw);

        let mut poisoned = CastCrl::parse(REAL_CRL).unwrap();
        poisoned
            .revoked_serials
            .entry(issuer)
            .or_default()
            .push((serial, serial));

        assert_eq!(
            poisoned.revokes(&refs(&chain)).unwrap(),
            Some(Revocation::Serial { index: 0, serial })
        );
    }

    /// A range that does not contain us must not match — otherwise the check above would
    /// pass for the wrong reason.
    #[test]
    fn a_serial_range_that_misses_us_does_not_revoke() {
        let chain = chain();
        let (_, device) = x509_parser::parse_x509_certificate(&chain[0]).unwrap();
        let (_, ica) = x509_parser::parse_x509_certificate(&chain[1]).unwrap();
        let serial = serial_u64(device.raw_serial()).unwrap();
        let issuer = spki_hash(ica.tbs_certificate.subject_pki.raw);

        let mut poisoned = CastCrl::parse(REAL_CRL).unwrap();
        poisoned
            .revoked_serials
            .entry(issuer)
            .or_default()
            .push((serial + 1, serial + 100));

        assert_eq!(poisoned.revokes(&refs(&chain)).unwrap(), None);
    }

    #[test]
    fn a_serial_too_long_for_the_ranges_cannot_match_one() {
        // Nine significant octets: outside anything a uint64 range can name, and
        // truncating it into range would be a false revocation.
        assert_eq!(serial_u64(&[1, 2, 3, 4, 5, 6, 7, 8, 9]), None);
        // Leading DER zero padding is not significant.
        assert_eq!(serial_u64(&[0, 0xff, 0xff]), Some(0xffff));
        assert_eq!(serial_u64(&[0x52, 0x49, 0x1f, 0x16]), Some(0x5249_1f16));
    }

    #[test]
    fn a_malformed_bundle_is_an_error_rather_than_an_empty_crl() {
        assert!(CastCrl::parse(b"not a protobuf at all").is_err());
        assert!(
            CastCrl::parse(&[]).is_err(),
            "an empty bundle carries no CRL"
        );
    }
}
