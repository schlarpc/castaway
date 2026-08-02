//! The credential provider: fallback order, disk cache, and background refresh.
//!
//! This is the only module here that touches a socket or the filesystem
//! (ground rule 3). Everything it decides is decided from [`crate::api`],
//! [`crate::cks`] and [`crate::cache`], which are pure.
//!
//! ## Resolution order
//!
//! Within one identity: **cache → backend → table**.
//!
//! 1. **The cache**, if it holds a credential still inside its window. A cached
//!    credential *is* a previously fetched one, so using it is not a downgrade —
//!    it just avoids spending a request per restart on material we already have.
//! 2. **The backend.** Preferred over any table because it keeps working
//!    indefinitely, where every table has a fixed end date.
//! 3. **The checked-in tables**, in the order [`ReplayConfig::identity_order`] names,
//!    taking the first that covers the instant wanted.
//!
//! Across identities, revocation outranks that order. Every candidate must be inside
//! its window — outside it, nothing works at all — and among those,
//! [`Resolver::resolve_preferring_unrevoked`] takes one the CRL does not name before
//! one it does:
//!
//! | standing | outcome |
//! |---|---|
//! | valid, unrevoked | chosen first; the CRL is attached and every sender works |
//! | valid, revoked | used only if nothing better exists; the CRL is withheld, so Chrome works and Chromium-based senders do not |
//! | outside its window | never used |
//!
//! The credential and the verdict about it leave here together, as one
//! [`ReceiverAuth`]. They are not independently gettable, because a CRL is only safe
//! beside the chain it was checked against.
//!
//! A failed fetch is not fatal at any point — it falls through to the tables and
//! is retried after [`ReplayConfig::failure_backoff`], matching the reference
//! client's 360-second hold-down. An unattended panel that loses its uplink keeps
//! authenticating.
//!
//! ## Why two offline identities, and what the order is really for
//!
//! The default order is [`Identity::Cks`] then
//! [`Identity::AirServer`], and on *expiry* grounds that second entry is
//! nearly vacuous: CKS covers through 2027-12-06 and AirServer stops 2027-03-21,
//! so any instant AirServer can serve, CKS can serve too. It earns its place in
//! two situations that expiry does not describe:
//!
//! * **Revocation.** D41's unmitigated risk is Google revoking the AirReceiver
//!   identity, which would leave the panel completing TLS and failing auth with
//!   nothing in the logs to explain it. The AirServer identity is a different
//!   device under a different intermediate on a different branch of the Cast PKI
//!   (see [`crate::airserver`]), so a revocation is survivable rather than fatal.
//!
//!   This used to say that nothing here could *detect* a revocation — that the only
//!   signal was the sender refusing us, and so the order had to be operator-set policy.
//!   Holding the CRL changed that: the document names revoked keys and serials outright,
//!   so the switch now happens on its own and `identity_order` only breaks ties between
//!   identities of equal standing.
//! * **A broken fixture set.** If one table fails to load, the other still
//!   resolves, and startup does not fail.
//!
//! The order is a declarative list rather than a chain of fallback flags so that
//! "which identity is this panel presenting" has one answer, readable from config,
//! instead of being reconstructed from which branches happened to be taken.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use crate::airserver::AirServerTable;
use crate::airserver_db::{AirServerDb, Kek};
use crate::cks::CksTable;
use crate::crl::{CastCrl, ServableCrl};
use crate::{airserver_api, api, cache, crl, CastCredential, ReplayError};

/// The two roots the reference client pins in place of the system trust store.
/// `cast.remotetogo.com` is CloudFront-fronted, so this is the Starfield /
/// Amazon Root CA 1 chain.
const PINNED_ROOTS_PEM: &str = include_str!("../fixtures/pinned_roots.pem");

/// How long to wait before retrying the backend after a failure. The reference
/// client uses 360 seconds; there is no reason to be more eager against someone
/// else's endpoint.
pub const DEFAULT_FAILURE_BACKOFF: Duration = Duration::from_secs(360);

/// Request timeout. The reference client passes `CURLOPT_TIMEOUT = 30`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long before a window ends to start trying to replace the credential, so a
/// roll does not land mid-session.
const REFRESH_LEAD: Duration = Duration::from_secs(3600);

/// Cap on the CKS response body. A live response is a few kilobytes. (AirServer's
/// response is ~14 MB and has its own, much larger cap — see
/// [`crate::airserver_db::MAX_DB_BYTES`].)
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

/// How close to a fetched database's end to start trying to replace it.
///
/// AirServer's live set spans about a month of 2-day windows, so three days of
/// headroom means several refresh attempts — each separated by
/// [`ReplayConfig::failure_backoff`] — before the old set stops working. The point is
/// that a panel offline for a fortnight still rolls over the moment its uplink
/// returns, rather than discovering the problem when the last window lapses.
const REFRESH_HORIZON: i64 = 3 * 24 * 3600;

/// A checked-in offline receiver-auth identity.
///
/// Named rather than numbered so a config file and a log line say the same word,
/// and so adding a third identity is a variant the compiler makes you handle
/// everywhere it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Identity {
    /// SoftMedia / AirReceiver, `CN=RYW0O FA8FCA6AC5A0` under `Eureka Gen1 ICA`.
    /// Covers 2023-01-01 → 2027-12-06. See [`crate::cks`].
    Cks,
    /// App Dynamic / AirServer, `CN=2001805200936810051` under
    /// `Widevine Cast Subroot`. Covers 2024-03-20 → 2027-03-21. See
    /// [`crate::airserver`].
    AirServer,
}

impl core::fmt::Display for Identity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Cks => f.write_str("cks"),
            Self::AirServer => f.write_str("airserver"),
        }
    }
}

/// How the provider is allowed to obtain credentials.
#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// Whether the backend may be contacted at all. With this off the provider is
    /// entirely offline and bounded by whichever table `identity_order` selects.
    pub network: bool,
    /// Where to cache a fetched credential. `None` disables caching.
    pub cache_path: Option<PathBuf>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Hold-down after a failed fetch.
    pub failure_backoff: Duration,
    /// Which identities to try, in order. Each is tried cache → live → table before
    /// the next is considered.
    ///
    /// Empty means no Cast credential at all, which is a legitimate choice for an
    /// operator who would rather Cast fail loudly than present a borrowed identity.
    pub identity_order: Vec<Identity>,
    /// Where to keep AirServer's fetched credential database. `None` disables that
    /// identity's live path, leaving it on its bundled table.
    pub airserver_db_path: Option<PathBuf>,
    /// The constants that open an AirServer database.
    ///
    /// Defaults to whatever the build was given ([`Kek::provisioned`]), which is `None`
    /// on a build that never saw the installer — so this identity's live path is simply
    /// unavailable there, and says so rather than pretending. Overridable so the tests
    /// can drive the whole path against fixtures keyed under a constant of our own.
    pub airserver_kek: Option<Kek<'static>>,
    /// Where to cache the fetched Cast CRL. `None` disables the CRL entirely, which
    /// leaves `AuthResponse.crl` empty — fine for Chrome, fatal for Chromium-based
    /// senders (see [`crate::crl`]).
    pub crl_cache_path: Option<PathBuf>,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            network: true,
            cache_path: Some(cache::default_path()),
            timeout: DEFAULT_TIMEOUT,
            failure_backoff: DEFAULT_FAILURE_BACKOFF,
            // CKS first: its *table* outlasts AirServer's by eight months, so on
            // expiry alone it is the better default. See the module docs for why
            // AirServer is carried at all.
            identity_order: vec![Identity::Cks, Identity::AirServer],
            airserver_db_path: Some(airserver_api::default_db_path()),
            airserver_kek: Kek::provisioned(),
            crl_cache_path: Some(crl::default_cache_path()),
        }
    }
}

