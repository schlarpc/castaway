//! DLNA errors, including the UPnP SOAP fault codes we surface to control points.

use thiserror::Error;

/// Failures handling a DLNA/UPnP request.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DlnaError {
    /// The SOAP envelope/body could not be parsed.
    #[error("malformed SOAP: {0}")]
    MalformedSoap(&'static str),

    /// The requested action is not implemented by this service.
    #[error("invalid action: {0}")]
    InvalidAction(String),

    /// A required SOAP argument was missing.
    #[error("missing argument: {0}")]
    MissingArgument(&'static str),

    /// An argument value was out of range or unparseable.
    #[error("invalid argument value: {0}")]
    InvalidArgument(&'static str),
}

impl DlnaError {
    /// The UPnP SOAP fault code a control point expects for this error.
    ///
    /// See UPnP Device Architecture §3.2.2 error codes: 401 Invalid Action,
    /// 402 Invalid Args, 501 Action Failed, 600+ argument-specific.
    #[must_use]
    pub fn upnp_code(&self) -> u16 {
        match self {
            DlnaError::InvalidAction(_) => 401,
            DlnaError::MissingArgument(_) | DlnaError::InvalidArgument(_) => 402,
            DlnaError::MalformedSoap(_) => 501,
        }
    }
}
