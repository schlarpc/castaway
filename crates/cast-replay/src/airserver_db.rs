//! Reading an AirServer credential database.
//!
//! [`crate::airserver`] serves the *bundled* identity from checked-in fixtures.
//! This reads a whole database file, which is what AirServer's live endpoint
//! answers with, so a fetched credential set can be used and re-used the same way.
//!
//! ## The container
//!
//! Every BLOB in the database is a libsodium `crypto_secretbox` —
//! XSalsa20-Poly1305 — laid out as `nonce(24) || tag(16) || ciphertext`, under a
//! key derived once per database:
//!
//! ```text
//! key = BLAKE2b-256(message = "", key = PASS, salt = <salt table>, person = PERSON)
//! ```
//!
//! which is libsodium's `crypto_generichash_blake2b_salt_personal`. `PASS` and
//! `PERSON` are string literals in AirServer's shipped binary; the salt is a row in
//! the database's own `salt` table, so it varies per database and the key must be
//! derived from the file rather than hardcoded.
//!
//! Provenance and the recovery of those two constants:
//! `re-shell/artifacts/airreceiver-cast-signatures/AIRSERVER_HANDOFF.md`.
//!
//! ## Scope
//!
//! Six tables are read: `salt`, `metadata`, `device_info`, `device_cert_chain`,
//! `daily_private` and `daily_cert`. The seventh, `jwt_token`, is **never touched**
//! — it holds live bearer credentials for outbound app identification, which this
//! project does not implement (D42), and a live response carries 20 520 of them. Not
//! reading it is also why a 13.6 MB response costs little to ingest.

use std::path::Path;

use blake2b_simd::Params as Blake2bParams;
use crypto_secretbox::aead::Aead as _;
use crypto_secretbox::{KeyInit as _, XSalsa20Poly1305};
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs8::EncodePrivateKey as _;
use rsa::RsaPrivateKey;

use crate::provider::OfflineIdentity;
use crate::window::Window;
use crate::{CastCredential, CredentialOrigin, ReplayError};

/// The BLAKE2b personalisation constant, from AirServer's binary.
const PERSON: &[u8] = b"***REMOVED: App Dynamic BLAKE2b personalisation, PROVENANCE S5***";

/// The BLAKE2b key constant, from AirServer's binary.
const PASS: &[u8] = b"***REMOVED: App Dynamic BLAKE2b key, PROVENANCE S6***";

/// `crypto_secretbox` nonce length.
const NONCE_LEN: usize = 24;

/// Poly1305 tag length.
const TAG_LEN: usize = 16;

/// Bytes per precomputed signature (RSA-2048).
const SIGNATURE_LEN: usize = 256;

/// Refuse absurd databases before handing anything to SQLite. A live response is
/// ~14 MB; this is generous enough for growth and small enough to bound memory on a
/// panel.
pub const MAX_DB_BYTES: u64 = 128 * 1024 * 1024;

/// One window's material, decrypted.
#[derive(Debug, Clone)]
struct DailyCert {
    window: Window,
    peer_cert_der: Vec<u8>,
    sha1: Vec<u8>,
    sha256: Vec<u8>,
}

/// An opened AirServer credential set: one identity and its windows.
#[derive(Debug, Clone)]
pub struct AirServerDb {
    device_cert_der: Vec<u8>,
    chain_der: Vec<Vec<u8>>,
    peer_key_pkcs8_der: Vec<u8>,
    windows: Vec<DailyCert>,
    generated_unix: Option<i64>,
}