/// The credential this receiver presents, together with the CRL it may attach.
///
/// One value rather than two gettable independently, because they are not independent:
/// a CRL is only safe to attach to the chain it was checked against, and attaching one
/// that revokes *us* turns a receiver that works in Chrome into one that works nowhere
/// (see [`crate::crl`]). Handing a caller a credential and a CRL separately makes
/// "check whether this document rejects this chain" something each caller has to
/// remember; handing it this makes the check unskippable, because the only way to get a
/// [`ServableCrl`] out of the provider is to get it from the credential it belongs to.
///
/// The same reason `proto_cast::CastIdentity` returns its acceptor and responder
/// together.
#[derive(Debug)]
pub struct ReceiverAuth {
    credential: Arc<CastCredential>,
    crl: Option<ServableCrl>,
}

impl ReceiverAuth {
    /// The credential: certificate chain, peer key, and precomputed signatures.
    #[must_use]
    pub fn credential(&self) -> &Arc<CastCredential> {
        &self.credential
    }

    /// The CRL to put in `AuthResponse.crl`, if one is held, is in date, and does not
    /// revoke [`Self::credential`]. `None` in every other case, which is the state this
    /// receiver had before it fetched CRLs at all.
    #[must_use]
    pub fn crl(&self) -> Option<&ServableCrl> {
        self.crl.as_ref()
    }
}

/// Pair a credential with the CRL, if that CRL is safe to serve alongside it.
///
/// The single place the decision is made, so the reasons for withholding are stated
/// once. A CRL naming our own chain is logged at `error`: it is the only notice this
/// receiver ever gets that its identity has been revoked (D41).
fn pair_with_crl(credential: CastCredential, crl: Option<&CastCrl>, now: i64) -> ReceiverAuth {
    let credential = Arc::new(credential);
    let servable = crl.and_then(|crl| {
        let mut chain: Vec<&[u8]> = vec![credential.device_cert_der()];
        chain.extend(credential.intermediates_der().iter().map(Vec::as_slice));
        // What we present stops one below the root; the sender checks the anchor too.
        let chain = crate::roots::with_anchor(&chain);
        match crl.servable_for(&chain, now) {
            Ok(Ok(servable)) => Some(servable),
            Ok(Err(crate::ServeRefusal::OutsideWindow)) => {
                debug!("holding back a Cast CRL that is outside its validity window");
                None
            }
            Ok(Err(refusal)) => {
                tracing::error!(
                    origin = %credential.origin(),
                    reason = %refusal,
                    "the published Cast CRL revokes the identity this receiver presents; \
                     withholding it, which keeps Chrome working and leaves Chromium-based \
                     senders refusing us. This identity needs replacing (D41)."
                );
                None
            }
            Err(e) => {
                warn!(error = %e, "could not evaluate the Cast CRL against our chain");
                None
            }
        }
    });
    ReceiverAuth {
        credential,
        crl: servable,
    }
}

/// Everything needed to run the fallback chain, minus the credential it produces.
///
/// Split out so [`ReplayProvider`] can be built with a real credential already in
/// hand. The alternative — construct the provider around a placeholder and
/// overwrite it — makes "no credential yet" a representable state on the
/// connection path, which is exactly the state that must not exist.
struct Resolver {
    /// The offline tables, each `None` if its fixtures failed to load. A broken
    /// fixture set degrades that one identity rather than failing startup, which
    /// is half the point of carrying two.
    cks_table: Option<CksTable>,
    airserver_table: Option<AirServerTable>,
    /// The last fetched AirServer database, memoised: opening one decrypts every
    /// window, and nothing about it changes between fetches.
    airserver_cache: RwLock<Option<Arc<AirServerDb>>>,
    config: ReplayConfig,
    /// `backend_now - local_now`, learned from a successful fetch.
    ///
    /// A panel whose clock is wrong picks the wrong window and every sender
    /// rejects it. The reference client keeps the same correction for the same
    /// reason; it is why the schedule does not follow the device clock.
    clock_offset: AtomicI64,
    /// When each live source may next be tried, as a local Unix second. Separate
    /// per source: one endpoint being down says nothing about the other, and sharing
    /// a hold-down would let a dead CKS backend suppress AirServer refreshes.
    cks_retry_after: AtomicI64,
    airserver_retry_after: AtomicI64,
}

/// Supplies the current Cast receiver-auth credential.
///
/// Reads are cheap and synchronous ([`Self::current_at`]) because they sit on the
/// per-connection path; refreshing happens in [`Self::run`]. There is always a
/// credential: one is resolved before the provider exists.
pub struct ReplayProvider {
    resolver: Resolver,
    current: RwLock<Arc<ReceiverAuth>>,
    /// The device CRL to attach to a challenge response, when one is held and safe to
    /// serve. Separate from the credential because it is a different document on a
    /// different schedule — about seven days against the credential's two.
    crl: RwLock<Option<Arc<CastCrl>>>,
    /// When the CRL is next due to be refetched, in Unix seconds. Drives its share of
    /// `next_wakeup`, so the loop wakes for whichever of the two documents is due first.
    crl_next_attempt: AtomicI64,
}

