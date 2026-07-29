//! AirServer's live credential endpoint.
//!
//! The request was recovered statically from `AirServer.exe` 2025.7.23 rather than
//! guessed — the URL literal at `.rdata 0x140b81dc0`, wrapped into a `QUrl` global,
//! whose only consumer is the request builder at `0x14012dc11`:
//!
//! ```text
//! POST https://api.airserver.com/cast_certificates/get
//! Content-Type: application/json
//! AD-Redirect-Supported: 1
//! AD-Db-Schema-Version: 2
//! body: []
//! ```
//!
//! `AD-` is App Dynamic. The schema version matches the generation the client itself
//! probes for at runtime (`pragma_table_info('jwt_token_header') WHERE name =
//! 'includes_chain'`), so it is a real compatibility assertion rather than a
//! constant. Full notes:
//! `re-shell/artifacts/airreceiver-cast-signatures/AIRSERVER_HANDOFF.md`.
//!
//! ## The body is `[]`, and that is a known gap
//!
//! The body is a JSON *array* assembled in a loop at `0x14012e100` over a `QList`
//! the caller holds; the element shape was never recovered. An empty array is
//! accepted and returns a complete, current credential set, so that is what is sent.
//! It works, but it is the one part of this request that is "sufficient" rather than
//! "faithful", and a future schema version could start requiring an element.
//!
//! ## What comes back
//!
//! A whole SQLite database — ~14 MB, one identity, 30 rolling windows, and 20 520
//! JWTs this project neither reads nor wants (D42). It is written to disk and opened
//! with [`crate::airserver_db`], which reads six tables and ignores the rest.
//!
//! The response identity is *not* the bundled one: observed fetches return a
//! different SHIELD leaf, so there is a pool or a rotation behind the endpoint.
//! Nothing here depends on which identity arrives — the credential carries its own
//! chain, and [`crate::CastCredential`] keeps certificate and signature together.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::airserver_db::MAX_DB_BYTES;
use crate::ReplayError;

/// The endpoint.
pub const URL: &str = "https://api.airserver.com/cast_certificates/get";

/// The client's own User-Agent. Sent verbatim: this is someone else's API and
/// pretending to be a browser would be less honest, not more.
pub const USER_AGENT: &str = "AirServer";

/// `AD-Db-Schema-Version`. Bumping this without checking what the new schema looks
/// like would be a way to silently start receiving a shape the reader cannot map.
pub const SCHEMA_VERSION: &str = "2";

/// Filename for the cached database under the state directory.
pub const DB_FILE: &str = "cast-replay-airserver.sqlite";

/// The default cache path for a fetched database.
#[must_use]
pub fn default_db_path() -> PathBuf {
    castaway_paths::host().state().join(DB_FILE)
}

/// Fetch a credential database and write it to `dest`, replacing what was there.
///
/// Blocking — call it from `spawn_blocking` (ground rule 4). Returns the number of
/// bytes written.
///
/// Written to a `.part` file and renamed on success, so a fetch that dies halfway
/// cannot leave a truncated database where the reader will find it and a torn file
/// cannot be mistaken for a stale-but-valid one.
///
/// # Errors
/// [`ReplayError::Http`] for a transport failure or a non-200 status,
/// [`ReplayError::Response`] if the body is not a SQLite database or exceeds
/// [`MAX_DB_BYTES`], and [`ReplayError::Cache`] if it cannot be written.
pub fn fetch_to(dest: &Path, timeout: Duration) -> Result<u64, ReplayError> {
    let response = ureq::builder()
        .timeout(timeout)
        // The recovered request sets NoLessSafeRedirectPolicy. ureq's default is to
        // follow redirects but never downgrade https→http, which is the same rule.
        .redirects(4)
        .build()
        .post(URL)
        .set("Content-Type", "application/json")
        .set("AD-Redirect-Supported", "1")
        .set("AD-Db-Schema-Version", SCHEMA_VERSION)
        .set("User-Agent", USER_AGENT)
        .send_bytes(b"[]")
        .map_err(|e| ReplayError::Http(format!("POST {URL}: {e}")))?;

    if response.status() != 200 {
        return Err(ReplayError::Http(format!(
            "{URL} answered {} {}",
            response.status(),
            response.status_text()
        )));
    }

    // Read through a cap: this is an unauthenticated third-party endpoint and the
    // panel has finite memory and disk.
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_DB_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|e| ReplayError::Http(format!("reading the response body: {e}")))?;
    let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
    if len > MAX_DB_BYTES {
        return Err(ReplayError::Response(format!(
            "the response exceeds {MAX_DB_BYTES} bytes"
        )));
    }

    // Check the magic before writing, so a captive-portal HTML page or an error
    // document never lands where the reader expects a database.
    if !body.starts_with(b"SQLite format 3\0") {
        return Err(ReplayError::Response(format!(
            "the response is not a SQLite database ({} bytes, starts {:?})",
            body.len(),
            String::from_utf8_lossy(&body[..body.len().min(48)])
        )));
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ReplayError::Cache(format!("creating {}: {e}", parent.display())))?;
    }
    let partial = dest.with_extension("part");
    std::fs::write(&partial, &body)
        .map_err(|e| ReplayError::Cache(format!("writing {}: {e}", partial.display())))?;
    restrict(&partial)?;
    std::fs::rename(&partial, dest)
        .map_err(|e| ReplayError::Cache(format!("renaming into {}: {e}", dest.display())))?;
    Ok(len)
}

/// Tighten permissions: the database carries a private key.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), ReplayError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| ReplayError::Cache(format!("chmod {}: {e}", path.display())))
}

/// No equivalent on Windows; the state directory is per-user there.
#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), ReplayError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The constants are the recovered ones. Asserted because a typo here produces a
    /// 404 or a wrong-schema response rather than anything obviously broken.
    #[test]
    fn the_request_matches_what_was_recovered() {
        assert_eq!(URL, "https://api.airserver.com/cast_certificates/get");
        assert_eq!(SCHEMA_VERSION, "2");
        assert_eq!(USER_AGENT, "AirServer");
    }

    #[test]
    fn the_cache_path_is_under_the_state_directory() {
        let path = default_db_path();
        assert!(path.ends_with(DB_FILE));
        assert_eq!(path.parent(), Some(castaway_paths::host().state()));
    }
}