impl AirServerDb {
    /// Open and decrypt a database at `path`.
    ///
    /// # Errors
    /// [`ReplayError::Database`] for anything malformed — a wrong key surfaces as a
    /// Poly1305 authentication failure, which is the honest description of "this is
    /// not an AirServer database".
    pub fn open(path: &Path) -> Result<Self, ReplayError> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| ReplayError::Database(format!("opening {}: {e}", path.display())))?;

        let salt: Vec<u8> = conn
            .query_row("SELECT data FROM salt LIMIT 1", [], |r| r.get(0))
            .map_err(|e| ReplayError::Database(format!("reading the salt: {e}")))?;
        let key = derive_key(&salt)?;

        // `metadata.json` is the one column in the schema that is *not* a secretbox —
        // it is declared TEXT and stored in the clear, holding `{"generated": <unix>}`.
        // That field is what App Dynamic's own policy file keys on to force clients off
        // a stale database, so it is worth surfacing; it is not load-bearing here.
        let generated_unix = conn
            .query_row("SELECT json FROM metadata LIMIT 1", [], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .ok()
            .and_then(|json| {
                serde_json::from_slice::<serde_json::Value>(&json)
                    .ok()?
                    .get("generated")?
                    .as_i64()
            });

        let device_cert_der =
            decrypt_one(&conn, "SELECT device_cert FROM device_info LIMIT 1", &key)
                .map_err(|e| ReplayError::Database(format!("device certificate: {e}")))?;

        let mut chain_stmt = conn
            .prepare("SELECT data FROM device_cert_chain ORDER BY pos")
            .map_err(|e| ReplayError::Database(format!("preparing the chain query: {e}")))?;
        let chain_rows = chain_stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .map_err(|e| ReplayError::Database(format!("reading the chain: {e}")))?;
        let mut chain_der = Vec::new();
        for row in chain_rows {
            let blob = row.map_err(|e| ReplayError::Database(format!("chain row: {e}")))?;
            chain_der.push(open_box(&blob, &key)?);
        }

        let peer_key_pkcs1 = decrypt_one(&conn, "SELECT data FROM daily_private LIMIT 1", &key)
            .map_err(|e| ReplayError::Database(format!("peer key: {e}")))?;
        // Stored PKCS#1, needed as PKCS#8 by rustls — the same conversion the bundled
        // table does.
        let peer_key = RsaPrivateKey::from_pkcs1_der(&peer_key_pkcs1)
            .map_err(|e| ReplayError::InvalidKey(format!("AirServer peer key: {e}")))?;
        let peer_key_pkcs8_der = peer_key
            .to_pkcs8_der()
            .map_err(|e| ReplayError::InvalidKey(format!("re-encoding the peer key: {e}")))?
            .as_bytes()
            .to_vec();

        let mut stmt = conn
            .prepare(
                "SELECT start_time, end_time, cert, sha1, sha256 \
                 FROM daily_cert ORDER BY start_time",
            )
            .map_err(|e| ReplayError::Database(format!("preparing the window query: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                ))
            })
            .map_err(|e| ReplayError::Database(format!("reading windows: {e}")))?;

        let mut windows = Vec::new();
        for row in rows {
            let (start, end, cert, sha1, sha256) =
                row.map_err(|e| ReplayError::Database(format!("window row: {e}")))?;
            let sha1 = open_box(&sha1, &key)?;
            let sha256 = open_box(&sha256, &key)?;
            for (what, sig) in [("SHA-1", &sha1), ("SHA-256", &sha256)] {
                if sig.len() != SIGNATURE_LEN {
                    return Err(ReplayError::Database(format!(
                        "{what} signature for window starting {start} is {} bytes, not \
                         {SIGNATURE_LEN}",
                        sig.len()
                    )));
                }
            }
            windows.push(DailyCert {
                window: Window::new(start, end)?,
                peer_cert_der: open_box(&cert, &key)?,
                sha1,
                sha256,
            });
        }
        if windows.is_empty() {
            return Err(ReplayError::Database(
                "the database holds no windows".into(),
            ));
        }
        if device_cert_der.is_empty() || chain_der.is_empty() {
            return Err(ReplayError::Database(
                "the database holds no device certificate chain".into(),
            ));
        }

        Ok(Self {
            device_cert_der,
            chain_der,
            peer_key_pkcs8_der,
            windows,
            generated_unix,
        })
    }

    /// When this credential set was generated, if `metadata` said.
    #[must_use]
    pub const fn generated_unix(&self) -> Option<i64> {
        self.generated_unix
    }

    /// How many windows it carries.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// The last instant any window covers, exclusive — the horizon of this set.
    #[must_use]
    pub fn covers_until(&self) -> i64 {
        self.windows
            .iter()
            .map(|w| w.window.end_unix())
            .max()
            .unwrap_or(0)
    }

    /// The credential for `unix`, choosing the covering window with the most life
    /// left.
    ///
    /// A live set's windows overlap exactly as the bundled table's do, so more than
    /// one can qualify; picking the latest end minimises the chance of a roll landing
    /// mid-session.
    ///
    /// # Errors
    /// [`ReplayError::OutOfRange`] if no window covers `unix`.
    pub fn credential_at(&self, unix: i64) -> Result<CastCredential, ReplayError> {
        let best = self
            .windows
            .iter()
            .filter(|w| w.window.contains(unix))
            .max_by_key(|w| w.window.end_unix())
            .ok_or(ReplayError::OutOfRange {
                identity: OfflineIdentity::AirServer,
                unix,
                covers_until: self.covers_until(),
            })?;

        CastCredential::new(
            self.device_cert_der.clone(),
            self.chain_der.clone(),
            best.peer_cert_der.clone(),
            self.peer_key_pkcs8_der.clone(),
            best.sha1.clone(),
            best.sha256.clone(),
            best.window,
            CredentialOrigin::AirServerLive,
        )
    }
}

