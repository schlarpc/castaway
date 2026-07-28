//! GameStream client errors. One enum, one audience: the adapter (and its tests) that
//! drives discovery → pairing → launch → stream and needs to tell *which* stage broke
//! and whether re-pairing would help.

use thiserror::Error;

/// Failures across the GameStream client lifecycle.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GameStreamError {
    /// Generating, loading, or persisting the client certificate/key.
    #[error("client identity error: {0}")]
    Identity(String),

    /// The transport under an NVHTTP request (TCP, TLS, malformed HTTP).
    #[error("NVHTTP transport error: {0}")]
    Http(String),

    /// NVHTTP answered, but the XML was not the shape the API promises.
    #[error("NVHTTP response did not parse: {0}")]
    Xml(String),

    /// NVHTTP answered with an explicit failure (`status_code` ≠ 200).
    #[error("NVHTTP declined ({code}): {message}")]
    Nvhttp {
        /// The `status_code` attribute from the response root.
        code: i32,
        /// The `status_message` attribute, verbatim.
        message: String,
    },

    /// The pairing handshake failed a cryptographic check — a MITM, or a host that
    /// forgot us mid-handshake. Never retried silently: the caller decides.
    #[error("pairing failed: {0}")]
    Pairing(String),

    /// The pairing hash check failed in exactly the way a mistyped PIN fails. Its own
    /// variant because the recovery is different: ask the person to try again, don't
    /// distrust the host.
    #[error("pairing PIN did not match")]
    WrongPin,

    /// This client is not paired with the host (yet, or any more).
    #[error("not paired with {host}")]
    NotPaired {
        /// The host we asked.
        host: String,
    },

    /// The streaming session (the linked moonlight-common-c core) failed to start or
    /// died. The stage name comes from `LiGetStageName`.
    #[error("stream {stage} failed (code {code})")]
    Stream {
        /// Which connection stage failed.
        stage: String,
        /// The library's error code for that stage.
        code: i32,
    },

    /// The session event channel to the session manager closed — shutdown, not a bug.
    #[error("session sink closed")]
    SinkClosed,
}