impl ReplayProvider {
    /// Resolve a credential and build the provider.
    ///
    /// Fails only if *no* path yields a credential — which, inside the table's
    /// range, means the embedded fixtures are broken. Startup failing loudly here
    /// is deliberate: a receiver that comes up without a credential looks healthy
    /// and rejects every sender.
    ///
    /// # Errors
    /// [`ReplayError`] if neither the cache, the backend nor the table produces a
    /// credential.
    pub async fn resolve(config: ReplayConfig) -> Result<Self, ReplayError> {
        let resolver = Resolver {
            cks_table: log_load("cks", CksTable::load()),
            airserver_table: log_load("airserver", AirServerTable::load()),
            airserver_cache: RwLock::new(None),
            config,
            clock_offset: AtomicI64::new(0),
            cks_retry_after: AtomicI64::new(0),
            airserver_retry_after: AtomicI64::new(0),
        };
        // The CRL is loaded before the credential, not after: which identity we should
        // present depends on which ones it revokes.
        let crl = load_crl(&resolver.config).await;
        let credential = resolver
            .resolve_preferring_unrevoked(local_now(), crl.as_ref())
            .await?;
        info!(
            origin = %credential.origin(),
            window_start = credential.window().start_unix(),
            window_end = credential.window().end_unix(),
            "Cast receiver-auth credential resolved"
        );
        // A failed initial load retries on the ordinary failure backoff rather than
        // waiting out a full day: coming up without a CRL costs Chromium-based senders.
        let next_attempt = local_now()
            + if crl.is_some() {
                CRL_REFRESH_INTERVAL_SECS
            } else {
                i64::try_from(resolver.config.failure_backoff.as_secs()).unwrap_or(360)
            };
        let now = local_now();
        Ok(Self {
            current: RwLock::new(Arc::new(pair_with_crl(credential, crl.as_ref(), now))),
            resolver,
            crl: RwLock::new(crl.map(Arc::new)),
            crl_next_attempt: AtomicI64::new(next_attempt),
        })
    }

    /// The raw CRL held, for re-pairing when the credential changes. Private: a caller
    /// outside must go through [`ReceiverAuth`], which cannot hand out a CRL that was
    /// not checked against the credential beside it.
    fn current_crl(&self) -> Option<Arc<CastCrl>> {
        self.crl.read().ok().and_then(|c| c.clone())
    }

    /// The credential to use for a connection at `now_unix`.
    ///
    /// If the held credential has aged out — the window rolled and the background
    /// refresh has not caught up, or is wedged — this re-issues from the table
    /// inline rather than handing back a credential every sender will reject.
    /// That costs one RSA signature, once per window.
    #[must_use]
    pub fn current_at(&self, now_unix: i64) -> Arc<ReceiverAuth> {
        let now = now_unix + self.resolver.clock_offset.load(Ordering::Relaxed);
        if let Ok(current) = self.current.read() {
            if current.credential().valid_at(now) {
                return Arc::clone(&current);
            }
        }
        match self.resolver.offline_credential(now) {
            Ok(fresh) => {
                // Re-paired, not carried over: the CRL verdict belongs to the chain it
                // was checked against, and this is a different one.
                let fresh = Arc::new(pair_with_crl(
                    fresh,
                    self.current_crl().as_deref(),
                    now_unix,
                ));
                warn!(
                    origin = %fresh.credential().origin(),
                    "the Cast credential aged out; re-issued from a checked-in table"
                );
                if let Ok(mut slot) = self.current.write() {
                    *slot = Arc::clone(&fresh);
                }
                fresh
            }
            Err(e) => {
                // Nothing better to offer. Hand back what we have and let the
                // sender's rejection be the visible symptom, with this in the log.
                warn!(error = %e, "no valid Cast credential for this connection");
                self.current
                    .read()
                    .map_or_else(|p| Arc::clone(&p.into_inner()), |c| Arc::clone(&c))
            }
        }
    }

    /// The credential for the current instant, and the CRL that may go with it.
    #[must_use]
    pub fn current(&self) -> Arc<ReceiverAuth> {
        self.current_at(local_now())
    }

    /// Refresh in the background until cancelled.
    ///
    /// Wakes shortly before the current window ends, and — when running on the
    /// table with the network enabled — retries the backend after the failure
    /// backoff, so a panel that boots offline upgrades itself once the uplink
    /// returns rather than staying on the table until reboot.
    pub async fn run(self: Arc<Self>) {
        loop {
            let delay = self.next_wakeup();
            debug!(seconds = delay.as_secs(), "next Cast credential refresh");
            tokio::time::sleep(delay).await;

            // The CRL first, so the credential is chosen against current revocation
            // data: an identity revoked since the last tick is abandoned here rather
            // than kept and quietly stripped of its CRL.
            self.refresh_crl().await;
            let crl = self.current_crl();

            match self
                .resolver
                .resolve_preferring_unrevoked(local_now(), crl.as_deref())
                .await
            {
                Ok(fresh) => {
                    let changed = self.current.read().is_ok_and(|c| {
                        c.credential().window() != fresh.window()
                            || c.credential().origin() != fresh.origin()
                    });
                    if changed {
                        info!(
                            origin = %fresh.origin(),
                            window_end = fresh.window().end_unix(),
                            "Cast receiver-auth credential refreshed"
                        );
                    }
                    // Re-paired every time, so a CRL refreshed on this same tick is
                    // re-evaluated against whatever chain we just settled on.
                    let paired = pair_with_crl(fresh, crl.as_deref(), local_now());
                    if let Ok(mut slot) = self.current.write() {
                        *slot = Arc::new(paired);
                    }
                }
                Err(e) => warn!(error = %e, "refreshing the Cast credential failed"),
            }
        }
    }

    /// Refetch the CRL, unconditionally, when its own schedule says so.
    ///
    /// No "is it nearly expired yet" test: the document is cheap, the schedule is
    /// already daily, and a conditional refresh is how a fetch gets skipped at 25 hours
    /// remaining and not retried until after the CRL has lapsed.
    async fn refresh_crl(&self) {
        let now = local_now();
        if now < self.crl_next_attempt.load(Ordering::Relaxed) {
            return;
        }
        let held_until = self.current_crl().map(|c| c.window().end_unix());

        match fetch_crl(&self.resolver.config).await {
            Some(fresh) => {
                let end = fresh.window().end_unix();
                if held_until != Some(end) {
                    info!(window_end = end, "Cast device CRL refreshed");
                }
                if let Ok(mut slot) = self.crl.write() {
                    *slot = Some(Arc::new(fresh));
                }
                self.crl_next_attempt
                    .store(now + CRL_REFRESH_INTERVAL_SECS, Ordering::Relaxed);
            }
            None => {
                // Keep serving the CRL we hold — `servable_for` withholds it the moment
                // it falls out of its window, so a failed refresh degrades to "no CRL"
                // rather than to "a CRL a sender hard-fails on".
                let backoff =
                    i64::try_from(self.resolver.config.failure_backoff.as_secs()).unwrap_or(360);
                self.crl_next_attempt
                    .store(now + backoff, Ordering::Relaxed);
            }
        }
    }

    /// How long to wait before the next refresh attempt.
    fn next_wakeup(&self) -> Duration {
        let now = self.corrected_now();
        let current = self.current.read().ok().map(|c| Arc::clone(&c));
        let window_end = current
            .as_ref()
            .map_or(now, |c| c.credential().window().end_unix());
        let on_table = current
            .as_ref()
            .is_some_and(|c| c.credential().origin().is_offline_table());

        // Running on the table with a reachable backend is a state worth leaving:
        // the table expires, the backend does not.
        let target = if on_table && self.resolver.config.network {
            now + i64::try_from(self.resolver.config.failure_backoff.as_secs()).unwrap_or(360)
        } else {
            window_end - i64::try_from(REFRESH_LEAD.as_secs()).unwrap_or(3600)
        };
        // The CRL is on its own clock, so the loop sleeps until whichever of the two is
        // due first. Without this the CRL would inherit the credential's schedule, which
        // on a fresh credential is most of two days.
        let target = target.min(self.crl_next_attempt.load(Ordering::Relaxed));
        Duration::from_secs(u64::try_from(target - now).unwrap_or(0).max(60))
    }

