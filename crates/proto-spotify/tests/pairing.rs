//! The zeroconf pairing surface, driven the way a phone drives it — offline.
//!
//! `examples/selfplay.rs` proves the same path against real Spotify, but it needs an
//! account and the internet, so it cannot run in CI. This runs the whole HTTP exchange
//! in-process against the real router: `getInfo`, a Diffie-Hellman against the key it
//! published, a properly wrapped credentials blob, and `addUser`. Everything except the
//! login behind it.
//!
//! What it is really guarding is the *join* between the two halves. Each half already has
//! unit tests; the failure this catches is the one where `getInfo` advertises one device
//! id and the blob is encrypted against another, which is invisible in either half alone
//! and reaches the phone as an unexplained "pairing expired" (OPEN-QUESTIONS Q10).

#![allow(clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use proto_spotify::crypto::{encode_credentials_blob, encrypt_blob, DhKeys};
use proto_spotify::SpotifyService;
use serde_json::Value;
use tower::ServiceExt as _;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Ask the router for `getInfo` and parse it.
async fn get_info(app: &axum::Router) -> Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/spotify?action=getInfo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Do what a phone does: DH against the published key, wrap credentials, POST them.
async fn add_user(app: &axum::Router, info: &Value, user: &str) -> StatusCode {
    let device_id = info["deviceID"].as_str().unwrap();
    let receiver_key = B64.decode(info["publicKey"].as_str().unwrap()).unwrap();

    let inner = encode_credentials_blob(user, device_id, 1, b"reusable-credentials").unwrap();
    let phone = DhKeys::generate();
    let shared = phone.shared_secret(&receiver_key);
    let blob = encrypt_blob(&inner, &shared, &[0x5a; 16]).unwrap();

    let form = serde_urlencoded::to_string([
        ("action", "addUser".to_owned()),
        ("userName", user.to_owned()),
        ("blob", B64.encode(&blob)),
        ("clientKey", B64.encode(phone.public_key())),
    ])
    .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/spotify")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn a_phone_can_pair_against_the_key_getinfo_published() {
    let svc = SpotifyService::new("Hackerspace TV", "0f8c2e10castaway0001000000000001");
    let app = svc.router();

    let info = get_info(&app).await;
    assert_eq!(info["remoteName"], "Hackerspace TV");
    // Premium-only is not our choice to make, but advertising otherwise would have every
    // free-tier user tap the device and get silence.
    assert_eq!(info["accountReq"], "PREMIUM");

    assert_eq!(add_user(&app, &info, "alice").await, StatusCode::OK);

    // The receiver now names who is on it — which is what the phone reads back to decide
    // whether the device is "yours".
    let after = get_info(&app).await;
    assert_eq!(after["activeUser"], "alice");
}

#[tokio::test]
async fn the_second_person_to_pair_takes_the_device() {
    // A shared panel: last writer wins, deliberately, and the device says so.
    let svc = SpotifyService::new("Hackerspace TV", "0f8c2e10castaway0001000000000001");
    let app = svc.router();
    let info = get_info(&app).await;

    assert_eq!(add_user(&app, &info, "alice").await, StatusCode::OK);
    assert_eq!(add_user(&app, &info, "bob").await, StatusCode::OK);
    assert_eq!(get_info(&app).await["activeUser"], "bob");
}

#[tokio::test]
async fn a_blob_encrypted_against_the_wrong_key_is_refused() {
    // The DH is what stops a passer-by pushing credentials at the device, so a blob that
    // does not match the published key has to fail the checksum rather than be decrypted
    // into nonsense and handed to a login.
    let svc = SpotifyService::new("Hackerspace TV", "0f8c2e10castaway0001000000000001");
    let app = svc.router();
    let mut info = get_info(&app).await;

    // Substitute a public key the receiver has no private half for.
    let impostor = DhKeys::generate();
    info["publicKey"] = Value::String(B64.encode(impostor.public_key()));

    assert_eq!(
        add_user(&app, &info, "mallory").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get_info(&app).await["activeUser"],
        "",
        "a refused pairing must not claim the device"
    );
}

#[tokio::test]
async fn a_malformed_add_user_is_refused_rather_than_accepted_blank() {
    let svc = SpotifyService::new("Hackerspace TV", "deadbeef");
    let app = svc.router();

    for body in [
        "action=addUser",
        "action=addUser&userName=alice",
        "action=addUser&userName=alice&blob=!!!&clientKey=!!!",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/spotify")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "body: {body}");
    }
}
