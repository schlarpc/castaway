//! DLNA errors, including the UPnP SOAP fault codes we surface to control points.
//!
//! The fault code is the whole point of this type. A control point does not read our log
//! and cannot see our screen; the number in the fault is the only thing that tells the
//! person holding the phone *why* their cast did not happen, and the difference between
//! "that is not a real action" and "that file is not where you said it was" is the
//! difference between a bug report and a shrug.
//!
//! Codes come from two tables and the split matters: 401/402/501/600s are UDA 1.1 §3.2.2,
//! common to every UPnP service; the 700s are AVTransport's own, defined per action in
//! §2.4. A code from the wrong table is not a small error — 402 "Invalid Args" tells a
//! control point its *request* was malformed and it should stop, where 701 "Transition not
//! available" tells it the request was fine and to try again from another state.

use thiserror::Error;

/// Failures handling a DLNA/UPnP request.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DlnaError {
    /// The SOAP envelope/body could not be parsed.
    #[error("malformed SOAP: {0}")]
    MalformedSoap(&'static str),

    /// The requested action is not part of this service at all.
    #[error("invalid action: {0}")]
    InvalidAction(String),

    /// The requested action is part of this service, is optional, and is not implemented.
    ///
    /// Distinct from [`DlnaError::InvalidAction`] because control points act on the
    /// difference: 602 means "this device does not do that", which is a fact about the
    /// device, and 401 means "no such action", which reads as a broken implementation.
    /// ConnectionManager's `PrepareForConnection` is the case that matters — several
    /// control points use its absence to detect the DLNA default-connection model, so it
    /// has to be absent *correctly*.
    #[error("optional action not implemented: {0}")]
    OptionalActionNotImplemented(String),

    /// A required SOAP argument was missing.
    #[error("missing argument: {0}")]
    MissingArgument(&'static str),

    /// An argument value was out of range or unparseable.
    #[error("invalid argument value: {0}")]
    InvalidArgument(&'static str),

    /// The action addressed a virtual instance this renderer does not have.
    ///
    /// Every AVTransport action defines 718 and this renderer has exactly one instance, 0.
    /// Ignoring the argument meant a control point addressing instance 1 silently drove
    /// instance 0 — so a device it believed it had never started was playing.
    #[error("invalid InstanceID: {0}")]
    InvalidInstanceId(String),

    /// The transport cannot make the transition that was asked for from where it is.
    ///
    /// AVTransport §2.4: `Play` with no media set, `Next` with nothing staged behind the
    /// current item. The generic 402 that used to come back said "your arguments are
    /// wrong", which they were not.
    #[error("transition not available: {0}")]
    TransitionNotAvailable(&'static str),

    /// The resource the control point named is not a URI this renderer can fetch.
    #[error("resource not found: {0}")]
    ResourceNotFound(String),

    /// The resource is of a type this renderer cannot play.
    #[error("illegal MIME type: {0}")]
    IllegalMimeType(String),

    /// A `Seek` named a unit this renderer does not implement.
    #[error("seek mode not supported")]
    SeekModeNotSupported,

    /// A `Seek` target could not be parsed, or is outside the item.
    #[error("illegal seek target")]
    IllegalSeekTarget,
}

impl DlnaError {
    /// The UPnP SOAP fault code a control point expects for this error.
    ///
    /// UDA 1.1 §3.2.2 for the common codes; AVTransport:1 §2.4's per-action tables for the
    /// 700s. Exhaustive on purpose, so a variant added later cannot silently inherit
    /// somebody else's number.
    #[must_use]
    pub const fn upnp_code(&self) -> u16 {
        match self {
            DlnaError::InvalidAction(_) => 401,
            DlnaError::MissingArgument(_) | DlnaError::InvalidArgument(_) => 402,
            DlnaError::MalformedSoap(_) => 501,
            DlnaError::OptionalActionNotImplemented(_) => 602,
            DlnaError::TransitionNotAvailable(_) => 701,
            DlnaError::SeekModeNotSupported => 710,
            DlnaError::IllegalSeekTarget => 711,
            DlnaError::IllegalMimeType(_) => 714,
            DlnaError::ResourceNotFound(_) => 716,
            DlnaError::InvalidInstanceId(_) => 718,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tables must not blur into each other. A 4xx says the request was wrong; a
    /// 7xx says the request was fine and the transport could not do it — and a control
    /// point given the first when it should have had the second stops trying.
    #[test]
    fn each_failure_carries_the_code_its_action_table_defines() {
        assert_eq!(
            DlnaError::InvalidAction("Frobnicate".into()).upnp_code(),
            401
        );
        assert_eq!(DlnaError::MissingArgument("CurrentURI").upnp_code(), 402);
        assert_eq!(DlnaError::MalformedSoap("no body").upnp_code(), 501);
        assert_eq!(
            DlnaError::OptionalActionNotImplemented("PrepareForConnection".into()).upnp_code(),
            602
        );
        assert_eq!(
            DlnaError::TransitionNotAvailable("Play without media").upnp_code(),
            701
        );
        assert_eq!(DlnaError::SeekModeNotSupported.upnp_code(), 710);
        assert_eq!(DlnaError::IllegalSeekTarget.upnp_code(), 711);
        assert_eq!(
            DlnaError::IllegalMimeType("text/html".into()).upnp_code(),
            714
        );
        assert_eq!(
            DlnaError::ResourceNotFound("http://h/x".into()).upnp_code(),
            716
        );
        assert_eq!(DlnaError::InvalidInstanceId("1".into()).upnp_code(), 718);
    }
}
