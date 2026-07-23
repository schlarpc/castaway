//! # crypto-fairplay
//!
//! FairPlay-SAP is the gate in front of AirPlay mirroring: it protects the AirPlay
//! *session key* (the "~568 byte" v3 `/fp-setup` flow). This crate models the handshake
//! as a typestate state machine so the AirPlay adapter can drive it correctly on the
//! wire — but the actual key-derivation math depends on Apple's FairPlay tables, which
//! we don't have captured yet. So the derivation step is an explicit
//! [`FairPlayError::NotImplemented`] boundary (OPEN-QUESTIONS Q1); everything up to it
//! (message shaping, stage sequencing) is real and testable.
//!
//! This is distinct from **FairPlay Streaming** (content DRM) — a wall we don't touch.
#![forbid(unsafe_code)]

use thiserror::Error;

/// FairPlay handshake errors.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum FairPlayError {
    /// A message arrived in the wrong handshake stage.
    #[error("unexpected fp-setup message in stage {stage:?}")]
    WrongStage {
        /// The stage the state machine was in.
        stage: Stage,
    },

    /// The message body wasn't a recognizable fp-setup payload.
    #[error("malformed fp-setup message: {0}")]
    Malformed(&'static str),

    /// The step needs Apple's FairPlay key tables, which aren't captured yet.
    #[error("FairPlay key derivation not implemented (needs captured tables — see Q1)")]
    NotImplemented,
}

/// The FairPlay `/fp-setup` handshake stages. AirPlay's v3 flow is two round trips: the
/// sender posts an initial SETUP1 blob, we reply; it posts SETUP2, we derive and reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// No fp-setup seen yet.
    Idle,
    /// Received SETUP1, replied; awaiting SETUP2.
    AwaitingSetup2,
    /// SETUP2 received; session key derived (or would be).
    Complete,
}

/// The first byte of an fp-setup body encodes the mode; `0x03` is the v3 flow we target.
const FP_VERSION_V3: u8 = 0x03;

/// The AirPlay FairPlay-SAP handshake driver. Pure: feed it the request bodies the
/// RTSP `/fp-setup` handler received, get back the reply bodies to send.
#[derive(Debug, Clone)]
pub struct FairPlaySession {
    stage: Stage,
}

impl Default for FairPlaySession {
    fn default() -> Self {
        Self::new()
    }
}

impl FairPlaySession {
    /// Start a fresh handshake.
    #[must_use]
    pub fn new() -> Self {
        Self { stage: Stage::Idle }
    }

    /// The current handshake stage.
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Handle the first `/fp-setup` POST (SETUP1). Returns the reply body to send.
    ///
    /// The reply is a fixed server-mode response keyed by the mode byte; the actual
    /// bytes come from Apple's tables (stubbed).
    ///
    /// # Errors
    /// [`FairPlayError`] on wrong stage, malformed body, or the unimplemented boundary.
    pub fn setup1(&mut self, body: &[u8]) -> Result<Vec<u8>, FairPlayError> {
        if self.stage != Stage::Idle {
            return Err(FairPlayError::WrongStage { stage: self.stage });
        }
        let mode = parse_fp_header(body)?;
        if mode != FP_VERSION_V3 {
            return Err(FairPlayError::Malformed("unsupported fp-setup version"));
        }
        // The SETUP1 reply is a 142-byte server response selected by a mode index in the
        // body. Producing it requires the captured FairPlay reply tables.
        self.stage = Stage::AwaitingSetup2;
        Err(FairPlayError::NotImplemented)
    }

    /// Handle the second `/fp-setup` POST (SETUP2), which carries the sender's key
    /// material to be unwrapped into the AirPlay session key.
    ///
    /// # Errors
    /// [`FairPlayError`] on wrong stage or the unimplemented derivation boundary.
    pub fn setup2(&mut self, body: &[u8]) -> Result<Vec<u8>, FairPlayError> {
        if self.stage != Stage::AwaitingSetup2 {
            return Err(FairPlayError::WrongStage { stage: self.stage });
        }
        let _ = parse_fp_header(body)?;
        self.stage = Stage::Complete;
        // Deriving the session key from SETUP2 needs the FairPlay decrypt tables.
        Err(FairPlayError::NotImplemented)
    }
}

/// Validate the 4-byte `FPLY` magic + return the mode byte.
fn parse_fp_header(body: &[u8]) -> Result<u8, FairPlayError> {
    // fp-setup bodies begin with the ASCII magic "FPLY" then a version/mode byte.
    if body.len() < 5 {
        return Err(FairPlayError::Malformed("fp-setup body too short"));
    }
    if &body[..4] != b"FPLY" {
        return Err(FairPlayError::Malformed("missing FPLY magic"));
    }
    Ok(body[4])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn setup1_body() -> Vec<u8> {
        let mut v = b"FPLY".to_vec();
        v.push(FP_VERSION_V3);
        v.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]); // remaining mode/header bytes
        v
    }

    #[test]
    fn rejects_non_fply_body() {
        let mut s = FairPlaySession::new();
        assert_eq!(
            s.setup1(b"nope-nope"),
            Err(FairPlayError::Malformed("missing FPLY magic"))
        );
    }

    #[test]
    fn setup1_advances_stage_then_hits_boundary() {
        let mut s = FairPlaySession::new();
        // Reaches the documented derivation boundary but advances the stage first.
        assert_eq!(s.setup1(&setup1_body()), Err(FairPlayError::NotImplemented));
        assert_eq!(s.stage(), Stage::AwaitingSetup2);
    }

    #[test]
    fn setup2_out_of_order_is_wrong_stage() {
        let mut s = FairPlaySession::new();
        assert_eq!(
            s.setup2(&setup1_body()),
            Err(FairPlayError::WrongStage { stage: Stage::Idle })
        );
    }

    #[test]
    fn version_other_than_v3_rejected() {
        let mut s = FairPlaySession::new();
        let mut body = b"FPLY".to_vec();
        body.push(0x01);
        body.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(
            s.setup1(&body),
            Err(FairPlayError::Malformed("unsupported fp-setup version"))
        );
    }
}
