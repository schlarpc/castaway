//! The `ANNOUNCE` session description: what an AirPlay 1 sender tells us it is about to
//! stream, and the only place the audio format is stated.
//!
//! This is *Session* Description Protocol. `substrate-sdp` in this workspace is
//! Bluetooth's *Service* Discovery Protocol — a different thing with the same acronym,
//! and the wrong home for this.
//!
//! The parse is deliberately narrow, because real senders are: shairport-sync scans for
//! six line prefixes and ignores everything else in the body, and so do we. A sender
//! that adds a `b=` or a second `m=` line is not an error, it is a sender.
//!
//! The one piece of real modelling here is [`StreamCrypto`]. Encryption is signalled by
//! *two* attributes, and their presence is a three-way rather than a boolean: both
//! absent means the stream is unencrypted, both present means it is encrypted, and
//! exactly one present is a malformed announcement that shairport-sync answers `456` to.
//! Making that an enum at the boundary means no later stage can ask "is there a key"
//! and get a half-answer.

use std::fmt;

use crate::error::SdpError;

/// Defaults for the twelve `a=fmtp:` integers, in wire order.
///
/// Senders omit trailing fields, so these are pre-loaded and then overwritten by
/// whatever the body actually carries — the same thing shairport-sync does, and the
/// reason the field order below can be trusted.
const FMTP_DEFAULTS: [u32; 12] = [96, 352, 0, 16, 40, 10, 14, 2, 255, 0, 0, 44100];

/// The size of an ALAC magic cookie, and the value of its own length field.
const ALAC_MAGIC_COOKIE_LEN: usize = 36;

/// ALAC's parameters, as the twelve `a=fmtp:` integers state them.
///
/// Kept whole rather than reduced to "rate and channels" because libavcodec's ALAC
/// decoder will not open without all of it — see [`AlacConfig::magic_cookie`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlacConfig {
    /// Samples per packet (`frameLength`). 352 in every capture seen.
    pub frame_length: u32,
    /// Format version the sender claims compatibility with.
    pub compatible_version: u8,
    /// Bits per sample.
    pub bit_depth: u8,
    /// Rice history multiplier.
    pub pb: u8,
    /// Rice initial history.
    pub mb: u8,
    /// Rice limit.
    pub kb: u8,
    /// Channel count.
    pub channels: u8,
    /// Maximum run length.
    pub max_run: u16,
    /// Maximum bytes in a frame, or 0 for unknown.
    pub max_frame_bytes: u32,
    /// Average bit rate, or 0 for unknown.
    pub avg_bit_rate: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
}

impl AlacConfig {
    /// The ALAC configuration a `SETUP` plist implies.
    ///
    /// The plist carries only `spf`, `sr` and a channel count — where an SDP body spells
    /// out all eleven `fmtp` integers — so the rest are the constants every AirPlay
    /// sender uses and every receiver assumes: the `40 10 14` Rice parameters and a
    /// 255-sample maximum run, exactly the values in the classic
    /// `a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100`. They are not guesses about this
    /// sender; they are what ALAC-over-AirPlay is.
    #[must_use]
    pub const fn airplay(frame_length: u32, sample_rate: u32, channels: u8) -> Self {
        Self {
            frame_length,
            compatible_version: 0,
            bit_depth: 16,
            pb: 40,
            mb: 10,
            kb: 14,
            channels,
            max_run: 255,
            max_frame_bytes: 0,
            avg_bit_rate: 0,
            sample_rate,
        }
    }

