//! Ask the published Cast CRL, out loud, whether it revokes the identities we present.
//!
//! `#40` assumes a real sender refuses this receiver because the borrowed credential is
//! not trusted, and "revoked" is the first thing anyone reaches for when a device is
//! refused. That is checkable rather than arguable: fetch the CRL Chrome fetches, walk
//! each identity's chain with its trust anchor appended, and print the verdict per
//! identity — against the checked-in fixture *and* against the live list, because the
//! fixture only says what was true the day it was captured.
//!
//! ```text
//! cargo run -p cast-replay --example crl_check
//! ```
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::time::Duration;

use cast_replay::{CastCrl, Identity, ReplayConfig, ReplayProvider};

fn describe(crl: &CastCrl, label: &str) {
    println!(
        "{label:<16} {} revoked keys, {} issuers, {} serial ranges",
        crl.revoked_key_count(),
        crl.revoked_issuer_count(),
        crl.revoked_range_count(),
    );
}

fn report(label: &str, crl: &CastCrl, chain: &[&[u8]]) {
    let with_anchor = cast_replay::roots::with_anchor(chain);
    print!(
        "  {label:<16} {} certs (+anchor → {}) → ",
        chain.len(),
        with_anchor.len()
    );
    match crl.revokes(&with_anchor) {
        Ok(None) => println!("NOT revoked"),
        Ok(Some(revocation)) => println!("REVOKED: {revocation:?}"),
        Err(e) => println!("could not decide: {e}"),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let fixture = CastCrl::parse(include_bytes!("../fixtures/cast-crl-latest.bin"))
        .expect("the checked-in CRL parses");
    describe(&fixture, "checked-in CRL:");

    let live = match cast_replay::crl::fetch_blocking(Duration::from_secs(20)) {
        Ok(raw) => match CastCrl::parse(&raw) {
            Ok(crl) => {
                describe(&crl, "live CRL:");
                Some(crl)
            }
            Err(e) => {
                eprintln!("live CRL did not parse: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("could not fetch the live CRL ({e}); fixture only");
            None
        }
    };

    // Each identity resolved the way a live session resolves it — one provider per
    // identity, so `current()` cannot fall through to the other one and report a
    // verdict for a credential this run never asked about.
    for identity in [Identity::Cks, Identity::AirServer] {
        println!("\n{identity}:");
        let provider = match ReplayProvider::resolve(ReplayConfig {
            identity_order: vec![identity],
            ..ReplayConfig::default()
        })
        .await
        {
            Ok(provider) => provider,
            Err(e) => {
                println!("  unavailable: {e}");
                continue;
            }
        };
        let auth = provider.current();
        let credential = auth.credential();
        let mut chain: Vec<&[u8]> = vec![credential.device_cert_der()];
        chain.extend(credential.intermediates_der().iter().map(Vec::as_slice));

        report("checked-in CRL", &fixture, &chain);
        if let Some(live) = &live {
            report("live CRL", live, &chain);
        }
        // What the receiver would actually do with it: a CRL it declines to serve is
        // the loud half of a revocation, and the reason is worth printing next to the
        // verdict above.
        println!(
            "  would attach a CRL to AuthResponse: {}",
            if auth.crl().is_some() { "yes" } else { "no" }
        );
    }
}
