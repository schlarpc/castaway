//! The GameStream client — the orchestration over [`crate::nvhttp`],
//! [`crate::pairing`], and [`crate::http`].
//!
//! This is the only module that sequences round trips, and it is deliberately thin:
//! each step asks the pure layers what to send, hands the bytes to the transport, and
//! feeds the answer back. The HTTP itself is blocking, so every method that touches a
//! socket hops through `spawn_blocking` — pairing in particular parks on a human
//! walking over to the host and typing a PIN, which is not a timeout to tune but a
//! wait to allow.

use std::sync::Arc;

use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::error::GameStreamError;
use crate::http::NvHttpClient;
use crate::identity::ClientIdentity;
use crate::nvhttp::{
    self, App, LaunchParams, LaunchResponse, RequestBuilder, ServerInfo, Transport, UniqueId,
};
use crate::pairing::{self, PairedServer, PairingSeed};

/// A client bound to one host. Cheap to build; holds no connection.
pub struct GameStreamClient {
    identity: Arc<ClientIdentity>,
    requests: RequestBuilder,
    host: String,
    http_port: u16,
    /// Set once paired: the pinned host certificate and the TLS port it answers on.
    paired: Option<(PairedServer, u16)>,
}

impl GameStreamClient {
    /// A client for a host we have not paired with yet.
    #[must_use]
    pub fn new(
        identity: Arc<ClientIdentity>,
        unique_id: UniqueId,
        host: impl Into<String>,
        http_port: u16,
    ) -> Self {
        Self {
            identity,
            requests: RequestBuilder::new(unique_id),
            host: host.into(),
            http_port,
            paired: None,
        }
    }

    /// Restore a pairing from persisted state, skipping the handshake.
    #[must_use]
    pub fn with_pairing(mut self, server: PairedServer, https_port: u16) -> Self {
        self.paired = Some((server, https_port));
        self
    }

    /// The pinned host certificate, once paired — what the caller should persist.
    #[must_use]
    pub fn pairing(&self) -> Option<&PairedServer> {
        self.paired.as_ref().map(|(s, _)| s)
    }

    /// The host address this client dials.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Build the transport for the current pairing state.
    fn transport(&self) -> Result<NvHttpClient, GameStreamError> {
        let client = NvHttpClient::unpaired(self.host.clone(), self.http_port);
        match &self.paired {
            Some((server, port)) => {
                client.with_tls(&self.identity, server.server_cert_der.clone(), *port)
            }
            None => Ok(client),
        }
    }

    /// Ask the host about itself. Uses TLS when paired, which is the only way
    /// `PairStatus` means anything; falls back to plain HTTP when it does not.
    ///
    /// # Errors
    /// [`GameStreamError::Http`] / [`GameStreamError::Xml`] on transport or parse
    /// failure.
    pub async fn server_info(&self) -> Result<ServerInfo, GameStreamError> {
        let transport = if self.paired.is_some() {
            Transport::Tls
        } else {
            Transport::Plain
        };
        let request = self.requests.server_info(transport, &fresh_uuid());
        let client = self.transport()?;
        let body = spawn_blocking(move || client.send(&request))
            .await
            .map_err(|e| GameStreamError::Http(e.to_string()))??;
        match nvhttp::parse_server_info(&body) {
            // A paired client whose certificate the host has forgotten gets a 401. The
            // plain-HTTP answer still carries the version and port we need to re-pair,
            // so ask again rather than failing the whole discovery.
            Err(GameStreamError::NotPaired { .. }) if self.paired.is_some() => {
                warn!(host = %self.host, "host no longer trusts our certificate; re-pairing needed");
                let request = self.requests.server_info(Transport::Plain, &fresh_uuid());
                let client = NvHttpClient::unpaired(self.host.clone(), self.http_port);
                let body = spawn_blocking(move || client.send(&request))
                    .await
                    .map_err(|e| GameStreamError::Http(e.to_string()))??;
                let mut info = nvhttp::parse_server_info(&body)?;
                info.paired = false;
                Ok(info)
            }
            other => other,
        }
    }