    /// Build the 36-byte ALAC "magic cookie" (an `ALACSpecificConfig` atom).
    ///
    /// libavcodec's ALAC decoder refuses to open with fewer than 36 bytes of extradata
    /// — `alac_decode_init` checks the length and bails — so this is not optional
    /// decoration, it is the thing that makes decoding possible at all. Big-endian
    /// throughout, and the first twelve bytes are an atom header the decoder skips.
    #[must_use]
    pub fn magic_cookie(&self) -> [u8; ALAC_MAGIC_COOKIE_LEN] {
        let mut c = [0u8; ALAC_MAGIC_COOKIE_LEN];
        // Atom size, then the 'alac' type, then a zero version — skipped by the
        // decoder, but it reads the length, so it has to be right.
        c[0..4].copy_from_slice(
            &u32::try_from(ALAC_MAGIC_COOKIE_LEN)
                .unwrap_or(36)
                .to_be_bytes(),
        );
        c[4..8].copy_from_slice(b"alac");
        c[8..12].copy_from_slice(&0u32.to_be_bytes());
        c[12..16].copy_from_slice(&self.frame_length.to_be_bytes());
        c[16] = self.compatible_version;
        c[17] = self.bit_depth;
        c[18] = self.pb;
        c[19] = self.mb;
        c[20] = self.kb;
        c[21] = self.channels;
        c[22..24].copy_from_slice(&self.max_run.to_be_bytes());
        c[24..28].copy_from_slice(&self.max_frame_bytes.to_be_bytes());
        c[28..32].copy_from_slice(&self.avg_bit_rate.to_be_bytes());
        c[32..36].copy_from_slice(&self.sample_rate.to_be_bytes());
        c
    }
}

/// The `AudioSpecificConfig` for AirPlay's AAC-ELD: AOT 39 (ER AAC ELD), 44.1 kHz,
/// stereo, 480-sample frames.
///
/// Four bytes, and always these four — every mirroring sender uses the same profile, so
/// unlike ALAC's cookie there is nothing to derive from the negotiation.
pub const AAC_ELD_CONFIG: [u8; 4] = [0xf8, 0xe8, 0x50, 0x00];

/// What the sender is about to put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaopCodec {
    /// Apple Lossless. `a=rtpmap:96 AppleLossless`.
    Alac(AlacConfig),
    /// Uncompressed 16-bit PCM. `a=rtpmap:96 L16/44100/2`.
    Pcm {
        /// Sample rate in Hz.
        sample_rate: u32,
        /// Channel count.
        channels: u8,
    },
    /// AAC Enhanced Low Delay — the codec a *mirroring* session's audio uses.
    ///
    /// It never appears in an SDP body: mirroring negotiates through `SETUP` plists, and
    /// this is the one codec that path offers.
    AacEld {
        /// Sample rate in Hz.
        sample_rate: u32,
        /// Channel count.
        channels: u8,
    },
}

impl RaopCodec {
    /// Sample rate in Hz.
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        match self {
            Self::Alac(c) => c.sample_rate,
            Self::Pcm { sample_rate, .. } | Self::AacEld { sample_rate, .. } => *sample_rate,
        }
    }

    /// Channel count.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        match self {
            Self::Alac(c) => c.channels,
            Self::Pcm { channels, .. } | Self::AacEld { channels, .. } => *channels,
        }
    }

    /// The codec's name as it is normally written.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Alac(_) => "ALAC",
            Self::Pcm { .. } => "PCM",
            Self::AacEld { .. } => "AAC-ELD",
        }
    }

    /// The out-of-band configuration a decoder needs to open, if any.
    ///
    /// libavcodec will not open ALAC without its 36-byte cookie or AAC-ELD without its
    /// `AudioSpecificConfig`; PCM needs no decoder at all. Keeping this beside the codec
    /// means the actor never has to know which is which.
    #[must_use]
    pub fn codec_config(&self) -> Option<Vec<u8>> {
        match self {
            Self::Alac(c) => Some(c.magic_cookie().to_vec()),
            Self::AacEld { .. } => Some(AAC_ELD_CONFIG.to_vec()),
            Self::Pcm { .. } => None,
        }
    }
}

/// Which AirPlay flow negotiated a stream.
///
/// Only for saying so on screen: the two arrive through completely different
/// negotiations — an `ANNOUNCE` with an SDP body against a `SETUP` with a plist — so
/// nothing downstream has to branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    /// The AirPlay 1 audio flow.
    AirPlay1,
    /// Audio riding alongside a mirroring session.
    Mirroring,
}

/// An unwrapped AES-128 session key.
///
/// A newtype for one reason: its [`fmt::Debug`] does not print the key. `AnnounceParams`
/// derives `Debug` and gets logged, and a session key in the journal is a session key on
/// disk. Reaching the bytes takes an explicit [`SessionKey::expose`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionKey([u8; 16]);

impl SessionKey {
    /// Wrap already-unwrapped key material.
    #[must_use]
    pub const fn from_bytes(key: [u8; 16]) -> Self {
        Self(key)
    }

    /// The key material. Named to be conspicuous at the call site.
    #[must_use]
    pub const fn expose(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKey(<redacted>)")
    }
}