    fn corrected_now(&self) -> i64 {
        local_now() + self.resolver.clock_offset.load(Ordering::Relaxed)
    }
}

impl Resolver {
    /// Run the chain once: each identity in order, and within an identity
    /// cache → live → checked-in table.
    ///
    /// Returns the *last* failure rather than the first, because the last one comes
    /// from the least-preferred identity and so describes the widest gap — for a
    /// panel past every horizon, that is the message an operator needs.
    /// Resolve without consulting a CRL. Only the tests want this now — production
    /// always ranks against the CRL, and a path that quietly skips that check is
    /// exactly the thing [`ReceiverAuth`] exists to prevent.
    #[cfg(test)]
    async fn resolve_once(&self, now_local: i64) -> Result<CastCredential, ReplayError> {
        self.resolve_pass(now_local, None).await
    }

    /// Resolve, preferring an identity the CRL does not revoke.
    ///
    /// Revocation is a *ranking* input, not a veto, and the order it imposes is:
    ///
    /// 1. **valid and unrevoked** — works everywhere, CRL attached;
    /// 2. **valid but revoked** — the CRL is withheld, so it works in Chrome and not in
    ///    a Chromium-based sender. Strictly better than nothing, so it is used;
    /// 3. **outside its window** — never usable, and already excluded before this.
    ///
    /// Two passes rather than a score, because the fallback has to be able to reach a
    /// credential the first pass rejected: an all-revoked LAN must still come up.
    ///
    /// This is what [`crate::provider`]'s module docs said could not be done — "nothing
    /// here can *detect* a revocation, which is exactly why the order is operator-set
    /// policy". Holding the CRL is what changed that. `identity_order` still decides
    /// between identities of equal standing; it just no longer has to be edited by hand
    /// on the day one of them is revoked.
    async fn resolve_preferring_unrevoked(
        &self,
        now_local: i64,
        crl: Option<&CastCrl>,
    ) -> Result<CastCredential, ReplayError> {
        if let Some(crl) = crl {
            match self.resolve_pass(now_local, Some(crl)).await {
                Ok(credential) => return Ok(credential),
                // Deliberately does not name revocation as the cause: this pass also
                // comes back empty when no identity is configured, or when every table
                // has run out of calendar. When revocation *is* the reason,
                // `resolve_pass` has already said so per identity, at `warn`.
                Err(e) => debug!(
                    error = %e,
                    "no identity passed the revocation filter; retrying without it"
                ),
            }
        }
        self.resolve_pass(now_local, None).await
    }