    /// Run the pairing handshake. Blocks on the host's side of it — Sunshine holds the
    /// first response open until someone types the PIN into its web UI, so this can
    /// take as long as it takes to walk across the room.
    ///
    /// On success the client is paired in place and [`Self::pairing`] returns the
    /// certificate to persist.
    ///
    /// # Errors
    /// [`GameStreamError::WrongPin`] when the PIN did not match,
    /// [`GameStreamError::Pairing`] when the host failed a trust check.
    pub async fn pair(&mut self, pin: &str, https_port: u16) -> Result<(), GameStreamError> {
        let seed = PairingSeed {
            salt: random_bytes(),
            challenge: random_bytes(),
            secret: random_bytes(),
        };
        info!(host = %self.host, "GameStream pairing: waiting for the PIN on the host");

        let (state, phase) = pairing::start(&self.identity, pin, seed);
        let plaincert = self
            .pair_phase(&phase, Transport::Plain, "plaincert")
            .await?;
        let (state, phase) = state.on_server_cert(&plaincert)?;

        let response = self
            .pair_phase(&phase, Transport::Plain, "challengeresponse")
            .await?;
        let (state, phase) = state.on_challenge_response(&response)?;

        let secret = self
            .pair_phase(&phase, Transport::Plain, "pairingsecret")
            .await?;
        let (state, phase) = state.on_pairing_secret(&secret)?;

        // Phase 4's verdict is the `<paired>` flag, not the status code.
        let request = self.requests.pair(&phase, Transport::Plain, &fresh_uuid());
        let client = self.transport()?;
        let body = spawn_blocking(move || client.send(&request))
            .await
            .map_err(|e| GameStreamError::Http(e.to_string()))??;
        let (paired, _) = nvhttp::parse_pair_phase(&body, "paired")?;
        let server = state.finish(paired)?;

        // Phase 5 is the proof: the first mutual-TLS request with the new certificate.
        // Without it a "successful" pairing can still leave every later request 401ing,
        // which looks like a host problem rather than a pairing one.
        self.paired = Some((server, https_port));
        match self.confirm_over_tls().await {
            Ok(()) => {
                info!(host = %self.host, "GameStream paired");
                Ok(())
            }
            Err(e) => {
                // Every failure past this point must put the pairing back, including a
                // transport error — a client left claiming to be paired goes on to 401
                // on every later request with nothing anywhere saying why.
                self.paired = None;
                Err(e)
            }
        }
    }

    /// Phase 5: the first mutual-TLS request with the freshly accepted certificate.
    async fn confirm_over_tls(&self) -> Result<(), GameStreamError> {
        let request = self.requests.pair(
            &pairing::pair_challenge_request(),
            Transport::Tls,
            &fresh_uuid(),
        );
        let client = self.transport()?;
        let body = spawn_blocking(move || client.send(&request))
            .await
            .map_err(|e| GameStreamError::Http(e.to_string()))??;
        match nvhttp::parse_pair_phase(&body, "paired") {
            Ok((true, _)) => Ok(()),
            Ok((false, _)) | Err(GameStreamError::NotPaired { .. }) => {
                Err(GameStreamError::Pairing(
                    "the host accepted our certificate but then refused it over TLS".into(),
                ))
            }
            Err(e) => Err(e),
        }
    }

    /// One pairing round trip, returning the named element's text.
    async fn pair_phase(
        &self,
        phase: &pairing::PhaseRequest,
        transport: Transport,
        element: &str,
    ) -> Result<String, GameStreamError> {
        let request = self.requests.pair(phase, transport, &fresh_uuid());
        let client = self.transport()?;
        let body = spawn_blocking(move || client.send(&request))
            .await
            .map_err(|e| GameStreamError::Http(e.to_string()))??;
        let (paired, value) = nvhttp::parse_pair_phase(&body, element)?;
        if !paired {
            return Err(GameStreamError::Pairing(format!(
                "host refused the pairing handshake before `{element}`"
            )));
        }
        Ok(value)
    }

    /// The host's app list.
    ///
    /// # Errors
    /// [`GameStreamError::NotPaired`] if we are not paired; transport/parse errors
    /// otherwise.
    pub async fn apps(&self) -> Result<Vec<App>, GameStreamError> {
        self.require_paired()?;
        let request = self.requests.app_list(&fresh_uuid());
        let client = self.transport()?;
        let body = spawn_blocking(move || client.send(&request))
            .await
            .map_err(|e| GameStreamError::Http(e.to_string()))??;
        nvhttp::parse_app_list(&body)
    }