/// Whether, and how, the audio payload is encrypted.
///
/// The three-way that `a=rsaaeskey` and `a=aesiv` really encode — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamCrypto {
    /// Neither attribute present: the payload arrives in the clear.
    None,
    /// Both present: AES-128-CBC, with the IV reset from `iv` for *every* packet.
    Aes {
        /// The session key, already unwrapped.
        key: SessionKey,
        /// The initialisation vector, which arrives unwrapped.
        iv: [u8; 16],
    },
}

impl AnnounceParams {
    /// Re-key the media with a verified legacy-pairing secret (feature bit 27).
    ///
    /// `SHA512(aeskey ‖ shared)[0..16]`, per research §4.3. A no-op for an unencrypted
    /// stream: there is no key to hash, and inventing one would be worse than the
    /// plaintext the sender actually sent.
    pub fn rekey_with(&mut self, shared: &[u8; 32]) {
        if let StreamCrypto::Aes { key, .. } = &mut self.crypto {
            *key = SessionKey(crate::pairing::rekey_media(&key.0, shared));
        }
    }
}

impl StreamCrypto {
    /// Whether the payload needs decrypting before it can be decoded.
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        matches!(self, Self::Aes { .. })
    }
}

/// Everything an `ANNOUNCE` body settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceParams {
    /// Which flow negotiated this stream.
    pub generation: Generation,
    /// The negotiated codec and its parameters.
    pub codec: RaopCodec,
    /// Whether the payload is encrypted, and with what.
    pub crypto: StreamCrypto,
    /// The sender's requested minimum latency, in frames.
    pub min_latency: Option<u32>,
    /// The sender's requested maximum latency, in frames.
    pub max_latency: Option<u32>,
}

impl AnnounceParams {
    /// The audio stream that rides alongside a mirroring session.
    ///
    /// Mirroring negotiates its audio through a `SETUP` plist rather than an SDP body.
    /// The key is the FairPlay-unwrapped one, and the IV is the `eiv` from the same
    /// `SETUP` that carried the wrapped key; the payload rules are AirPlay 1's, which is
    /// why this reuses the same depacketiser.
    #[must_use]
    pub const fn mirror_aac_eld(key: SessionKey, iv: [u8; 16]) -> Self {
        Self::plist_stream(
            RaopCodec::AacEld {
                sample_rate: 44_100,
                channels: 2,
            },
            key,
            iv,
        )
    }

    /// A stream negotiated by a `SETUP` plist rather than an SDP body.
    ///
    /// The general form of [`Self::mirror_aac_eld`], and it exists because mirroring is
    /// not the only session that negotiates this way: a sender casting *media* (a video
    /// from an app, rather than the screen) sets up a plist stream too, and describes a
    /// different codec in it — `ct: 2`, ALAC at 352 samples a packet, where mirroring
    /// says `ct: 8`, AAC-ELD at 480. Which one is a property of the request, so it is a
    /// parameter here rather than a constant.
    #[must_use]
    pub const fn plist_stream(codec: RaopCodec, key: SessionKey, iv: [u8; 16]) -> Self {
        Self {
            generation: Generation::Mirroring,
            codec,
            crypto: StreamCrypto::Aes { key, iv },
            // A sender on this path declares its latency in the sync packets rather than
            // here; observed values are around 7497 frames against ALAC's 77175.
            min_latency: None,
            max_latency: None,
        }
    }

    /// A human-readable summary of what was negotiated, for the on-screen device card.
    ///
    /// The AirPlay counterpart of `proto-bluetooth-audio`'s `Codec::describe`, and it
    /// reads the same way on the panel: what is carrying the audio, at what rate, in
    /// how many channels. The generation is stated because it is the thing people ask
    /// about first and cannot otherwise tell — and because an `ANNOUNCE` with an SDP
    /// body *is* what makes this AirPlay 1. AirPlay 2 negotiates through a `SETUP`
    /// plist instead, so when that arrives it describes itself and does not come
    /// through here.
    #[must_use]
    pub fn describe(&self) -> String {
        let rate = self.codec.sample_rate();
        let khz = if rate.is_multiple_of(1000) {
            format!("{} kHz", rate / 1000)
        } else {
            // 44100 reads as 44.1, not 44 — the difference people actually look for.
            format!("{:.1} kHz", f64::from(rate) / 1000.0)
        };
        let channels = match self.codec.channels() {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{n} channels"),
        };
        let generation = match self.generation {
            Generation::AirPlay1 => "AirPlay 1",
            Generation::Mirroring => "AirPlay mirroring",
        };
        format!(
            "{generation} · {} · {khz} · {channels}",
            self.codec.display_name()
        )
    }

