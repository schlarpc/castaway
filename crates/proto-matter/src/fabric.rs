//! The fabric the panel administers, and the certificate authority behind it.
//!
//! In ordinary Matter a device is *given* a fabric by whatever commissioned it. Casting
//! inverts that: the panel is the administrator, so it has to be a CA — it mints a root
//! certificate, an operational certificate for itself, and one more for every phone it
//! ever commissions.
//!
//! ## Why the root key stays here
//!
//! `rs-matter`'s own commissioning example generates a root, signs an intermediate with
//! it, and throws the root key away, on the reasoning that in production it lives in an
//! HSM. There is no HSM here and no second machine: the panel *is* the authority, and a
//! root key it cannot use is a root it cannot issue against after the first restart. So
//! the panel keeps the root key and signs NOCs directly with it — `rs-matter` calls this
//! RCAC-direct mode and supports it explicitly.
//!
//! What that costs is worth stating plainly: anyone who can read the panel's state
//! directory can mint an identity on this fabric. That is the same exposure as the
//! panel's other stored credentials and it is bounded by what the fabric can do, which is
//! drive this one screen.
//!
//! ## What is persisted, and why it is ours rather than `rs-matter`'s
//!
//! Three files and a list. The fabric is *reconstructed* at every boot from the root key
//! and a few numbers rather than being restored from `rs-matter`'s own key-value persist:
//! a fresh operational certificate signed by the same root, for the same node id, is
//! indistinguishable to a client from the one we had yesterday, and rebuilding it means
//! there is exactly one code path that produces a fabric instead of two.
//!
//! The part that genuinely has to survive is the *list of phones* — a client commissioned
//! yesterday must still be allowed to speak today, and that is an access-control entry
//! keyed by its node id.

use std::fs;
use std::path::{Path, PathBuf};

use rs_matter::cert::gen::{Validity, VALID_FOREVER};
use rs_matter::cert::{MAX_CERT_TLV_AND_ASN1_LEN, MAX_CERT_TLV_LEN};
use rs_matter::crypto::{CanonAeadKey, Crypto};
use rs_matter::onboard::cac::RcacGenerator;
use rs_matter::onboard::noc::NocGenerator;

use rand_core::RngCore;

use crate::error::MatterError;

/// File names under the state directory. Raw bytes, one artefact per file: the blobs are
/// binary and wrapping them in a text encoding would buy nothing but a decoder.
const ROOT_KEY_FILE: &str = "root.key";
const ROOT_CERT_FILE: &str = "root.matter-cert";
const IPK_FILE: &str = "ipk.bin";
const CLIENTS_FILE: &str = "clients.tsv";

/// The panel's fabric id. One fabric, so it is a constant rather than a stored number —
/// it appears inside the root certificate, which is stored, and the two must agree.
const FABRIC_ID: u64 = 1;

/// The panel's own node id on that fabric.
const PANEL_NODE_ID: u64 = 1;

/// Node ids handed to commissioned clients start here, so they can never collide with the
/// panel's own and are recognisable in a log line.
const FIRST_CLIENT_NODE_ID: u64 = 0x1000;

/// The vendor id recorded as the fabric's administrator. `0xFFF1` is the CSA's test
/// vendor range, which is what this is: an uncertified receiver, saying so.
pub const ADMIN_VENDOR_ID: u16 = 0xFFF1;

/// A phone that has been commissioned onto the panel's fabric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionedClient {
    /// The node id we assigned it. The subject of its ACL entry.
    pub node_id: u64,
    /// The commissionable instance name it declared over UDC. The join key back to a
    /// returning phone, so a second cast does not commission it a second time.
    pub instance: String,
    /// What it called itself, for the log and the prompt.
    pub name: String,
}

/// The certificate authority and the phones it has admitted.
///
/// Loaded from the state directory, or created there on first run.
#[derive(Debug)]
pub struct CastingCa {
    dir: PathBuf,
    root_key: Vec<u8>,
    root_cert: Vec<u8>,
    ipk: Vec<u8>,
    clients: Vec<CommissionedClient>,
}

