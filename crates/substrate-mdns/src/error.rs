//! mDNS substrate errors.

use thiserror::Error;

/// Failures advertising over mDNS-SD.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MdnsError {
    /// The service type wasn't a valid `_name._proto` pair.
    #[error("invalid mDNS service type: {0}")]
    InvalidServiceType(String),

    /// The mDNS daemon could not be created.
    #[error("mDNS daemon error: {0}")]
    Daemon(String),

    /// Building the `ServiceInfo` failed.
    #[error("mDNS service info error: {0}")]
    ServiceInfo(String),

    /// Registering the service failed.
    #[error("mDNS register error: {0}")]
    Register(String),
}
