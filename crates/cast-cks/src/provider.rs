//! The credential provider: fallback order, disk cache, and background refresh.
//!
//! This is the only module here that touches a socket or the filesystem
//! (ground rule 3). Everything it decides is decided from [`crate::api`],
//! [`crate::table`] and [`crate::cache`], which are pure.
//!
//! ## Resolution order
//!
//! 1. **The cache**, if it holds a credential still inside its window. A cached
//!    credential *is* a previously fetched one, so using it is not a downgrade —
//!    it just avoids spending a request per restart on material we already have.
//! 2. **The backend.** Preferred over the table because it keeps working
//!    indefinitely, where the table stops on 2027-12-06.
//! 3. **The checked-in table.** Always available, needs no network, and covers
//!    every window through 2027-12-06.
//!
//! A failed fetch is not fatal at any point — it falls through to the table and
//! is retried after [`CksConfig::failure_backoff`], matching the reference
//! client's 360-second hold-down. An unattended panel that loses its uplink keeps
//! authenticating.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use crate::table::StaticTable;
use crate::{api, cache, CastCredential, CksError, CredentialOrigin};

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

/// Cap on the response body. A live response is a few kilobytes.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

/// How the provider is allowed to obtain credentials.
#[derive(Debug, Clone)]
pub struct CksConfig {
    /// Whether the backend may be contacted at all. With this off the provider is
    /// entirely offline and bounded by the table's 2027-12-06 end.
    pub network: bool,
    /// Where to cache a fetched credential. `None` disables caching.
    pub cache_path: Option<PathBuf>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Hold-down after a failed fetch.
    pub failure_backoff: Duration,
}

impl Default for CksConfig {
    fn default() -> Self {
        Self {
            network: true,
            cache_path: Some(cache::default_path()),
            timeout: DEFAULT_TIMEOUT,
            failure_backoff: DEFAULT_FAILURE_BACKOFF,
        }
    }
}

/// Everything needed to run the fallback chain, minus the credential it produces.
///
/// Split out so [`CksProvider`] can be built with a real credential already in
/// hand. The alternative — construct the provider around a placeholder and
/// overwrite it — makes "no credential yet" a representable state on the
/// connection path, which is exactly the state that must not exist.
struct Resolver {
    table: StaticTable,
    config: CksConfig,
    /// `backend_now - local_now`, learned from a successful fetch.
    ///
    /// A panel whose clock is wrong picks the wrong window and every sender
    /// rejects it. The reference client keeps the same correction for the same
    /// reason; it is why the schedule does not follow the device clock.
    clock_offset: AtomicI64,
    /// When the backend may next be tried, as a local Unix second.
    retry_after: AtomicI64,
}

/// Supplies the current Cast receiver-auth credential.
///
/// Reads are cheap and synchronous ([`Self::current_at`]) because they sit on the
/// per-connection path; refreshing happens in [`Self::run`]. There is always a
/// credential: one is resolved before the provider exists.
pub struct CksProvider {
    resolver: Resolver,
    current: RwLock<Arc<CastCredential>>,
}