impl CastingCa {
    /// Load the CA from `dir`, generating one if it is not there yet.
    ///
    /// # Errors
    /// [`MatterError::Io`] if the directory cannot be read or written, or
    /// [`MatterError::Core`] if key generation fails.
    pub fn open<C: Crypto>(dir: impl Into<PathBuf>, crypto: &C) -> Result<Self, MatterError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|source| MatterError::Io {
            context: "creating the matter state directory",
            source,
        })?;

        let clients = read_clients(&dir.join(CLIENTS_FILE))?;

        let key_path = dir.join(ROOT_KEY_FILE);
        let cert_path = dir.join(ROOT_CERT_FILE);
        let ipk_path = dir.join(IPK_FILE);

        if let (Some(root_key), Some(root_cert), Some(ipk)) = (
            read_optional(&key_path)?,
            read_optional(&cert_path)?,
            read_optional(&ipk_path)?,
        ) {
            return Ok(Self {
                dir,
                root_key,
                root_cert,
                ipk,
                clients,
            });
        }

        tracing::info!(dir = %dir.display(), "matter: generating a casting fabric");

        let mut cert_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
        let mut generator = RcacGenerator::new(&mut cert_buf);
        let (root_key, root_cert) = generator
            .generate(crypto, FABRIC_ID, VALID_FOREVER)
            .map_err(core_err)?;

        let mut ipk = CanonAeadKey::new();
        crypto
            .rand()
            .map_err(core_err)?
            .fill_bytes(ipk.access_mut());

        let this = Self {
            dir,
            root_key: root_key.access().to_vec(),
            root_cert: root_cert.to_vec(),
            ipk: ipk.access().to_vec(),
            clients,
        };

        // Written with owner-only permissions where the platform has them: the root key
        // is the fabric.
        write_private(&key_path, &this.root_key)?;
        write_private(&cert_path, &this.root_cert)?;
        write_private(&ipk_path, &this.ipk)?;

        Ok(this)
    }

    /// The root certificate, in Matter TLV form.
    #[must_use]
    pub fn root_cert(&self) -> &[u8] {
        &self.root_cert
    }

    /// The identity protection key shared across the fabric.
    #[must_use]
    pub fn ipk(&self) -> &[u8] {
        &self.ipk
    }

    /// The canonical bytes of the root signing key.
    #[must_use]
    pub fn root_key(&self) -> &[u8] {
        &self.root_key
    }

    /// The panel's own node id.
    #[must_use]
    pub const fn panel_node_id() -> u64 {
        PANEL_NODE_ID
    }

    /// Every phone commissioned so far.
    #[must_use]
    pub fn clients(&self) -> &[CommissionedClient] {
        &self.clients
    }

    /// The client previously commissioned under this instance name, if any.
    ///
    /// A returning phone re-sends UDC with the same instance name; recognising it is what
    /// keeps a second cast from minting a second identity for the same device and leaving
    /// the first one in the access-control list forever.
    #[must_use]
    pub fn client_for(&self, instance: &str) -> Option<&CommissionedClient> {
        self.clients.iter().find(|c| c.instance == instance)
    }

    /// Record a newly commissioned client and persist the list.
    ///
    /// # Errors
    /// [`MatterError::Io`] if the list cannot be written.
    pub fn remember(&mut self, client: CommissionedClient) -> Result<(), MatterError> {
        self.clients.retain(|c| c.instance != client.instance);
        self.clients.push(client);
        write_clients(&self.dir.join(CLIENTS_FILE), &self.clients)
    }

    /// The node id to assign the next phone.
    #[must_use]
    pub fn next_node_id(&self) -> u64 {
        self.clients
            .iter()
            .map(|c| c.node_id)
            .max()
            .map_or(FIRST_CLIENT_NODE_ID, |max| max + 1)
    }

    /// The fabric id the root certificate was issued for.
    #[must_use]
    pub const fn fabric_id() -> u64 {
        FABRIC_ID
    }

    /// How long the certificates this CA issues are good for.
    ///
    /// Forever, deliberately. The panel has no real-time clock it trusts on a cold boot
    /// (`rs-matter` seeds "last known good UTC" from the *build* timestamp), so a bounded
    /// window would be validated against a clock that can be behind it — and the failure
    /// mode of an expired operational certificate is a phone that pairs and then silently
    /// cannot talk.
    #[must_use]
    pub const fn validity() -> Validity {
        VALID_FOREVER
    }
}

/// Build a NOC generator that signs with the panel's root key directly.
///
/// `rs-matter` calls the empty intermediate slice RCAC-direct mode.
///
/// # Errors
/// [`MatterError::Core`] if the root certificate does not parse.
pub fn noc_generator<'a>(
    ca: &CastingCa,
    key: rs_matter::crypto::CanonPkcSecretKeyRef<'a>,
    buf: &'a mut [u8],
) -> Result<NocGenerator<'a>, MatterError> {
    let _ = ca;
    NocGenerator::create(key, ca.root_cert(), &[], buf).map_err(core_err)
}

/// Scratch space a [`rs_matter::onboard::Commissioner`] needs across its awaits.
#[must_use]
pub fn commissioner_scratch() -> Vec<u8> {
    vec![0u8; MAX_CERT_TLV_LEN]
}

fn core_err(e: rs_matter::error::Error) -> MatterError {
    MatterError::Core(e.to_string())
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, MatterError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(MatterError::Io {
            context: "reading the matter fabric",
            source,
        }),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), MatterError> {
    fs::write(path, bytes).map_err(|source| MatterError::Io {
        context: "writing the matter fabric",
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            MatterError::Io {
                context: "restricting permissions on the matter fabric",
                source,
            }
        })?;
    }

    Ok(())
}