    /// One pass over `identity_order`. With `avoid` set, a credential the CRL revokes is
    /// skipped as though that identity could not supply one.
    async fn resolve_pass(
        &self,
        now_local: i64,
        avoid: Option<&CastCrl>,
    ) -> Result<CastCredential, ReplayError> {
        let now = now_local + self.clock_offset.load(Ordering::Relaxed);
        let mut last = None;
        for identity in &self.config.identity_order {
            let attempt = match identity {
                Identity::Cks => self.resolve_cks(now_local, now).await,
                Identity::AirServer => self.resolve_airserver(now_local, now).await,
            };
            match attempt {
                Ok(credential) => {
                    if let Some(crl) = avoid {
                        if let Some(reason) = revoked_by(crl, &credential) {
                            warn!(
                                %identity,
                                %reason,
                                "the published Cast CRL revokes this identity; trying the next one"
                            );
                            last = Some(ReplayError::Table(format!(
                                "the {identity} identity is revoked"
                            )));
                            continue;
                        }
                    }
                    return Ok(credential);
                }
                Err(e) => {
                    debug!(%identity, error = %e, "identity cannot supply a credential");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| ReplayError::Table("no Cast identity is configured".into())))
    }

    /// CKS: the JSON credential cache, then the backend, then the table.
    async fn resolve_cks(&self, now_local: i64, now: i64) -> Result<CastCredential, ReplayError> {
        if let Some(path) = &self.config.cache_path {
            // `std::fs::read` plus a JSON parse and a PEM decode — small, but filesystem
            // work on a runtime worker all the same (ground rule 4).
            let target = path.clone();
            let loaded = tokio::task::spawn_blocking(move || cache::load(&target))
                .await
                .unwrap_or_else(|e| {
                    Err(ReplayError::Cache(format!("joining the cache read: {e}")))
                });
            match loaded {
                Ok(Some(cached)) if cached.valid_at(now) => {
                    debug!("using the cached CKS credential");
                    return Ok(cached);
                }
                Ok(Some(_)) => debug!("the cached CKS credential is outside its window"),
                Ok(None) => {}
                // A cache that will not load is a real fault, but not one worth
                // failing over — the remaining paths do not depend on it.
                Err(e) => warn!(error = %e, "ignoring the CKS credential cache"),
            }
        }

        if self.may_try(&self.cks_retry_after, now_local) {
            match self.fetch(now_local).await {
                Ok(credential) => {
                    if let Some(path) = &self.config.cache_path {
                        let target = path.clone();
                        let stored = credential.clone();
                        let written =
                            tokio::task::spawn_blocking(move || cache::store(&target, &stored))
                                .await;
                        match written {
                            Ok(Err(e)) => warn!(error = %e, "could not cache the CKS credential"),
                            Err(e) => warn!(error = %e, "joining the cache write"),
                            Ok(Ok(())) => {}
                        }
                    }
                    return Ok(credential);
                }
                Err(e) => self.back_off(&self.cks_retry_after, now_local, "CKS backend", &e),
            }
        }

        self.table_credential(Identity::Cks, now)
    }

    /// AirServer: the cached database, then a fetch, then the bundled table.
    ///
    /// The cache and the live source are the *same artifact* here — one database
    /// covering ~30 rolling windows — which is what makes rollover cheap: a fetch
    /// buys a month of windows rather than one, and between fetches the cached file
    /// answers with no network at all.
    ///
    /// The ordering that matters: a cached database still inside
    /// [`REFRESH_HORIZON`] of its end triggers a fetch *before* being used, so the
    /// set is replaced while the old one still works rather than after it stops. If
    /// that fetch fails, the old database is still used — an expiring credential
    /// beats none.
    async fn resolve_airserver(
        &self,
        now_local: i64,
        now: i64,
    ) -> Result<CastCredential, ReplayError> {
        let cached = self.airserver_db(now).await;
        let horizon_near = cached
            .as_ref()
            .is_some_and(|db| db.covers_until() - now < REFRESH_HORIZON);
        let usable = cached
            .as_ref()
            .is_some_and(|db| db.credential_at(now).is_ok());

        if (!usable || horizon_near) && self.may_try(&self.airserver_retry_after, now_local) {
            match self.fetch_airserver_db().await {
                Ok(db) => {
                    if let Ok(credential) = db.credential_at(now) {
                        info!(
                            windows = db.window_count(),
                            covers_until = db.covers_until(),
                            generated = db.generated_unix(),
                            "fetched a fresh AirServer credential database"
                        );
                        if let Ok(mut slot) = self.airserver_cache.write() {
                            *slot = Some(Arc::new(db));
                        }
                        return Ok(credential);
                    }
                    warn!("the fetched AirServer database does not cover the current window");
                }
                Err(e) => {
                    self.back_off(
                        &self.airserver_retry_after,
                        now_local,
                        "AirServer endpoint",
                        &e,
                    );
                }
            }
        }

        if let Some(db) = cached {
            if let Ok(credential) = db.credential_at(now) {
                if horizon_near {
                    warn!(
                        covers_until = db.covers_until(),
                        "the cached AirServer database is close to its end and could not be \
                         refreshed"
                    );
                }
                return Ok(credential);
            }
        }

        self.table_credential(Identity::AirServer, now)
    }

    /// The cached AirServer database: the one held in memory, else the file on disk.
    ///
    /// Opening decrypts every window, so the result is memoised — `resolve_once` runs
    /// on a timer, but there is no reason to pay for it on every wakeup.
    ///
    /// Off the runtime, like [`Self::fetch_airserver_db`] twenty lines below, which
    /// spawn_blocks the identical `AirServerDb::open` under a comment citing ground rule
    /// 4. The two used to disagree. It is a SQLite open, a BLAKE2b KDF, three secretbox
    /// opens per window and an RSA re-encode, plus a `stat` — and on the Windows box a
    /// cold read of a 14 MB file behind a real-time scanner is the case that hurts.
    async fn airserver_db(&self, _now: i64) -> Option<Arc<AirServerDb>> {
        if let Ok(slot) = self.airserver_cache.read() {
            if let Some(db) = slot.as_ref() {
                return Some(Arc::clone(db));
            }
        }
        let path = self.config.airserver_db_path.clone()?;
        let opened = path.clone();
        let kek = self.config.airserver_kek?;
        let found = tokio::task::spawn_blocking(move || {
            opened
                .exists()
                .then(|| AirServerDb::open_with_kek(&opened, kek))
        })
        .await;
        let found = match found {
            Ok(found) => found?,
            Err(e) => {
                warn!(error = %e, "joining the AirServer database open");
                return None;
            }
        };
        match found {
            Ok(db) => {
                let db = Arc::new(db);
                if let Ok(mut slot) = self.airserver_cache.write() {
                    *slot = Some(Arc::clone(&db));
                }
                Some(db)
            }
            // A database that will not open is worth saying loudly — it means this
            // identity is running on its bundled table instead — but not worth
            // failing over.
            Err(e) => {
                warn!(error = %e, path = %path.display(), "ignoring the cached AirServer database");
                None
            }
        }
    }

    /// One POST to AirServer's endpoint, written to the cache path and reopened.
    async fn fetch_airserver_db(&self) -> Result<AirServerDb, ReplayError> {
        let path = self
            .config
            .airserver_db_path
            .clone()
            .ok_or_else(|| ReplayError::Cache("no AirServer database path is set".into()))?;
        let timeout = self.config.timeout;
        // ureq is blocking and the body is ~14 MB, so this belongs off the runtime
        // (ground rule 4).
        let target = path.clone();
        let bytes = tokio::task::spawn_blocking(move || airserver_api::fetch_to(&target, timeout))
            .await
            .map_err(|e| ReplayError::Http(format!("joining the AirServer request: {e}")))??;
        debug!(bytes, path = %path.display(), "wrote an AirServer credential database");

        let kek = self.config.airserver_kek.ok_or(ReplayError::NoKek)?;
        let opened = path.clone();
        tokio::task::spawn_blocking(move || AirServerDb::open_with_kek(&opened, kek))
            .await
            .map_err(|e| ReplayError::Database(format!("joining the database open: {e}")))?
    }

    /// Whether a live source may be tried, given its hold-down.
    fn may_try(&self, retry_after: &AtomicI64, now_local: i64) -> bool {
        self.config.network && now_local >= retry_after.load(Ordering::Relaxed)
    }

    /// Record a failure and hold the source down for the configured backoff.
    fn back_off(&self, retry_after: &AtomicI64, now_local: i64, what: &str, e: &ReplayError) {
        let until = now_local + i64::try_from(self.config.failure_backoff.as_secs()).unwrap_or(360);
        retry_after.store(until, Ordering::Relaxed);
        warn!(
            error = %e,
            backoff_secs = self.config.failure_backoff.as_secs(),
            "the {what} is unreachable; falling back",
        );
    }

    /// One identity's checked-in table. Cheap: no network, no database, no decryption.
    ///
    /// This is what the per-connection path falls back to, so it must stay cheap.
    fn table_credential(
        &self,
        identity: Identity,
        now: i64,
    ) -> Result<CastCredential, ReplayError> {
        match identity {
            Identity::Cks => self.cks_table.as_ref().map_or_else(
                || Err(ReplayError::Table("the CKS fixtures did not load".into())),
                |t| t.credential_at(now),
            ),
            Identity::AirServer => self.airserver_table.as_ref().map_or_else(
                || {
                    Err(ReplayError::Table(
                        "the AirServer fixtures did not load".into(),
                    ))
                },
                |t| t.credential_at(now),
            ),
        }
    }

    /// The first checked-in table, in configured order, that covers `now`.
    fn offline_credential(&self, now: i64) -> Result<CastCredential, ReplayError> {
        let mut last = None;
        for identity in &self.config.identity_order {
            match self.table_credential(*identity, now) {
                Ok(credential) => return Ok(credential),
                Err(e) => {
                    debug!(%identity, error = %e, "table cannot cover this instant");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            ReplayError::Table(
                "no offline identity is configured and no live source answered".into(),
            )
        }))
    }

    /// One request to the backend.
    async fn fetch(&self, ts: i64) -> Result<CastCredential, ReplayError> {
        let timeout = self.config.timeout;
        // ureq is blocking, so it does not belong on the runtime (ground rule 4).
        let body = tokio::task::spawn_blocking(move || fetch_blocking(ts, timeout))
            .await
            .map_err(|e| ReplayError::Http(format!("joining the CKS request: {e}")))??;

        let response = api::decode_response(&body)?;
        // Learn the clock correction before the window check below depends on it.
        let offset = response.now - ts;
        if offset.abs() > 60 {
            info!(
                offset_secs = offset,
                "correcting for local clock skew from the CKS backend"
            );
        }
        self.clock_offset.store(offset, Ordering::Relaxed);

        let credential = response.into_credential()?;
        let now = ts + offset;
        if !credential.valid_at(now) {
            return Err(ReplayError::Response(format!(
                "the backend returned a credential valid {}..{}, which does not cover {now}",
                credential.window().start_unix(),
                credential.window().end_unix()
            )));
        }
        Ok(credential)
    }
}

/// Load one offline table, reporting rather than propagating a failure.
///
/// A table whose fixtures are broken is a build-time defect, but it must not take
/// the receiver down when another identity would have worked — so it is logged at
/// `error` (loud, because it means the panel is running on fewer identities than it
/// was built with) and the slot is left empty.
fn log_load<T>(identity: &str, loaded: Result<T, ReplayError>) -> Option<T> {
    match loaded {
        Ok(table) => Some(table),
        Err(e) => {
            tracing::error!(
                identity,
                error = %e,
                "an offline Cast identity failed to load; continuing without it"
            );
            None
        }
    }
}

/// How often the CRL is refetched while the panel runs.
///
/// Daily, on its own clock, and deliberately not derived from the credential's: those
/// windows are two days where the CRL's is about seven, and hanging one off the other
/// means the CRL's schedule is set by something unrelated to it. `next_wakeup` sleeps
/// until the credential's window is nearly out, which on a fresh credential is most of
/// two days — long enough for a check to land at "25 hours left", skip, and not come
/// back until well after the CRL has expired. A sender *hard-fails* on a stale CRL, so
/// that is a worse state than never having fetched one.
///
/// Daily against a seven-day document also means six consecutive fetch failures before
/// anything degrades.
const CRL_REFRESH_INTERVAL_SECS: i64 = 86_400;

/// How long a cached CRL must have left for startup to skip the network entirely.
const CRL_CACHE_FRESH_SECS: i64 = 86_400;

/// Load the Cast CRL: cache first, then the network.
///
/// Best-effort by design and never fatal. Without a CRL this receiver still
/// authenticates to Chrome (its fallback carries us); what is lost is Chromium-based
/// senders. Failing startup over that would trade a partial receiver for none.
async fn load_crl(config: &ReplayConfig) -> Option<CastCrl> {
    let path = config.crl_cache_path.clone()?;
    let now = local_now();

    let target = path.clone();
    let read = tokio::task::spawn_blocking(move || crl::read_cache(&target))
        .await
        .unwrap_or_else(|e| {
            Err(ReplayError::Cache(format!(
                "joining the CRL cache read: {e}"
            )))
        });
    let cached = match read {
        Ok(cached) => cached,
        Err(e) => {
            warn!(error = %e, "reading the cached Cast CRL failed");
            None
        }
    };
    // A cached CRL with room left is the whole answer; the endpoint is only consulted
    // when it cannot be.
    if let Some(crl) = &cached {
        if crl.window().end_unix() - now > CRL_CACHE_FRESH_SECS {
            return cached;
        }
    }
    fetch_crl(config).await.or(cached)
}

/// Fetch the CRL from the endpoint and cache it. `None` on any failure, which is never
/// fatal — see [`load_crl`].
async fn fetch_crl(config: &ReplayConfig) -> Option<CastCrl> {
    let path = config.crl_cache_path.clone()?;
    if !config.network {
        return None;
    }

    // ureq is blocking, so it does not belong on the runtime (ground rule 4).
    let timeout = config.timeout;
    let fetched = tokio::task::spawn_blocking(move || crl::fetch_blocking(timeout))
        .await
        .map_err(|e| ReplayError::Http(format!("joining the Cast CRL fetch: {e}")))
        .and_then(|r| r)
        .and_then(|raw| CastCrl::parse(&raw).map(|crl| (raw, crl)));

    match fetched {
        Ok((raw, fresh)) => {
            let target = path.clone();
            let written =
                tokio::task::spawn_blocking(move || crl::write_cache(&target, &raw)).await;
            match written {
                Ok(Err(e)) => warn!(error = %e, "caching the Cast CRL failed"),
                Err(e) => warn!(error = %e, "joining the CRL cache write"),
                Ok(Ok(())) => {}
            }
            Some(fresh)
        }
        Err(e) => {
            warn!(error = %e, "fetching the Cast CRL failed; Chromium-based senders will refuse this receiver");
            None
        }
    }
}

/// Whether `crl` revokes the chain `credential` presents, and why.
///
/// A chain that cannot be parsed is reported as *not* revoked: refusing an identity
/// because we could not read its certificates would turn a parsing bug into a dead
/// receiver, and the sender does its own check regardless.
fn revoked_by(crl: &CastCrl, credential: &CastCredential) -> Option<crl::Revocation> {
    let mut chain: Vec<&[u8]> = vec![credential.device_cert_der()];
    chain.extend(credential.intermediates_der().iter().map(Vec::as_slice));
    let chain = crate::roots::with_anchor(&chain);
    match crl.revokes(&chain) {
        Ok(found) => found,
        Err(e) => {
            warn!(error = %e, "could not evaluate the Cast CRL against a candidate identity");
            None
        }
    }
}

/// Perform the request. Blocking; called only from `spawn_blocking`.
fn fetch_blocking(ts: i64, timeout: Duration) -> Result<Vec<u8>, ReplayError> {
    use std::io::Read as _;

    let request = api::request(ts);
    let agent = ureq::builder()
        .timeout(timeout)
        .tls_config(Arc::new(pinned_tls_config()?))
        .build();

    let response = agent
        .get(&request.url)
        .set("User-Agent", request.user_agent)
        .set(request.api_key.0, request.api_key.1)
        .call()
        // `ureq::Error` is large; flatten it here rather than carry it outward.
        .map_err(|e| ReplayError::Http(e.to_string()))?;

    // Bounded: the body is a handful of certificates, and this is a third party's
    // endpoint — an unbounded read would let a bad day upstream exhaust the panel.
    let mut body = Vec::new();
    let mut reader = response.into_reader().take(MAX_RESPONSE_BYTES);
    std::io::Read::read_to_end(&mut reader, &mut body)
        .map_err(|e| ReplayError::Http(format!("reading the CKS response: {e}")))?;
    Ok(body)
}

/// A TLS client config trusting only the two pinned roots.
///
/// The system trust store is deliberately unused, matching the reference client
/// (`CURLOPT_CAINFO`/`CAPATH` both `NULL`, anchors injected through
/// `CURLOPT_SSL_CTX_FUNCTION`). It also means this path does not depend on the
/// Windows deploy target having a usable root store.
fn pinned_tls_config() -> Result<rustls::ClientConfig, ReplayError> {
    let mut roots = rustls::RootCertStore::empty();
    for der in crate::pem::decode_all(PINNED_ROOTS_PEM, "CERTIFICATE")? {
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .map_err(|e| ReplayError::Http(format!("pinned root is not usable: {e}")))?;
    }
    if roots.is_empty() {
        return Err(ReplayError::Http("no pinned roots were loaded".into()));
    }
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| ReplayError::Http(format!("building the CKS TLS config: {e}")))
        .map(|b| b.with_root_certificates(roots).with_no_client_auth())
}

