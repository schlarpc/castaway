//! On-disk cache for a fetched credential.
//!
//! The backend serves one 2-day window per request, so without a cache every
//! restart — and the panel restarts — spends a request re-fetching material it
//! already had. The reference client caches for the same reason.
//!
//! Stored as plain JSON. The reference client wraps its copy in the same fixed
//! keystream it uses on the wire, but that is obfuscation against someone reading
//! the app's own storage, and reproducing it here would only make the file harder
//! to inspect when a credential misbehaves. It is a private-key-bearing file
//! either way, so it is written `0600` on Unix and lives in the state directory.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::window::Window;
use crate::{CastCredential, CksError, CredentialOrigin};

/// Filename under the state directory.
pub const CACHE_FILE: &str = "cast-cks-credential.json";

/// The default cache path: `<state>/cast-cks-credential.json`.
#[must_use]
pub fn default_path() -> PathBuf {
    castaway_paths::host().state().join(CACHE_FILE)
}

#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    /// Bumped if the on-disk shape ever changes, so an old file is ignored rather
    /// than misread.
    version: u32,
    device_cert: String,
    intermediates: Vec<String>,
    peer_cert: String,
    peer_key_pkcs8: String,
    sha1: String,
    sha256: String,
    window_start: i64,
    window_end: i64,
}

const VERSION: u32 = 1;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(value: &str, what: &str) -> Result<Vec<u8>, CksError> {
    base64::engine::general_purpose::STANDARD
        .decode(value.as_bytes())
        .map_err(|e| CksError::Cache(format!("{what} is not base64: {e}")))
}

/// Read a cached credential.
///
/// Returns `Ok(None)` when there is no cache. A cache that exists but cannot be
/// read is an error, so it gets logged rather than silently treated as absent —
/// a cache that never loads is a bug worth seeing, not a slow path to live with.
///
/// # Errors
/// [`CksError::Cache`] if the file exists but is unreadable, malformed, or written
/// by a different version.
pub fn load(path: &Path) -> Result<Option<CastCredential>, CksError> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CksError::Cache(format!("reading {}: {e}", path.display()))),
    };
    let cached: Cached = serde_json::from_slice(&raw)
        .map_err(|e| CksError::Cache(format!("parsing {}: {e}", path.display())))?;
    if cached.version != VERSION {
        return Err(CksError::Cache(format!(
            "{} is version {}; this build writes {VERSION}",
            path.display(),
            cached.version
        )));
    }
    let credential = CastCredential::new(
        unb64(&cached.device_cert, "device_cert")?,
        cached
            .intermediates
            .iter()
            .map(|i| unb64(i, "intermediates"))
            .collect::<Result<Vec<_>, _>>()?,
        unb64(&cached.peer_cert, "peer_cert")?,
        unb64(&cached.peer_key_pkcs8, "peer_key_pkcs8")?,
        unb64(&cached.sha1, "sha1")?,
        unb64(&cached.sha256, "sha256")?,
        Window::new(cached.window_start, cached.window_end)?,
        CredentialOrigin::Cache,
    )?;
    Ok(Some(credential))
}

/// Write a credential to the cache, atomically.
///
/// # Errors
/// [`CksError::Cache`] if the directory cannot be created or the file written.
pub fn store(path: &Path, credential: &CastCredential) -> Result<(), CksError> {
    let (peer_cert, peer_key) = credential.tls_identity();
    let cached = Cached {
        version: VERSION,
        device_cert: b64(credential.device_cert_der()),
        intermediates: credential
            .intermediates_der()
            .iter()
            .map(|i| b64(i))
            .collect(),
        peer_cert: b64(peer_cert),
        peer_key_pkcs8: b64(peer_key),
        sha1: b64(credential.signature(crate::HashAlgo::Sha1)),
        sha256: b64(credential.signature(crate::HashAlgo::Sha256)),
        window_start: credential.window().start_unix(),
        window_end: credential.window().end_unix(),
    };
    let body = serde_json::to_vec_pretty(&cached)
        .map_err(|e| CksError::Cache(format!("serialising the credential: {e}")))?;

    if let Some(parent) = path.parent() {
        castaway_paths::ensure(parent).map_err(|e| CksError::Cache(e.to_string()))?;
    }
    // Temp-then-rename, so a crash mid-write leaves the previous credential intact
    // rather than a truncated file the next start has to reject.
    let temp = path.with_extension("json.tmp");
    write_private(&temp, &body)?;
    std::fs::rename(&temp, path)
        .map_err(|e| CksError::Cache(format!("installing {}: {e}", path.display())))
}

/// Write `body` to `path`, owner-readable only.
fn write_private(path: &Path, body: &[u8]) -> Result<(), CksError> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| CksError::Cache(format!("creating {}: {e}", path.display())))?;
    file.write_all(body)
        .map_err(|e| CksError::Cache(format!("writing {}: {e}", path.display())))?;
    file.sync_all()
        .map_err(|e| CksError::Cache(format!("flushing {}: {e}", path.display())))
}

/// Remove a cached credential, if one is there.
///
/// # Errors
/// [`CksError::Cache`] if the file exists and cannot be removed.
pub fn clear(path: &Path) -> Result<(), CksError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CksError::Cache(format!("removing {}: {e}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn credential() -> CastCredential {
        CastCredential::new(
            b"device".to_vec(),
            vec![b"ica".to_vec()],
            b"peer".to_vec(),
            b"key".to_vec(),
            vec![0xAA; 256],
            vec![0xBB; 256],
            Window::new(1_785_196_800, 1_785_369_600).unwrap(),
            CredentialOrigin::Network,
        )
        .unwrap()
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cast-cks-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_stored_credential_reads_back_identically() {
        let path = temp_dir().join("roundtrip.json");
        let original = credential();
        store(&path, &original).unwrap();
        let back = load(&path).unwrap().unwrap();

        assert_eq!(back.tls_identity(), original.tls_identity());
        assert_eq!(back.device_cert_der(), original.device_cert_der());
        assert_eq!(back.intermediates_der(), original.intermediates_der());
        assert_eq!(
            back.signature(crate::HashAlgo::Sha256),
            original.signature(crate::HashAlgo::Sha256)
        );
        assert_eq!(back.window(), original.window());
        // The origin is where it came from *now*, not where it came from first.
        assert_eq!(back.origin(), &CredentialOrigin::Cache);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_absent_cache_is_not_an_error() {
        let path = temp_dir().join("does-not-exist.json");
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn a_corrupt_cache_is_reported_rather_than_ignored() {
        let path = temp_dir().join("corrupt.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(matches!(load(&path), Err(CksError::Cache(_))));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_cache_from_another_version_is_rejected() {
        let path = temp_dir().join("old-version.json");
        std::fs::write(
            &path,
            br#"{"version":999,"device_cert":"","intermediates":[],
            "peer_cert":"","peer_key_pkcs8":"","sha1":"","sha256":"",
            "window_start":1,"window_end":2}"#,
        )
        .unwrap();
        assert!(matches!(load(&path), Err(CksError::Cache(_))));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn clearing_an_absent_cache_succeeds() {
        assert!(clear(&temp_dir().join("nothing-here.json")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_cache_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = temp_dir().join("perms.json");
        store(&path, &credential()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the cache holds a private key");
        std::fs::remove_file(&path).ok();
    }
}