    /// Parse an `ANNOUNCE` SDP body.
    ///
    /// # Errors
    /// [`SdpError`] if the media line is missing, the codec is one we do not decode, or
    /// the encryption attributes disagree with each other.
    pub fn parse(body: &[u8]) -> Result<Self, SdpError> {
        let text = std::str::from_utf8(body).map_err(|_| SdpError::NotUtf8)?;

        let mut rtpmap: Option<&str> = None;
        let mut fmtp: Option<&str> = None;
        let mut wrapped_key: Option<Vec<u8>> = None;
        let mut iv: Option<[u8; 16]> = None;
        let mut min_latency = None;
        let mut max_latency = None;

        // Six prefixes, and everything else ignored. Senders put plenty in a body that
        // is none of our business.
        for line in text.lines().map(str::trim) {
            if let Some(v) = line.strip_prefix("a=rtpmap:") {
                rtpmap = Some(v);
            } else if let Some(v) = line.strip_prefix("a=fmtp:") {
                fmtp = Some(v);
            } else if let Some(v) = line.strip_prefix("a=rsaaeskey:") {
                wrapped_key = Some(decode_base64(v, "rsaaeskey")?);
            } else if let Some(v) = line.strip_prefix("a=aesiv:") {
                let raw = decode_base64(v, "aesiv")?;
                iv = Some(
                    <[u8; 16]>::try_from(raw.as_slice())
                        .map_err(|_| SdpError::BadLength { attribute: "aesiv" })?,
                );
            } else if let Some(v) = line.strip_prefix("a=min-latency:") {
                min_latency = v.trim().parse::<u32>().ok();
            } else if let Some(v) = line.strip_prefix("a=max-latency:") {
                max_latency = v.trim().parse::<u32>().ok();
            }
        }

        let rtpmap = rtpmap.ok_or(SdpError::MissingRtpmap)?;
        let codec = parse_codec(rtpmap, fmtp)?;

        // The three-way. Exactly one attribute present is the case worth naming: it is
        // a sender that thinks it negotiated encryption with a receiver that would then
        // play noise, so it is refused rather than silently treated as plaintext.
        let crypto = match (wrapped_key, iv) {
            (None, None) => StreamCrypto::None,
            (Some(wrapped), Some(iv)) => StreamCrypto::Aes {
                // Unwrap here rather than downstream: a key that cannot be unwrapped
                // makes the whole announcement useless, and the sender should be told
                // now — while it can still pick something else — rather than after it
                // has started streaming into a decoder that will emit static.
                key: SessionKey(
                    crypto_raop::unwrap_aes_key(&wrapped).map_err(|_| SdpError::KeyUnwrap)?,
                ),
                iv,
            },
            (Some(_), None) => return Err(SdpError::HalfEncrypted { missing: "aesiv" }),
            (None, Some(_)) => {
                return Err(SdpError::HalfEncrypted {
                    missing: "rsaaeskey",
                })
            }
        };

        Ok(Self {
            generation: Generation::AirPlay1,
            codec,
            crypto,
            min_latency,
            max_latency,
        })
    }
}

/// Decode a base64 attribute value, tolerating the missing `=` padding senders omit.
fn decode_base64(value: &str, attribute: &'static str) -> Result<Vec<u8>, SdpError> {
    use base64::Engine as _;
    let trimmed = value.trim();
    // Senders strip padding, so the padding-optional alphabet is the one that works on
    // real traffic; the strict engine rejects bodies an iPhone actually sends.
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(trimmed.trim_end_matches('='))
        .map_err(|_| SdpError::BadBase64 { attribute })
}