    /// Launch (or resume) an app, returning the RTSP session URL the streaming core
    /// takes. `resume` comes from `/serverinfo`'s `currentgame`, so it is passed in
    /// rather than guessed here.
    ///
    /// # Errors
    /// [`GameStreamError::Nvhttp`] carries the host's own refusal — "an app is already
    /// running", "is a display connected and turned on?" — which is the text worth
    /// showing a person.
    pub async fn launch(&self, params: &LaunchParams) -> Result<LaunchResponse, GameStreamError> {
        self.require_paired()?;
        let request = self.requests.launch(params, &fresh_uuid());
        let client = self.transport()?;
        let body = spawn_blocking(move || client.send(&request))
            .await
            .map_err(|e| GameStreamError::Http(e.to_string()))??;
        nvhttp::parse_launch(&body)
    }

    /// Ask the host to stop whatever is running. Best-effort by design: this is called
    /// on the way out of a session, and a host that already stopped is not an error
    /// worth propagating.
    pub async fn cancel(&self) {
        if self.require_paired().is_err() {
            return;
        }
        let request = self.requests.cancel(&fresh_uuid());
        let Ok(client) = self.transport() else {
            return;
        };
        match spawn_blocking(move || client.send(&request)).await {
            Ok(Ok(_)) => info!(host = %self.host, "asked the host to stop streaming"),
            Ok(Err(e)) => warn!(host = %self.host, error = %e, "cancel failed"),
            Err(e) => warn!(host = %self.host, error = %e, "cancel task failed"),
        }
    }

    fn require_paired(&self) -> Result<(), GameStreamError> {
        if self.paired.is_some() {
            Ok(())
        } else {
            Err(GameStreamError::NotPaired {
                host: self.host.clone(),
            })
        }
    }
}

/// A fresh per-request UUID. Sunshine ignores it; GFE wanted it.
fn fresh_uuid() -> String {
    pairing::hex_encode(&random_bytes())
}

fn random_bytes() -> [u8; 16] {
    use rand::Rng;
    let mut out = [0u8; 16];
    rand::rng().fill_bytes(&mut out);
    out
}

/// Generate the per-session AES key and IV that `/launch` carries as `rikey`/`rikeyid`
/// and the streaming core uses for input, control, and audio.
///
/// `rikeyid` is the first four IV bytes read as a **signed** big-endian integer — it is
/// negative about half the time, and a client that emitted it unsigned would hand the
/// host a different IV than the one it uses itself.
#[must_use]
pub fn generate_session_keys() -> ([u8; 16], [u8; 16], i32) {
    let key = random_bytes();
    let iv = random_bytes();
    let id = i32::from_be_bytes([iv[0], iv[1], iv[2], iv[3]]);
    (key, iv, id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn session_key_ids_are_the_ivs_first_four_bytes_signed() {
        for _ in 0..64 {
            let (_key, iv, id) = generate_session_keys();
            assert_eq!(id.to_be_bytes(), iv[..4]);
        }
    }

    #[test]
    fn a_negative_key_id_round_trips_through_its_decimal_form() {
        // The failure this catches is silent: an unsigned rikeyid still parses on the
        // host, it just builds a different IV, and the session then fails to decrypt
        // input and audio with no error anywhere.
        let iv = [0xffu8, 0xff, 0xff, 0xff];
        let id = i32::from_be_bytes(iv);
        assert_eq!(id, -1);
        assert_eq!(id.to_string(), "-1");
        assert_eq!(id.to_string().parse::<i32>().unwrap().to_be_bytes(), iv);
    }

    #[test]
    fn an_unpaired_client_refuses_tls_only_calls_by_name() {
        let identity = Arc::new(ClientIdentity::generate().unwrap());
        let client = GameStreamClient::new(identity, UniqueId::new("abc"), "10.0.0.7", 47989);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        match rt.block_on(client.apps()) {
            Err(GameStreamError::NotPaired { host }) => assert_eq!(host, "10.0.0.7"),
            other => panic!("expected NotPaired, got {other:?}"),
        }
    }
}
