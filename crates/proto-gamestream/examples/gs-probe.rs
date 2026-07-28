//! `gs-probe <host> [--pin 1234] [--launch <app>]` — drive a real GameStream host.
//!
//! The GameStream sibling of `yt-selfplay` and `proto-spotify`'s `selfplay`, and needed
//! for the same reason: the scripted host in `tests/pairing_over_http.rs` is *our* reading
//! of Sunshine's source, so a test against it cannot fail in the one way that matters —
//! our reading being wrong. This binary talks to the real thing.
//!
//! Used two ways. `nix build .#checks.x86_64-linux.gamestream-vm` runs it against a real
//! Sunshine in a VM with no hardware and no person. Pointed at a host by hand, it is also
//! the quickest way to find out why a panel will not pair.
//!
//! Exits non-zero with the failure named. Prints a terminal sentinel line on success, so
//! the VM test can assert on it rather than on the absence of a crash.

use std::sync::Arc;

use proto_gamestream::{ClientIdentity, GameStreamClient, GameStreamError, PairingStore};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,proto_gamestream=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let host = args
        .next()
        .ok_or("usage: gs-probe <host> [--pin N] [--launch APP]")?;
    let mut pin = None;
    let mut launch = None;
    let mut state_dir = std::env::temp_dir().join("gs-probe-state");
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--pin" => pin = args.next(),
            "--launch" => launch = args.next(),
            "--state-dir" => {
                state_dir = args
                    .next()
                    .map(Into::into)
                    .ok_or("--state-dir needs a path")?;
            }
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }

    let store = PairingStore::new(&state_dir);
    let identity: Arc<ClientIdentity> = Arc::new(store.load_identity()?);
    let unique_id = store.load_unique_id()?;
    println!(
        "client identity ready ({} bytes of cert)",
        identity.cert_der().len()
    );

    let mut client = GameStreamClient::new(
        Arc::clone(&identity),
        unique_id.clone(),
        host.clone(),
        proto_gamestream::nvhttp::DEFAULT_HTTP_PORT,
    );

    // 1. Who is it, and does it already know us?
    let info = client.server_info().await?;
    println!(
        "serverinfo: hostname={:?} appversion={} https_port={} sunshine={} state={}",
        info.hostname,
        info.app_version,
        info.https_port,
        info.is_sunshine(),
        info.state
    );
    if !info.is_sunshine() {
        // Not fatal — GFE speaks the same protocol — but worth saying, because every
        // GFE-only workaround in the client hangs off this bit.
        println!("note: host does not self-identify as Sunshine");
    }

    // 2. Pair, if asked. Restores a previous pairing otherwise.
    if let Some(server) = store.load_pairing(&host) {
        println!("restoring the pairing from {}", state_dir.display());
        client = client.with_pairing(server, info.https_port);
    } else if let Some(pin) = pin {
        println!("pairing: type {pin} into the host now (Sunshine's web UI, or its stdin)");
        match client.pair(&pin, info.https_port).await {
            Ok(()) => {
                let server = client
                    .pairing()
                    .ok_or("paired but no certificate to persist")?;
                store.save_pairing(&host, server)?;
                println!("paired, and the host certificate is persisted");
            }
            // Named separately because these are the two failures a person can act on,
            // and they need different actions.
            Err(GameStreamError::WrongPin) => {
                return Err("the PIN did not match — retype it and try again".into());
            }
            Err(GameStreamError::Pairing(msg)) => {
                return Err(format!("the host failed a trust check: {msg}").into());
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        return Err("not paired with this host, and no --pin was given".into());
    }

    // 3. The first request that actually exercises mutual TLS end to end. `PairStatus`
    //    over TLS is the host's own answer to "do I trust this certificate".
    let info = client.server_info().await?;
    if !info.paired {
        return Err("the host answered over TLS but does not consider us paired".into());
    }
    println!("mutual TLS works and the host considers us paired");

    // 4. The app list — the first thing a chooser would need.
    let apps = client.apps().await?;
    if apps.is_empty() {
        return Err("the host lists no apps; nothing could be launched".into());
    }
    println!("applist: {} app(s)", apps.len());
    for app in &apps {
        println!(
            "  {:>10}  {}{}",
            app.id,
            app.title,
            if app.hdr_supported { "  (HDR)" } else { "" }
        );
    }

    // 5. Launch, if asked. Kept opt-in: it starts something on someone's PC, and in a
    //    headless VM it fails at the encoder probe for reasons that say nothing about
    //    the protocol.
    if let Some(wanted) = launch {
        let chosen = apps
            .iter()
            .find(|a| a.title.eq_ignore_ascii_case(&wanted))
            .ok_or_else(|| format!("host has no app named {wanted:?}"))?;
        let (ri_key, _iv, ri_key_id) = proto_gamestream::generate_session_keys();
        let params = proto_gamestream::LaunchParams {
            app_id: chosen.id,
            resume: info.current_game != 0,
            width: 1920,
            height: 1080,
            fps: 60,
            optimize_settings: false,
            play_audio_on_host: false,
            surround_audio_info: 196_610,
            ri_key,
            ri_key_id,
        };
        match client.launch(&params).await {
            Ok(launched) => {
                println!("launched: sessionUrl0={}", launched.session_url);
                let encrypted = launched.session_url.starts_with("rtspenc://");
                println!("rtsp encryption: {}", if encrypted { "on" } else { "off" });
                client.cancel().await;
            }
            Err(GameStreamError::Nvhttp { code, message }) => {
                // The host's own words. 503 here is usually "no display attached",
                // which is the expected answer in a headless VM and not a protocol bug.
                println!("launch refused by the host ({code}): {message}");
            }
            Err(e) => return Err(e.into()),
        }
    }

    println!("gs-probe completed");
    Ok(())
}