/// The local clock as Unix seconds.
///
/// A clock set before 1970 reads as 0, which is before the table's epoch and so
/// produces [`ReplayError::OutOfRange`] — a typed failure rather than a confidently
/// wrong window.
fn local_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::CredentialOrigin;

    /// Inside the table's range, an offline provider always resolves.
    #[tokio::test]
    async fn resolves_offline_from_the_table() {
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            ..ReplayConfig::default()
        })
        .await
        .unwrap();

        let at = 1_785_196_800; // 2026-07-28, window 652
        let credential = provider.current_at(at);
        let credential = credential.credential();
        assert!(credential.valid_at(at));
        assert_eq!(
            credential.origin(),
            &CredentialOrigin::CksTable { index: 652 }
        );
    }

    /// Reversing the order is the whole mitigation for a revoked identity, so it
    /// has to actually change which identity is presented.
    #[tokio::test]
    async fn the_identity_order_selects_the_identity() {
        let at = 1_785_196_800; // 2026-07-28: inside both tables

        for (order, expect_airserver) in [
            (vec![Identity::Cks, Identity::AirServer], false),
            (vec![Identity::AirServer, Identity::Cks], true),
        ] {
            let provider = ReplayProvider::resolve(ReplayConfig {
                network: false,
                cache_path: None,
                identity_order: order.clone(),
                ..ReplayConfig::default()
            })
            .await
            .unwrap();

            let credential = provider.current_at(at);
            let credential = credential.credential();
            assert!(
                credential.valid_at(at),
                "order {order:?} produced a stale window"
            );
            assert_eq!(
                matches!(credential.origin(), CredentialOrigin::AirServerTable { .. }),
                expect_airserver,
                "order {order:?} picked {}",
                credential.origin()
            );
        }
    }

    /// Between AirServer's end (2027-03-21) and CKS's (2027-12-06) only one table
    /// can answer, so the preferred-but-exhausted identity must fall through rather
    /// than fail. This is the one case where the fallback is driven by expiry.
    #[tokio::test]
    async fn an_exhausted_preferred_identity_falls_through() {
        let at = 1_814_400_000; // 2027-07-01: past AirServer, inside CKS
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            identity_order: vec![Identity::AirServer, Identity::Cks],
            ..ReplayConfig::default()
        })
        .await
        .unwrap();

        let credential = provider.current_at(at);
        let credential = credential.credential();
        assert!(credential.valid_at(at));
        assert!(
            matches!(credential.origin(), CredentialOrigin::CksTable { .. }),
            "expected the CKS table to cover this instant, got {}",
            credential.origin()
        );
    }

    /// Past every table, *resolution* fails loudly instead of handing back something a
    /// sender will reject with no explanation.
    ///
    /// Loudly, but no longer fatally: the app logs this and starts without Cast rather
    /// than refusing to boot, because the panel's other six protocols do not depend on
    /// it. The distinction matters — this crate's job is to be honest that it has
    /// nothing to offer, and the caller's job is to decide what that costs.
    #[tokio::test]
    async fn past_every_table_resolution_fails() {
        let far_future_config = ReplayConfig {
            network: false,
            cache_path: None,
            identity_order: vec![Identity::AirServer],
            ..ReplayConfig::default()
        };
        // AirServer stops 2027-03-21; ask for 2027-07-01.
        let resolver = Resolver {
            cks_table: CksTable::load().ok(),
            airserver_table: AirServerTable::load().ok(),
            config: far_future_config,
            airserver_cache: RwLock::new(None),
            clock_offset: AtomicI64::new(0),
            cks_retry_after: AtomicI64::new(0),
            airserver_retry_after: AtomicI64::new(0),
        };
        assert!(matches!(
            resolver.offline_credential(1_814_400_000),
            Err(ReplayError::OutOfRange { .. })
        ));
    }

    /// An operator who would rather Cast fail than present a borrowed identity gets
    /// a clear error, not a panic and not a silent success.
    #[tokio::test]
    async fn no_offline_identity_is_a_named_error() {
        let resolver = Resolver {
            cks_table: CksTable::load().ok(),
            airserver_table: AirServerTable::load().ok(),
            config: ReplayConfig {
                network: false,
                cache_path: None,
                identity_order: vec![],
                ..ReplayConfig::default()
            },
            airserver_cache: RwLock::new(None),
            clock_offset: AtomicI64::new(0),
            cks_retry_after: AtomicI64::new(0),
            airserver_retry_after: AtomicI64::new(0),
        };
        assert!(matches!(
            resolver.offline_credential(1_785_196_800),
            Err(ReplayError::Table(_))
        ));
    }

    /// The trimmed AirServer database, three windows from 2024-03-20.
    const AIRSERVER_DB: &[u8] = include_bytes!("../fixtures/airserver/db_trimmed.sqlite");

    /// Inside the first window of the trimmed database.
    const IN_DB: i64 = 1_710_892_800 + 3600;

    fn with_database() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("airserver.sqlite");
        std::fs::write(&path, AIRSERVER_DB).unwrap();
        (dir, path)
    }

    /// A cached database answers with no network at all. This is the state a panel
    /// spends almost all its time in: one fetch bought ~30 windows.
    #[tokio::test]
    async fn a_cached_database_is_used_without_network() {
        let (_dir, path) = with_database();
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            identity_order: vec![Identity::AirServer],
            airserver_db_path: Some(path),
            airserver_kek: Some(crate::airserver_db::TEST_KEK),
            ..ReplayConfig::default()
        })
        .await
        .unwrap();

        let credential = provider.resolver.resolve_once(IN_DB).await.unwrap();
        assert_eq!(credential.origin(), &CredentialOrigin::AirServerLive);
        assert!(credential.valid_at(IN_DB));
    }

    /// A database inside REFRESH_HORIZON of its end wants replacing, but if the fetch
    /// cannot run it must still be used. An expiring credential beats none, and this
    /// is the path a panel with no uplink takes.
    #[tokio::test]
    async fn an_expiring_database_is_still_used_when_no_fetch_is_possible() {
        let (_dir, path) = with_database();
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            identity_order: vec![Identity::AirServer],
            airserver_db_path: Some(path),
            airserver_kek: Some(crate::airserver_db::TEST_KEK),
            ..ReplayConfig::default()
        })
        .await
        .unwrap();

        // The last window of the trimmed set ends 2024-03-25; an hour before that is
        // well inside the three-day horizon.
        let near_end = 1_711_065_600 + 2 * 86_400 - 3600;
        let credential = provider.resolver.resolve_once(near_end).await.unwrap();
        assert_eq!(credential.origin(), &CredentialOrigin::AirServerLive);
        assert!(credential.valid_at(near_end));
    }

    /// With no database path the identity is table-only, which is what an operator who
    /// does not want the endpoint contacted gets.
    #[tokio::test]
    async fn without_a_database_path_airserver_uses_its_table() {
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            identity_order: vec![Identity::AirServer],
            airserver_db_path: None,
            ..ReplayConfig::default()
        })
        .await
        .unwrap();

        let at = 1_785_196_800;
        let credential = provider.resolver.resolve_once(at).await.unwrap();
        assert!(matches!(
            credential.origin(),
            CredentialOrigin::AirServerTable { .. }
        ));
    }

    /// The two live sources hold down independently. Sharing one timer would let a
    /// dead CKS backend suppress AirServer refreshes for the whole backoff, which is
    /// exactly the coupling a second identity exists to avoid.
    #[tokio::test]
    async fn the_live_sources_back_off_independently() {
        let (_dir, path) = with_database();
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            identity_order: vec![Identity::Cks, Identity::AirServer],
            airserver_db_path: Some(path),
            airserver_kek: Some(crate::airserver_db::TEST_KEK),
            ..ReplayConfig::default()
        })
        .await
        .unwrap();

        provider
            .resolver
            .cks_retry_after
            .store(i64::MAX, Ordering::Relaxed);
        assert_eq!(
            provider
                .resolver
                .airserver_retry_after
                .load(Ordering::Relaxed),
            0,
            "holding CKS down must not touch AirServer"
        );

        // CKS is first in order and still answers from its table, so this asserts the
        // isolation of the timers rather than the ordering.
        assert!(provider.resolver.resolve_once(IN_DB).await.is_ok());
    }

    /// A window roll must produce a different credential, not a stale one.
    #[tokio::test]
    async fn a_window_roll_re_issues_inline() {
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            ..ReplayConfig::default()
        })
        .await
        .unwrap();

        let first = provider.current_at(1_785_196_800);
        let first = first.credential();
        let second = provider.current_at(1_785_196_800 + 172_800);
        let second = second.credential();
        assert_ne!(first.window(), second.window());
        assert_ne!(first.peer_cert_der(), second.peer_cert_der());
        assert!(second.valid_at(1_785_196_800 + 172_800));
    }

    /// Reads on the connection path must not hit the network or the disk.
    #[tokio::test]
    async fn repeated_reads_in_one_window_return_the_same_credential() {
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            ..ReplayConfig::default()
        })
        .await
        .unwrap();
        let a = provider.current_at(1_785_196_800);
        let b = provider.current_at(1_785_196_800 + 1000);
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn the_pinned_roots_load() {
        let config = pinned_tls_config();
        assert!(config.is_ok(), "{:?}", config.err());
    }

    /// With the network off, the backoff must never gate the table.
    #[tokio::test]
    async fn an_offline_provider_never_waits_on_a_backoff() {
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            ..ReplayConfig::default()
        })
        .await
        .unwrap();
        provider
            .resolver
            .cks_retry_after
            .store(i64::MAX, Ordering::Relaxed);
        let credential = provider.resolver.resolve_once(1_785_196_800).await.unwrap();
        assert!(credential.valid_at(1_785_196_800));
    }

    /// A clock so wrong that no window covers it must be a typed error, not a
    /// credential from the wrong window.
    #[tokio::test]
    async fn a_time_outside_the_table_is_an_error() {
        let provider = ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            ..ReplayConfig::default()
        })
        .await
        .unwrap();
        assert!(matches!(
            provider.resolver.resolve_once(0).await,
            Err(ReplayError::OutOfRange { .. })
        ));
    }

    /// An offline provider with no caches, so resolution comes from the tables alone
    /// and the identity chosen is decided purely by order and revocation.
    async fn offline_provider() -> ReplayProvider {
        ReplayProvider::resolve(ReplayConfig {
            network: false,
            cache_path: None,
            airserver_db_path: None,
            crl_cache_path: None,
            ..ReplayConfig::default()
        })
        .await
        .unwrap()
    }

    /// A revoked identity is stepped over, not merely stripped of its CRL.
    ///
    /// This is the ordering the whole arrangement exists to produce:
    ///
    /// 1. valid and unrevoked — attaches the CRL, works in every sender;
    /// 2. valid but revoked — CRL withheld, works in Chrome only;
    /// 3. outside its window — never used.
    ///
    /// Without the first pass a revoked CKS identity would simply be served with the
    /// CRL dropped, which needlessly costs every Chromium-based sender when a perfectly
    /// good AirServer identity is sitting behind it on a different branch of the PKI.
    #[tokio::test]
    async fn a_revoked_identity_is_stepped_over_for_an_unrevoked_one() {
        let provider = offline_provider().await;
        // Inside both tables *and* inside the fixture CRL's window (which opens at
        // 2026-07-28T20:24:28Z) — otherwise the CRL is withheld as out of date and the
        // assertion below would pass or fail for the wrong reason.
        let at = 1_785_300_000;

        // Baseline: with no CRL, the configured order puts CKS first.
        let plain = provider.resolver.resolve_once(at).await.unwrap();
        assert!(matches!(plain.origin(), CredentialOrigin::CksTable { .. }));

        // Now revoke exactly that identity.
        let crl = crate::CastCrl::parse(include_bytes!("../fixtures/cast-crl-latest.bin")).unwrap();
        let poisoned = crl.also_revoking(crate::CastCrl::spki_hash_of(plain.device_cert_der()));

        let chosen = provider
            .resolver
            .resolve_preferring_unrevoked(at, Some(&poisoned))
            .await
            .unwrap();
        assert!(
            matches!(chosen.origin(), CredentialOrigin::AirServerTable { .. }),
            "a revoked CKS identity must fall through to AirServer, got {}",
            chosen.origin()
        );
        // And the one it fell through to keeps its CRL.
        let paired = pair_with_crl(chosen, Some(&poisoned), at);
        assert!(paired.crl().is_some());
    }

    /// When every identity is revoked there is nothing to fall through to, so the
    /// configured order wins and the CRL is withheld — Chrome keeps working, which is
    /// strictly better than refusing to come up at all.
    #[tokio::test]
    async fn all_identities_revoked_still_yields_a_credential_with_no_crl() {
        let provider = offline_provider().await;
        let at = 1_785_300_000;

        let crl = crate::CastCrl::parse(include_bytes!("../fixtures/cast-crl-latest.bin")).unwrap();
        let cks = provider.resolver.resolve_once(at).await.unwrap();
        let air = crate::AirServerTable::load()
            .unwrap()
            .credential_at(at)
            .unwrap();
        let poisoned = crl
            .also_revoking(crate::CastCrl::spki_hash_of(cks.device_cert_der()))
            .also_revoking(crate::CastCrl::spki_hash_of(air.device_cert_der()));

        let chosen = provider
            .resolver
            .resolve_preferring_unrevoked(at, Some(&poisoned))
            .await
            .expect("a revoked credential still beats no credential");
        let paired = pair_with_crl(chosen, Some(&poisoned), at);
        assert!(
            paired.crl().is_none(),
            "a CRL that revokes the chain we present must never be attached to it"
        );
        assert!(paired.credential().valid_at(at));
    }
}
