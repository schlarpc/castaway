//! The credential provider: fallback order, disk cache, and background refresh.
//!
//! This is the only module here that touches a socket or the filesystem
//! (ground rule 3). Everything it decides is decided from [`crate::api`],
//! [`crate::cks`] and [`crate::cache`], which are pure.
//!
//! ## Resolution order
//!
//! 1. **The cache**, if it holds a credential still inside its window. A cached
//!    credential *is* a previously fetched one, so using it is not a downgrade —
//!    it just avoids spending a request per restart on material we already have.
//! 2. **The backend.** Preferred over any table because it keeps working
//!    indefinitely, where every table has a fixed end date.
//! 3. **The checked-in tables**, in the order [`ReplayConfig::identity_order`] names,
//!    taking the first that covers the instant wanted.
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
//!   (see [`crate::airserver`]), so reversing the order is a config change instead
//!   of a dead receiver. Nothing here can *detect* a revocation — that signal is
//!   the sender refusing us — which is exactly why the order is operator-set
//!   policy rather than something this module tries to infer.
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
use crate::airserver_db::AirServerDb;
use crate::cks::CksTable;
use crate::crl::CastCrl;
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
            crl_cache_path: Some(crl::default_cache_path()),
        }
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
    current: RwLock<Arc<CastCredential>>,
    /// The device CRL to attach to a challenge response, when one is held and safe to
    /// serve. Separate from the credential because it is a different document on a
    /// different schedule — about seven days against the credential's two.
    crl: RwLock<Option<Arc<CastCrl>>>,
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
        let credential = resolver.resolve_once(local_now()).await?;
        info!(
            origin = %credential.origin(),
            window_start = credential.window().start_unix(),
            window_end = credential.window().end_unix(),
            "Cast receiver-auth credential resolved"
        );
        let crl = load_crl(&resolver.config).await;
        Ok(Self {
            resolver,
            current: RwLock::new(Arc::new(credential)),
            crl: RwLock::new(crl.map(Arc::new)),
        })
    }

    /// The CRL held for this receiver, if any.
    #[must_use]
    pub fn current_crl(&self) -> Option<Arc<CastCrl>> {
        self.crl.read().ok().and_then(|c| c.clone())
    }

    /// The credential to use for a connection at `now_unix`.
    ///
    /// If the held credential has aged out — the window rolled and the background
    /// refresh has not caught up, or is wedged — this re-issues from the table
    /// inline rather than handing back a credential every sender will reject.
    /// That costs one RSA signature, once per window.
    #[must_use]
    pub fn current_at(&self, now_unix: i64) -> Arc<CastCredential> {
        let now = now_unix + self.resolver.clock_offset.load(Ordering::Relaxed);
        if let Ok(current) = self.current.read() {
            if current.valid_at(now) {
                return Arc::clone(&current);
            }
        }
        match self.resolver.offline_credential(now) {
            Ok(fresh) => {
                let fresh = Arc::new(fresh);
                warn!(
                    origin = %fresh.origin(),
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

    /// The credential for the current instant.
    #[must_use]
    pub fn current(&self) -> Arc<CastCredential> {
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

            match self.resolver.resolve_once(local_now()).await {
                Ok(fresh) => {
                    let changed = self.current.read().is_ok_and(|c| {
                        c.window() != fresh.window() || c.origin() != fresh.origin()
                    });
                    if changed {
                        info!(
                            origin = %fresh.origin(),
                            window_end = fresh.window().end_unix(),
                            "Cast receiver-auth credential refreshed"
                        );
                    }
                    if let Ok(mut slot) = self.current.write() {
                        *slot = Arc::new(fresh);
                    }
                }
                Err(e) => warn!(error = %e, "refreshing the Cast credential failed"),
            }
            self.refresh_crl().await;
        }
    }

    /// Refetch the CRL if the one held is missing or close enough to its end to be
    /// worth replacing. A stale CRL is not merely useless — a sender hard-fails on one
    /// outside its window — so this errs toward refreshing early.
    async fn refresh_crl(&self) {
        let now = local_now();
        let held_until = self.current_crl().map(|c| c.window().end_unix());
        if held_until.is_some_and(|end| end - now > CRL_REFRESH_LEAD_SECS) {
            return;
        }
        if let Some(fresh) = load_crl(&self.resolver.config).await {
            let end = fresh.window().end_unix();
            if held_until != Some(end) {
                info!(window_end = end, "Cast device CRL refreshed");
            }
            if let Ok(mut slot) = self.crl.write() {
                *slot = Some(Arc::new(fresh));
            }
        }
    }

    /// How long to wait before the next refresh attempt.
    fn next_wakeup(&self) -> Duration {
        let now = self.corrected_now();
        let current = self.current.read().ok().map(|c| Arc::clone(&c));
        let window_end = current.as_ref().map_or(now, |c| c.window().end_unix());
        let on_table = current
            .as_ref()
            .is_some_and(|c| c.origin().is_offline_table());

        // Running on the table with a reachable backend is a state worth leaving:
        // the table expires, the backend does not.
        let target = if on_table && self.resolver.config.network {
            now + i64::try_from(self.resolver.config.failure_backoff.as_secs()).unwrap_or(360)
        } else {
            window_end - i64::try_from(REFRESH_LEAD.as_secs()).unwrap_or(3600)
        };
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
    async fn resolve_once(&self, now_local: i64) -> Result<CastCredential, ReplayError> {
        let now = now_local + self.clock_offset.load(Ordering::Relaxed);
        let mut last = None;
        for identity in &self.config.identity_order {
            let attempt = match identity {
                Identity::Cks => self.resolve_cks(now_local, now).await,
                Identity::AirServer => self.resolve_airserver(now_local, now).await,
            };
            match attempt {
                Ok(credential) => return Ok(credential),
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
            match cache::load(path) {
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
                        if let Err(e) = cache::store(path, &credential) {
                            warn!(error = %e, "could not cache the CKS credential");
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
        let cached = self.airserver_db(now);
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
    fn airserver_db(&self, _now: i64) -> Option<Arc<AirServerDb>> {
        if let Ok(slot) = self.airserver_cache.read() {
            if let Some(db) = slot.as_ref() {
                return Some(Arc::clone(db));
            }
        }
        let path = self.config.airserver_db_path.as_ref()?;
        if !path.exists() {
            return None;
        }
        match AirServerDb::open(path) {
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

        let opened = path.clone();
        tokio::task::spawn_blocking(move || AirServerDb::open(&opened))
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

/// Refetch the CRL once it is within this long of expiring. The document runs about
/// seven days, so a day of lead leaves several refresh attempts before a sender would
/// start hard-failing on a stale one.
const CRL_REFRESH_LEAD_SECS: i64 = 86_400;

/// Load the Cast CRL: cache first, then the network.
///
/// Best-effort by design and never fatal. Without a CRL this receiver still
/// authenticates to Chrome (its fallback carries us); what is lost is Chromium-based
/// senders. Failing startup over that would trade a partial receiver for none.
async fn load_crl(config: &ReplayConfig) -> Option<CastCrl> {
    let path = config.crl_cache_path.clone()?;
    let now = local_now();

    let cached = match crl::read_cache(&path) {
        Ok(cached) => cached,
        Err(e) => {
            warn!(error = %e, "reading the cached Cast CRL failed");
            None
        }
    };
    // A cached CRL with room left is the whole answer; the endpoint is only consulted
    // when it cannot be.
    if let Some(crl) = &cached {
        if crl.window().end_unix() - now > CRL_REFRESH_LEAD_SECS {
            return cached;
        }
    }
    if !config.network {
        return cached;
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
            if let Err(e) = crl::write_cache(&path, &raw) {
                warn!(error = %e, "caching the Cast CRL failed");
            }
            Some(fresh)
        }
        Err(e) => {
            warn!(error = %e, "fetching the Cast CRL failed; Chromium-based senders will refuse this receiver");
            cached
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
        assert!(credential.valid_at(at));
        assert!(
            matches!(credential.origin(), CredentialOrigin::CksTable { .. }),
            "expected the CKS table to cover this instant, got {}",
            credential.origin()
        );
    }

    /// Past every table, resolution fails loudly instead of handing back something
    /// a sender will reject with no explanation.
    #[tokio::test]
    async fn past_every_table_startup_fails() {
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
        let second = provider.current_at(1_785_196_800 + 172_800);
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
}