/// Turn the `rtpmap` encoding name (and `fmtp`, for ALAC) into a codec.
fn parse_codec(rtpmap: &str, fmtp: Option<&str>) -> Result<RaopCodec, SdpError> {
    // `a=rtpmap:96 AppleLossless` — the payload type, a space, then the encoding.
    let encoding = rtpmap
        .split_once(' ')
        .map_or(rtpmap, |(_, rest)| rest)
        .trim();

    if encoding.eq_ignore_ascii_case("AppleLossless") {
        return Ok(RaopCodec::Alac(parse_fmtp(fmtp)?));
    }
    // `L16/44100/2`: uncompressed, and the rate and channels are in the name itself.
    if let Some(rest) = encoding
        .strip_prefix("L16/")
        .or_else(|| encoding.eq_ignore_ascii_case("L16").then_some("44100/2"))
    {
        let mut parts = rest.split('/');
        let sample_rate = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(44100);
        let channels = parts.next().and_then(|s| s.parse::<u8>().ok()).unwrap_or(2);
        return Ok(RaopCodec::Pcm {
            sample_rate,
            channels,
        });
    }
    Err(SdpError::UnsupportedCodec(encoding.to_string()))
}

/// Parse the twelve `a=fmtp:` integers over their defaults.
fn parse_fmtp(fmtp: Option<&str>) -> Result<AlacConfig, SdpError> {
    let mut v = FMTP_DEFAULTS;
    if let Some(line) = fmtp {
        // The payload type leads the list, then the eleven ALAC parameters. Whitespace-
        // separated, and a sender may send fewer than twelve.
        for (slot, field) in v.iter_mut().zip(line.split_whitespace()) {
            *slot = field.parse::<u32>().map_err(|_| SdpError::BadFmtp)?;
        }
    }
    let narrow = |i: usize| u8::try_from(v[i]).map_err(|_| SdpError::BadFmtp);
    Ok(AlacConfig {
        frame_length: v[1],
        compatible_version: narrow(2)?,
        bit_depth: narrow(3)?,
        pb: narrow(4)?,
        mb: narrow(5)?,
        kb: narrow(6)?,
        channels: narrow(7)?,
        max_run: u16::try_from(v[8]).map_err(|_| SdpError::BadFmtp)?,
        max_frame_bytes: v[9],
        avg_bit_rate: v[10],
        sample_rate: v[11],
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    /// The AirPort key is carved at build time rather than checked in, so a build
    /// without it cannot exercise the RSA paths. `nix flake check` always has it.
    fn skip_without_airport_key() -> bool {
        if crypto_raop::has_airport_key() {
            return false;
        }
        eprintln!("skipping: this build has no AirPort key");
        true
    }
    use super::*;

    /// An ALAC announcement in the shape iOS sends.
    const IOS_ALAC: &str = "v=0\r\n\
        o=iTunes 3696222840 0 IN IP4 10.0.0.7\r\n\
        s=iTunes\r\n\
        c=IN IP4 10.0.0.9\r\n\
        t=0 0\r\n\
        m=audio 0 RTP/AVP 96\r\n\
        a=rtpmap:96 AppleLossless\r\n\
        a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
        a=min-latency:11025\r\n\
        a=max-latency:88200\r\n";

    #[test]
    fn parses_an_ios_alac_announcement() {
        let p = AnnounceParams::parse(IOS_ALAC.as_bytes()).unwrap();
        let RaopCodec::Alac(alac) = p.codec else {
            panic!("expected ALAC, got {:?}", p.codec)
        };
        assert_eq!(alac.frame_length, 352);
        assert_eq!(alac.bit_depth, 16);
        assert_eq!(alac.channels, 2);
        assert_eq!(alac.sample_rate, 44100);
        assert_eq!(p.min_latency, Some(11025));
        assert_eq!(p.max_latency, Some(88200));
        assert_eq!(p.crypto, StreamCrypto::None);
    }

    #[test]
    fn parses_the_uncompressed_form_pyatv_sends() {
        // pyatv streams L16 rather than ALAC, and it is our scripted sender in CI — so
        // this is not a hypothetical dialect, it is the one the VM test will produce.
        let body = IOS_ALAC.replace("a=rtpmap:96 AppleLossless", "a=rtpmap:96 L16/44100/2");
        let p = AnnounceParams::parse(body.as_bytes()).unwrap();
        assert_eq!(
            p.codec,
            RaopCodec::Pcm {
                sample_rate: 44100,
                channels: 2
            }
        );
    }

    #[test]
    fn an_unknown_codec_is_named_rather_than_guessed_at() {
        let body = IOS_ALAC.replace("AppleLossless", "opus/48000/2");
        let err = AnnounceParams::parse(body.as_bytes()).unwrap_err();
        assert!(
            matches!(&err, SdpError::UnsupportedCodec(c) if c.contains("opus")),
            "{err:?}"
        );
    }

    #[test]
    fn missing_fmtp_falls_back_to_the_defaults_senders_omit() {
        let body = IOS_ALAC.replace("a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n", "");
        let p = AnnounceParams::parse(body.as_bytes()).unwrap();
        let RaopCodec::Alac(alac) = p.codec else {
            panic!("expected ALAC")
        };
        assert_eq!((alac.frame_length, alac.sample_rate), (352, 44100));
    }

    #[test]
    fn lines_we_do_not_care_about_are_not_errors() {
        let body = format!("{IOS_ALAC}b=AS:128\r\na=type:broadcast\r\na=x-nonsense:1\r\n");
        assert!(AnnounceParams::parse(body.as_bytes()).is_ok());
    }

    /// Wrap a session key the way a sender does, so the test exercises the real unwrap.
    fn wrap_for_us(session_key: &[u8; 16]) -> String {
        use base64::Engine as _;
        // The public half of the key `crypto-raop` carries: exactly what an iPhone
        // encrypts to. Asked for rather than read out of that crate's source, because
        // the key is carved at build time and is not a file in this tree.
        let wrapped = crypto_raop::airport_public_key()
            .unwrap()
            .encrypt(
                &mut rsa::rand_core::OsRng,
                rsa::Oaep::new::<sha1::Sha1>(),
                session_key,
            )
            .unwrap();
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(wrapped)
    }

    #[test]
    fn both_encryption_attributes_present_yields_an_unwrapped_key() {
        if skip_without_airport_key() {
            return;
        }
        // End to end: a key wrapped the way a sender wraps it comes back out usable.
        let session_key = *b"0123456789abcdef";
        let body = format!(
            "{IOS_ALAC}a=rsaaeskey:{}\r\na=aesiv:{}\r\n",
            wrap_for_us(&session_key),
            "QUJDREVGR0hJSktMTU5PUA"
        );
        let p = AnnounceParams::parse(body.as_bytes()).unwrap();
        let StreamCrypto::Aes { key, iv } = &p.crypto else {
            panic!("expected AES, got {:?}", p.crypto)
        };
        assert_eq!(iv, b"ABCDEFGHIJKLMNOP");
        assert_eq!(key.expose(), &session_key);
        assert!(p.crypto.is_encrypted());
    }

    #[test]
    fn a_key_we_cannot_unwrap_is_refused_rather_than_used() {
        // A FairPlay-wrapped key reaching the RSA path would otherwise decrypt the
        // stream into static, with nothing in any log to say why.
        let body = format!(
            "{IOS_ALAC}a=rsaaeskey:{}\r\na=aesiv:QUJDREVGR0hJSktMTU5PUA\r\n",
            "QUJD".repeat(86)
        );
        assert!(matches!(
            AnnounceParams::parse(body.as_bytes()),
            Err(SdpError::KeyUnwrap)
        ));
    }

    #[test]
    fn a_session_key_does_not_appear_in_debug_output() {
        if skip_without_airport_key() {
            return;
        }
        // AnnounceParams is logged. A key in the journal is a key on disk.
        let session_key = *b"0123456789abcdef";
        let body = format!(
            "{IOS_ALAC}a=rsaaeskey:{}\r\na=aesiv:QUJDREVGR0hJSktMTU5PUA\r\n",
            wrap_for_us(&session_key)
        );
        let p = AnnounceParams::parse(body.as_bytes()).unwrap();
        let rendered = format!("{p:?}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(
            !rendered.contains("0123456789abcdef"),
            "the key leaked into Debug: {rendered}"
        );
    }

    #[test]
    fn exactly_one_encryption_attribute_is_refused() {
        if skip_without_airport_key() {
            return;
        }
        // The case that matters: a sender that believes it negotiated encryption talking
        // to a receiver that would play noise. shairport-sync answers 456 to this.
        let only_key = format!(
            "{IOS_ALAC}a=rsaaeskey:{}\r\n",
            wrap_for_us(b"0123456789abcdef")
        );
        assert!(matches!(
            AnnounceParams::parse(only_key.as_bytes()),
            Err(SdpError::HalfEncrypted { missing: "aesiv" })
        ));
        let only_iv = format!("{IOS_ALAC}a=aesiv:QUJDREVGR0hJSktMTU5PUA\r\n");
        assert!(matches!(
            AnnounceParams::parse(only_iv.as_bytes()),
            Err(SdpError::HalfEncrypted {
                missing: "rsaaeskey"
            })
        ));
    }

    #[test]
    fn an_iv_of_the_wrong_length_is_refused() {
        if skip_without_airport_key() {
            return;
        }
        let body = format!(
            "{IOS_ALAC}a=rsaaeskey:{}\r\na=aesiv:QUJD\r\n",
            wrap_for_us(b"0123456789abcdef")
        );
        assert!(matches!(
            AnnounceParams::parse(body.as_bytes()),
            Err(SdpError::BadLength { attribute: "aesiv" })
        ));
    }

    #[test]
    fn the_magic_cookie_is_what_libavcodec_demands() {
        // 36 bytes or the ALAC decoder will not open at all, and the fields are the
        // fmtp integers in a fixed big-endian layout.
        let AnnounceParams {
            codec: RaopCodec::Alac(alac),
            ..
        } = AnnounceParams::parse(IOS_ALAC.as_bytes()).unwrap()
        else {
            panic!("expected ALAC")
        };
        let c = alac.magic_cookie();
        assert_eq!(c.len(), 36);
        assert_eq!(u32::from_be_bytes([c[0], c[1], c[2], c[3]]), 36);
        assert_eq!(&c[4..8], b"alac");
        assert_eq!(u32::from_be_bytes([c[12], c[13], c[14], c[15]]), 352);
        assert_eq!(c[17], 16, "bit depth");
        assert_eq!(c[21], 2, "channels");
        assert_eq!(u32::from_be_bytes([c[32], c[33], c[34], c[35]]), 44100);
    }

    #[test]
    fn mirror_audio_carries_the_config_its_decoder_needs() {
        // AAC-ELD will not open without its AudioSpecificConfig, and unlike ALAC's
        // cookie there is nothing to derive — every mirroring sender uses this profile.
        let p = AnnounceParams::mirror_aac_eld(SessionKey::from_bytes([7u8; 16]), [9u8; 16]);
        assert_eq!(p.codec.codec_config().unwrap(), AAC_ELD_CONFIG.to_vec());
        assert_eq!(p.codec.sample_rate(), 44_100);
        assert!(p.crypto.is_encrypted(), "mirror audio is always encrypted");
    }

    #[test]
    fn mirror_audio_says_it_is_mirroring_not_airplay_1() {
        // Both flows end in the same depacketiser, so the card is the only place the
        // difference is visible at all.
        let p = AnnounceParams::mirror_aac_eld(SessionKey::from_bytes([7u8; 16]), [9u8; 16]);
        assert_eq!(
            p.describe(),
            "AirPlay mirroring · AAC-ELD · 44.1 kHz · stereo"
        );
    }

    #[test]
    fn describe_names_the_generation_and_the_codec() {
        // What the panel shows, and the AirPlay counterpart of Bluetooth's
        // "aptX HD · 48 kHz · stereo".
        let p = AnnounceParams::parse(IOS_ALAC.as_bytes()).unwrap();
        assert_eq!(p.describe(), "AirPlay 1 · ALAC · 44.1 kHz · stereo");
    }

    #[test]
    fn describe_reads_44_1_not_44() {
        let body = IOS_ALAC.replace("a=rtpmap:96 AppleLossless", "a=rtpmap:96 L16/48000/1");
        let p = AnnounceParams::parse(body.as_bytes()).unwrap();
        assert_eq!(p.describe(), "AirPlay 1 · PCM · 48 kHz · mono");
    }

    #[test]
    fn a_body_that_is_not_text_is_refused() {
        assert!(matches!(
            AnnounceParams::parse(&[0xff, 0xfe, 0xfd]),
            Err(SdpError::NotUtf8)
        ));
    }

    #[test]
    fn a_body_with_no_media_description_is_refused() {
        assert!(matches!(
            AnnounceParams::parse(b"v=0\r\ns=iTunes\r\n"),
            Err(SdpError::MissingRtpmap)
        ));
    }
}