impl CksProvider {
    /// Resolve a credential and build the provider.
    ///
    /// Fails only if *no* path yields a credential — which, inside the table's
    /// range, means the embedded fixtures are broken. Startup failing loudly here
    /// is deliberate: a receiver that comes up without a credential looks healthy
    /// and rejects every sender.
    ///
    /// # Errors
    /// [`CksError`] if neither the cache, the backend nor the table produces a
    /// credential.
    pub async fn resolve(config: CksConfig) -> Result<Self, CksError> {
        let resolver = Resolver {
            table: StaticTable::load()?,
            config,
            clock_offset: AtomicI64::new(0),
            retry_after: AtomicI64::new(0),
        };
        let credential = resolver.resolve_once(local_now()).await?;
        info!(
            origin = %credential.origin(),
            window_start = credential.window().start_unix(),
            window_end = credential.window().end_unix(),
            "Cast receiver-auth credential resolved"
        );
        Ok(Self {
            resolver,
            current: RwLock::new(Arc::new(credential)),
        })
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
        match self.resolver.table.credential_at(now) {
            Ok(fresh) => {
                let fresh = Arc::new(fresh);
                warn!(
                    origin = %fresh.origin(),
                    "the Cast credential aged out; re-issued from the checked-in table"
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
        }
    }

    /// How long to wait before the next refresh attempt.
    fn next_wakeup(&self) -> Duration {
        let now = self.corrected_now();
        let current = self.current.read().ok().map(|c| Arc::clone(&c));
        let window_end = current.as_ref().map_or(now, |c| c.window().end_unix());
        let on_table = current
            .as_ref()
            .is_some_and(|c| matches!(c.origin(), CredentialOrigin::StaticTable { .. }));

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
    /// Run the fallback chain once.
    async fn resolve_once(&self, now_local: i64) -> Result<CastCredential, CksError> {
        let now = now_local + self.clock_offset.load(Ordering::Relaxed);

        if let Some(path) = &self.config.cache_path {
            match cache::load(path) {
                Ok(Some(cached)) if cached.valid_at(now) => {
                    debug!("using the cached Cast credential");
                    return Ok(cached);
                }
                Ok(Some(_)) => debug!("the cached Cast credential is outside its window"),
                Ok(None) => {}
                // A cache that will not load is a real fault, but not one worth
                // failing over — the remaining paths do not depend on it.
                Err(e) => warn!(error = %e, "ignoring the Cast credential cache"),
            }
        }

        if self.config.network && now_local >= self.retry_after.load(Ordering::Relaxed) {
            match self.fetch(now_local).await {
                Ok(credential) => {
                    if let Some(path) = &self.config.cache_path {
                        if let Err(e) = cache::store(path, &credential) {
                            warn!(error = %e, "could not cache the Cast credential");
                        }
                    }
                    return Ok(credential);
                }
                Err(e) => {
                    let until = now_local
                        + i64::try_from(self.config.failure_backoff.as_secs()).unwrap_or(360);
                    self.retry_after.store(until, Ordering::Relaxed);
                    warn!(
                        error = %e,
                        backoff_secs = self.config.failure_backoff.as_secs(),
                        "the CKS backend is unreachable; falling back to the checked-in table"
                    );
                }
            }
        }

        self.table.credential_at(now)
    }

    /// One request to the backend.
    async fn fetch(&self, ts: i64) -> Result<CastCredential, CksError> {
        let timeout = self.config.timeout;
        // ureq is blocking, so it does not belong on the runtime (ground rule 4).
        let body = tokio::task::spawn_blocking(move || fetch_blocking(ts, timeout))
            .await
            .map_err(|e| CksError::Http(format!("joining the CKS request: {e}")))??;

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
            return Err(CksError::Response(format!(
                "the backend returned a credential valid {}..{}, which does not cover {now}",
                credential.window().start_unix(),
                credential.window().end_unix()
            )));
        }
        Ok(credential)
    }
}

/// Perform the request. Blocking; called only from `spawn_blocking`.
fn fetch_blocking(ts: i64, timeout: Duration) -> Result<Vec<u8>, CksError> {
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
        .map_err(|e| CksError::Http(e.to_string()))?;

    // Bounded: the body is a handful of certificates, and this is a third party's
    // endpoint — an unbounded read would let a bad day upstream exhaust the panel.
    let mut body = Vec::new();
    let mut reader = response.into_reader().take(MAX_RESPONSE_BYTES);
    std::io::Read::read_to_end(&mut reader, &mut body)
        .map_err(|e| CksError::Http(format!("reading the CKS response: {e}")))?;
    Ok(body)
}

/// A TLS client config trusting only the two pinned roots.
///
/// The system trust store is deliberately unused, matching the reference client
/// (`CURLOPT_CAINFO`/`CAPATH` both `NULL`, anchors injected through
/// `CURLOPT_SSL_CTX_FUNCTION`). It also means this path does not depend on the
/// Windows deploy target having a usable root store.
fn pinned_tls_config() -> Result<rustls::ClientConfig, CksError> {
    let mut roots = rustls::RootCertStore::empty();
    for der in crate::pem::decode_all(PINNED_ROOTS_PEM, "CERTIFICATE")? {
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .map_err(|e| CksError::Http(format!("pinned root is not usable: {e}")))?;
    }
    if roots.is_empty() {
        return Err(CksError::Http("no pinned roots were loaded".into()));
    }
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| CksError::Http(format!("building the CKS TLS config: {e}")))
        .map(|b| b.with_root_certificates(roots).with_no_client_auth())
}

/// The local clock as Unix seconds.
///
/// A clock set before 1970 reads as 0, which is before the table's epoch and so
/// produces [`CksError::OutOfRange`] — a typed failure rather than a confidently
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

    /// Inside the table's range, an offline provider always resolves.
    #[tokio::test]
    async fn resolves_offline_from_the_table() {
        let provider = CksProvider::resolve(CksConfig {
            network: false,
            cache_path: None,
            ..CksConfig::default()
        })
        .await
        .unwrap();

        let at = 1_785_196_800; // 2026-07-28, window 652
        let credential = provider.current_at(at);
        assert!(credential.valid_at(at));
        assert_eq!(
            credential.origin(),
            &CredentialOrigin::StaticTable { index: 652 }
        );
    }

    /// A window roll must produce a different credential, not a stale one.
    #[tokio::test]
    async fn a_window_roll_re_issues_inline() {
        let provider = CksProvider::resolve(CksConfig {
            network: false,
            cache_path: None,
            ..CksConfig::default()
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
        let provider = CksProvider::resolve(CksConfig {
            network: false,
            cache_path: None,
            ..CksConfig::default()
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
        let provider = CksProvider::resolve(CksConfig {
            network: false,
            cache_path: None,
            ..CksConfig::default()
        })
        .await
        .unwrap();
        provider
            .resolver
            .retry_after
            .store(i64::MAX, Ordering::Relaxed);
        let credential = provider.resolver.resolve_once(1_785_196_800).await.unwrap();
        assert!(credential.valid_at(1_785_196_800));
    }

    /// A clock so wrong that no window covers it must be a typed error, not a
    /// credential from the wrong window.
    #[tokio::test]
    async fn a_time_outside_the_table_is_an_error() {
        let provider = CksProvider::resolve(CksConfig {
            network: false,
            cache_path: None,
            ..CksConfig::default()
        })
        .await
        .unwrap();
        assert!(matches!(
            provider.resolver.resolve_once(0).await,
            Err(CksError::OutOfRange { .. })
        ));
    }
}