/// The client list, one tab-separated line each.
///
/// A text format rather than a serialization: three fields, one of which a person may
/// want to read to answer "which phones can drive this panel?", and the answer to that
/// should not need a tool.
fn read_clients(path: &Path) -> Result<Vec<CommissionedClient>, MatterError> {
    let Some(bytes) = read_optional(path)? else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8_lossy(&bytes);

    let mut clients = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(node_id), Some(instance)) = (fields.next(), fields.next()) else {
            tracing::warn!(%line, "matter: skipping a malformed client record");
            continue;
        };
        let Ok(node_id) = node_id.parse::<u64>() else {
            tracing::warn!(%line, "matter: skipping a client record with an unreadable node id");
            continue;
        };
        clients.push(CommissionedClient {
            node_id,
            instance: instance.to_string(),
            name: fields.next().unwrap_or("").to_string(),
        });
    }

    Ok(clients)
}

fn write_clients(path: &Path, clients: &[CommissionedClient]) -> Result<(), MatterError> {
    let mut out = String::from("# node id\tinstance name\tdevice name\n");
    for client in clients {
        // Tabs and newlines cannot appear in a field, so a device name carrying one is
        // flattened rather than allowed to forge a record.
        let instance = sanitize(&client.instance);
        let name = sanitize(&client.name);
        out.push_str(&format!("{}\t{instance}\t{name}\n", client.node_id));
    }

    write_private(path, out.as_bytes())
}

fn sanitize(field: &str) -> String {
    field
        .chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn client(node_id: u64, instance: &str, name: &str) -> CommissionedClient {
        CommissionedClient {
            node_id,
            instance: instance.into(),
            name: name.into(),
        }
    }

    #[test]
    fn a_fabric_is_generated_once_and_reloaded_after() {
        let dir = tempfile::tempdir().unwrap();
        let crypto = rs_matter::crypto::test_only_crypto();

        let first = CastingCa::open(dir.path(), &crypto).unwrap();
        let second = CastingCa::open(dir.path(), &crypto).unwrap();

        assert_eq!(first.root_cert(), second.root_cert());
        assert_eq!(first.root_key(), second.root_key());
        assert_eq!(first.ipk(), second.ipk());
        assert!(!first.root_cert().is_empty());
    }

    /// A phone commissioned yesterday must still be allowed to speak today, which is the
    /// one thing here that genuinely has to survive a restart.
    #[test]
    fn commissioned_clients_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let crypto = rs_matter::crypto::test_only_crypto();

        let mut ca = CastingCa::open(dir.path(), &crypto).unwrap();
        ca.remember(client(ca.next_node_id(), "AABBCCDD", "Chaz's phone"))
            .unwrap();

        let reopened = CastingCa::open(dir.path(), &crypto).unwrap();
        assert_eq!(
            reopened.client_for("AABBCCDD"),
            Some(&client(FIRST_CLIENT_NODE_ID, "AABBCCDD", "Chaz's phone"))
        );
    }

    /// A returning phone re-sends the same instance name. Minting a second identity for
    /// it would leave the first in the access-control list forever.
    #[test]
    fn a_returning_client_replaces_its_own_record() {
        let dir = tempfile::tempdir().unwrap();
        let crypto = rs_matter::crypto::test_only_crypto();
        let mut ca = CastingCa::open(dir.path(), &crypto).unwrap();

        ca.remember(client(0x1000, "AABBCCDD", "old name")).unwrap();
        ca.remember(client(0x1000, "AABBCCDD", "new name")).unwrap();

        assert_eq!(ca.clients().len(), 1);
        assert_eq!(ca.client_for("AABBCCDD").unwrap().name, "new name");
    }

    #[test]
    fn node_ids_do_not_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let crypto = rs_matter::crypto::test_only_crypto();
        let mut ca = CastingCa::open(dir.path(), &crypto).unwrap();

        assert_eq!(ca.next_node_id(), FIRST_CLIENT_NODE_ID);
        ca.remember(client(ca.next_node_id(), "one", "")).unwrap();
        assert_eq!(ca.next_node_id(), FIRST_CLIENT_NODE_ID + 1);
        ca.remember(client(ca.next_node_id(), "two", "")).unwrap();
        assert_eq!(ca.next_node_id(), FIRST_CLIENT_NODE_ID + 2);
        assert_ne!(ca.clients()[0].node_id, ca.clients()[1].node_id);
    }

    /// A device name is whatever a phone says it is, including a tab.
    #[test]
    fn a_device_name_cannot_forge_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let crypto = rs_matter::crypto::test_only_crypto();
        let mut ca = CastingCa::open(dir.path(), &crypto).unwrap();

        ca.remember(client(0x1000, "AABB", "evil\n99999\tCCDD\tsmuggled"))
            .unwrap();

        let reopened = CastingCa::open(dir.path(), &crypto).unwrap();
        assert_eq!(reopened.clients().len(), 1);
        assert_eq!(reopened.clients()[0].node_id, 0x1000);
        assert_eq!(reopened.clients()[0].name, "evil 99999 CCDD smuggled");
    }
}