/// `crypto_generichash_blake2b_salt_personal` with an empty message.
fn derive_key(salt: &[u8]) -> Result<[u8; 32], ReplayError> {
    // libsodium's salt and personal fields are exactly 16 bytes; it zero-pads a
    // shorter input and rejects a longer one. blake2b_simd requires exactly 16, so
    // pad here rather than letting a short salt panic.
    let mut salt16 = [0_u8; 16];
    let mut person16 = [0_u8; 16];
    if salt.len() > 16 || PERSON.len() > 16 {
        return Err(ReplayError::Database(format!(
            "salt is {} bytes; BLAKE2b takes at most 16",
            salt.len()
        )));
    }
    salt16[..salt.len()].copy_from_slice(salt);
    person16[..PERSON.len()].copy_from_slice(PERSON);

    let hash = Blake2bParams::new()
        .hash_length(32)
        .key(PASS)
        .salt(&salt16)
        .personal(&person16)
        .to_state()
        .update(b"")
        .finalize();
    let mut key = [0_u8; 32];
    key.copy_from_slice(hash.as_bytes());
    Ok(key)
}

/// Open one libsodium `crypto_secretbox`: `nonce(24) || tag(16) || ciphertext`.
///
/// `crypto_secretbox`'s AEAD impl uses libsodium's own layout — tag first — so the
/// body after the nonce is handed over verbatim. (Established by experiment against a
/// real database rather than assumed: the RustCrypto convention elsewhere is to
/// *append* the tag, and guessing wrong here fails authentication on every blob.)
fn open_box(blob: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, ReplayError> {
    let nonce = blob
        .get(..NONCE_LEN)
        .ok_or_else(|| ReplayError::Database("secretbox shorter than its nonce".into()))?;
    // Must hold at least the tag; an empty ciphertext is legal.
    let body = blob
        .get(NONCE_LEN..)
        .filter(|b| b.len() >= TAG_LEN)
        .ok_or_else(|| ReplayError::Database("secretbox shorter than its tag".into()))?;

    let nonce = crypto_secretbox::Nonce::try_from(nonce)
        .map_err(|_| ReplayError::Database("secretbox nonce is not 24 bytes".into()))?;
    XSalsa20Poly1305::new(key.into())
        .decrypt(&nonce, body)
        .map_err(|_| {
            ReplayError::Database(
                "secretbox authentication failed \u{2014} wrong key, or not an AirServer \
                 database"
                    .into(),
            )
        })
}

/// Fetch one BLOB with `sql` and decrypt it.
fn decrypt_one(
    conn: &rusqlite::Connection,
    sql: &str,
    key: &[u8; 32],
) -> Result<Vec<u8>, ReplayError> {
    let blob: Vec<u8> = conn
        .query_row(sql, [], |r| r.get(0))
        .map_err(|e| ReplayError::Database(e.to_string()))?;
    open_box(&blob, key)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Trimmed from AirServer's bundled database by
    /// `airserver_castdb.py`: full schema, the six tables the receiver reads, three
    /// windows, and `jwt_token` deliberately empty.
    const TRIMMED: &[u8] = include_bytes!("../fixtures/airserver/db_trimmed.sqlite");

    fn open_trimmed() -> (tempfile::TempDir, AirServerDb) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cast.db");
        std::fs::write(&path, TRIMMED).unwrap();
        let db = AirServerDb::open(&path).unwrap();
        (dir, db)
    }

    /// The whole container, end to end: BLAKE2b-with-salt-and-personal derives a key
    /// that opens every secretbox in a real database. If either constant or the KDF
    /// parameterisation were wrong, every `open_box` would fail its Poly1305 check.
    #[test]
    fn a_real_database_decrypts() {
        let (_dir, db) = open_trimmed();
        assert_eq!(db.window_count(), 3);
        assert_eq!(db.generated_unix(), Some(1_710_925_317));
    }

    /// The decrypted material must match what the checked-in fixtures hold, because
    /// the fixtures came out of this same database by a different route (the Python
    /// exporter). Agreement across two independent implementations is the real check
    /// on the crypto.
    #[test]
    fn it_agrees_with_the_checked_in_fixtures() {
        let (_dir, db) = open_trimmed();
        let table = crate::AirServerTable::load().unwrap();

        // Window 0 is AirServer's epoch, present in both.
        let from_db = db.credential_at(1_710_892_800).unwrap();
        let from_table = table.credential_at(1_710_892_800).unwrap();

        assert_eq!(from_db.device_cert_der(), from_table.device_cert_der());
        assert_eq!(from_db.peer_cert_der(), from_table.peer_cert_der());
        assert_eq!(from_db.intermediates_der(), from_table.intermediates_der());
        for hash in [crate::HashAlgo::Sha1, crate::HashAlgo::Sha256] {
            assert_eq!(
                from_db.signature(hash),
                from_table.signature(hash),
                "{hash:?} signature differs between the database and the fixtures"
            );
        }
        assert_eq!(from_db.window(), from_table.window());
    }

    /// A live set is distinguishable from the bundled table in the logs, which is the
    /// point of a separate origin.
    #[test]
    fn a_database_credential_reports_its_own_origin() {
        let (_dir, db) = open_trimmed();
        let c = db.credential_at(1_710_892_800).unwrap();
        assert_eq!(c.origin(), &CredentialOrigin::AirServerLive);
        assert!(!c.origin().is_offline_table());
    }

    #[test]
    fn outside_every_window_is_out_of_range() {
        let (_dir, db) = open_trimmed();
        assert!(matches!(
            db.credential_at(1_710_892_800 - 1),
            Err(ReplayError::OutOfRange { .. })
        ));
        assert!(matches!(
            db.credential_at(db.covers_until()),
            Err(ReplayError::OutOfRange { .. })
        ));
    }

    /// Wrong bytes must be a typed error, not a panic — this opens a file written
    /// from a network response.
    #[test]
    fn a_non_database_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.db");
        std::fs::write(&path, b"not a database at all").unwrap();
        assert!(matches!(
            AirServerDb::open(&path),
            Err(ReplayError::Database(_))
        ));
    }

    /// A database whose blobs are encrypted under a different key must fail
    /// authentication rather than yielding garbage. Simulated by corrupting the salt,
    /// which changes the derived key.
    #[test]
    fn a_wrong_key_fails_authentication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cast.db");
        std::fs::write(&path, TRIMMED).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("UPDATE salt SET data = ?1", [vec![0_u8; 16]])
            .unwrap();
        drop(conn);
        match AirServerDb::open(&path) {
            Err(ReplayError::Database(msg)) => {
                assert!(
                    msg.contains("authentication failed"),
                    "expected an authentication failure, got: {msg}"
                );
            }
            other => panic!("expected a Database error, got {other:?}"),
        }
    }

    /// The KDF pads a short salt the way libsodium does and refuses an over-long one
    /// rather than panicking inside blake2b_simd.
    #[test]
    fn kdf_matches_the_reference_implementation() {
        let salt = hex_literal::hex!("a8c8de87cdfc203a9cae9f361f82e253");
        let expect =
            hex_literal::hex!("bb358af411634b3b21312fa267bafe6a681d233441e6ce6c55b1ca132947a6da");
        assert_eq!(derive_key(&salt).unwrap(), expect);
    }

    #[test]
    fn the_kdf_bounds_its_inputs() {
        assert!(derive_key(&[1, 2, 3]).is_ok());
        assert!(derive_key(&[0_u8; 16]).is_ok());
        assert!(matches!(
            derive_key(&[0_u8; 17]),
            Err(ReplayError::Database(_))
        ));
    }

    #[test]
    fn a_truncated_secretbox_is_an_error() {
        let key = [7_u8; 32];
        for len in [0, 1, 23, 24, 39] {
            assert!(
                open_box(&vec![0_u8; len], &key).is_err(),
                "a {len}-byte box must not decode"
            );
        }
    }
}
