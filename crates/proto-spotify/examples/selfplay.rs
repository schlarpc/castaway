//! The phone half of a Spotify Connect session, scripted — so "can someone pick this
//! thing in Spotify and queue a song?" is a command that exits 0 or 1 instead of a person
//! with a handset.
//!
//! Not a `nix flake check`. Like `yt-selfplay`, this needs the real internet and a real
//! account: Spotify's cloud is a third party to every part of this session and there is
//! nothing to fake it with. A test that stops at "the receiver answered `status: 101`"
//! tests almost none of what matters.
//!
//! ```text
//! cargo run -p proto-spotify --example selfplay -- http://<receiver>:8080
//! ```
//!
//! What it does, in the order a phone does it:
//!  1. log in to Spotify as the harness account, and turn that into the *reusable*
//!     credentials a phone would hold
//!  2. read the receiver's `getInfo` for its DH public key and device id
//!  3. wrap those credentials the way a phone wraps them and POST `addUser`
//!  4. wait for the device to appear in the account's device list   <- the cliff
//!  5. transfer playback to it and start a track
//!  6. queue a second track, and check it is really in the queue
//!  7. skip, and check the queued track is now the one playing
//!
//! Step 4 is the one that fails silently in every other test: the receiver answers the
//! pairing happily, the phone shows it connected, and nothing ever appears in the picker
//! because the login behind the pairing failed. That is the failure this exists to name.
//!
//! ## Credentials
//!
//! Read from `.env.local`, else `.env` (both gitignored; see `.env.example`). The
//! receiver itself never needs any of this — it holds no account. These exist only so the
//! harness can act as the controller.
//!
//! The first run needs a browser once: Spotify's OAuth is an authorization-code flow with
//! no device-code variant, so something has to visit a URL. The run prints it, serves the
//! loopback redirect itself, and then prints a refresh token to paste into `.env.local`.
//! Every run after that is hands-free. No Android VM and no headless browser are needed
//! for the receiver — the *receiver* is driven entirely over HTTP and the cloud.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use librespot_core::authentication::Credentials;
use librespot_core::{Session, SessionConfig};
use proto_spotify::crypto::{encode_credentials_blob, encrypt_blob, DhKeys};
use serde_json::Value;

/// Scopes the controller half needs: read what is playing, change it, and queue.
const SCOPES: &[&str] = &[
    "user-read-playback-state",
    "user-modify-playback-state",
    "streaming",
];

/// Where librespot's OAuth helper parks its loopback listener.
const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

/// `AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS` — what a real pairing carries.
const AUTH_TYPE_STORED: u32 = 1;

/// How long to wait for the receiver to finish logging in and register itself.
///
/// Generous: this covers an access-point handshake, login5, and a connect-state PUT, over
/// whatever the venue's uplink is.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(45);

/// How long to wait for a transport command to be reflected back by the cloud.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

type Error = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,selfplay=info".into()),
        )
        .init();

    let Some(base) = std::env::args().nth(1) else {
        // Printed rather than returned: a `Box<dyn Error>` full of `\n` escapes is not
        // usage text.
        eprintln!("usage: selfplay <receiver-base-url>");
        eprintln!("  e.g. selfplay http://10.0.0.5:8080");
        eprintln!("credentials come from .env.local / .env (see .env.example)");
        std::process::exit(2);
    };
    let base = base.trim_end_matches('/').to_owned();
    let env = load_env()?;

    // 1. Become a logged-in Spotify client, and keep what a phone would keep.
    let token = access_token(&env).await?;
    let stored = stored_credentials(&token).await?;
    step(&format!("logged in as {}", stored.username));

    // 2-3. Pair with the receiver exactly as a phone does.
    let info = get_info(&base).await?;
    step(&format!(
        "receiver is \"{}\" (device {})",
        info.remote_name, info.device_id
    ));
    add_user(&base, &info, &stored).await?;
    step("addUser accepted");

    // 4. The cliff. Pairing succeeding tells us nothing about the login behind it.
    let device_id = wait_for_device(&token, &info.remote_name).await?;
    step(&format!("device registered with Spotify (id {device_id})"));

    // 5-7. The actual claim: control and queueing.
    let tracks = pick_tracks(&token).await?;
    transfer_and_play(&token, &device_id, &tracks[0]).await?;
    wait_for_playing(&token, &tracks[0].uri, "transferred track").await?;
    step(&format!("playing {}", tracks[0]));

    queue(&token, &tracks[1]).await?;
    wait_for_queued(&token, &tracks[1].uri).await?;
    step(&format!("queued {}", tracks[1]));

    next(&token).await?;
    wait_for_playing(&token, &tracks[1].uri, "queued track after skip").await?;
    step(&format!("skipped to the queued track: {}", tracks[1]));

    println!("\nOK — pairing, playback, control and queueing all work against {base}");
    Ok(())
}

fn step(msg: &str) {
    println!("  ✓ {msg}");
}

// ---------------------------------------------------------------- credentials

/// Read `.env.local`, then `.env`, without pulling in a dotenv crate for six lines.
/// Earlier files win, and a real environment variable beats both.
fn load_env() -> Result<HashMap<String, String>, Error> {
    let mut out = HashMap::new();
    for path in [".env.local", ".env"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            out.entry(k.trim().to_owned())
                .or_insert_with(|| v.trim().trim_matches('"').to_owned());
        }
    }
    for key in ["SPOTIFY_CLIENT_ID", "SPOTIFY_REFRESH_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            out.insert(key.to_owned(), v);
        }
    }
    if out.get("SPOTIFY_CLIENT_ID").is_none_or(String::is_empty) {
        return Err("SPOTIFY_CLIENT_ID is not set — copy .env.example to .env.local".into());
    }
    Ok(out)
}

/// An access token for the Web API half, refreshing silently when we can.
async fn access_token(env: &HashMap<String, String>) -> Result<String, Error> {
    let client_id = &env["SPOTIFY_CLIENT_ID"];
    let client = librespot_oauth::OAuthClientBuilder::new(client_id, REDIRECT_URI, SCOPES.to_vec())
        .open_in_browser()
        .build()?;

    let refresh = env.get("SPOTIFY_REFRESH_TOKEN").filter(|s| !s.is_empty());
    if let Some(refresh) = refresh {
        match client.refresh_token_async(refresh).await {
            Ok(token) => return Ok(token.access_token),
            Err(e) => eprintln!("  ! stored refresh token rejected ({e}), logging in again"),
        }
    }

    eprintln!("  ! no usable refresh token — a browser has to authorise this once");
    let token = client.get_access_token_async().await?;
    println!("\nSave this in .env.local so later runs need no browser:\n");
    println!("SPOTIFY_REFRESH_TOKEN={}\n", token.refresh_token);
    Ok(token.access_token)
}

/// What a phone holds after logging in: a username and a *reusable* credential.
struct Stored {
    username: String,
    auth_data: Vec<u8>,
}

/// Trade an access token for reusable credentials, by logging in the way librespot does
/// and keeping what the access point hands back.
async fn stored_credentials(token: &str) -> Result<Stored, Error> {
    let session = Session::new(SessionConfig::default(), None);
    session
        .connect(Credentials::with_access_token(token), false)
        .await
        .map_err(|e| format!("access-point login failed: {e}"))?;
    let stored = Stored {
        username: session.username(),
        auth_data: session.auth_data(),
    };
    session.shutdown();
    if stored.auth_data.is_empty() {
        return Err("logged in but got no reusable credentials".into());
    }
    Ok(stored)
}

// ------------------------------------------------------------------- zeroconf

struct ReceiverInfo {
    remote_name: String,
    device_id: String,
    public_key: Vec<u8>,
}

async fn get_info(base: &str) -> Result<ReceiverInfo, Error> {
    let url = format!("{base}/spotify?action=getInfo");
    let body: Value = reqwest::get(&url)
        .await
        .map_err(|e| format!("GET {url}: {e}"))?
        .json()
        .await?;

    let field = |k: &str| -> Result<String, Error> {
        body.get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("getInfo has no {k}: {body}").into())
    };
    Ok(ReceiverInfo {
        remote_name: field("remoteName")?,
        device_id: field("deviceID")?,
        public_key: base64::engine::general_purpose::STANDARD.decode(field("publicKey")?)?,
    })
}

/// Wrap the credentials the way a phone does and post them.
async fn add_user(base: &str, info: &ReceiverInfo, stored: &Stored) -> Result<(), Error> {
    // Inner layer: bound to the receiver's device id, which is why `getInfo` has to be
    // read first rather than assumed.
    let inner = encode_credentials_blob(
        &stored.username,
        &info.device_id,
        AUTH_TYPE_STORED,
        &stored.auth_data,
    )?;

    // Outer layer: ephemeral DH against the key the receiver just published.
    let keys = DhKeys::generate();
    let shared = keys.shared_secret(&info.public_key);
    let mut iv = [0u8; 16];
    getrandom(&mut iv);
    let blob = encrypt_blob(&inner, &shared, &iv)?;

    let b64 = base64::engine::general_purpose::STANDARD;
    let form = [
        ("action", "addUser".to_owned()),
        ("userName", stored.username.clone()),
        ("blob", b64.encode(&blob)),
        ("clientKey", b64.encode(keys.public_key())),
    ];

    let resp = reqwest::Client::new()
        .post(format!("{base}/spotify"))
        .form(&form)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("addUser returned {status}: {text}").into());
    }
    // The receiver answers 200 before the login behind it has even started, so this is
    // not evidence of anything beyond "the blob decrypted".
    Ok(())
}

fn getrandom(buf: &mut [u8]) {
    use rand::RngCore as _;
    rand::thread_rng().fill_bytes(buf);
}

// ------------------------------------------------------------------- web api

async fn api(token: &str, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .request(method, format!("https://api.spotify.com{path}"))
        .bearer_auth(token)
}

async fn api_json(token: &str, path: &str) -> Result<Value, Error> {
    let resp = api(token, reqwest::Method::GET, path).await.send().await?;
    if resp.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(Value::Null);
    }
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("GET {path} returned {status}: {text}").into());
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Poll the account's device list until the receiver shows up.
///
/// Matched on *name*, not id: the id Spotify assigns is its own business, and the name is
/// what a person picks in the picker — which is the thing being tested.
async fn wait_for_device(token: &str, name: &str) -> Result<String, Error> {
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    let mut seen: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        let body = api_json(token, "/v1/me/player/devices").await?;
        seen = body["devices"]
            .as_array()
            .map(|d| {
                d.iter()
                    .filter_map(|x| x["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(dev) = body["devices"]
            .as_array()
            .and_then(|devices| devices.iter().find(|d| d["name"].as_str() == Some(name)))
        {
            if let Some(id) = dev["id"].as_str() {
                return Ok(id.to_owned());
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(format!(
        "\"{name}\" never appeared in the account's devices after {}s.\n\
         The pairing was accepted, so the login behind it failed — check the receiver's log \
         for \"connect session failed\". The usual causes are a non-Premium account and a \
         device id mismatch between getInfo and the session.\n\
         Devices that were visible: {seen:?}",
        DEVICE_TIMEOUT.as_secs()
    )
    .into())
}

#[derive(Clone)]
struct Track {
    uri: String,
    label: String,
}

impl std::fmt::Display for Track {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Two tracks that are actually playable for this account, found at runtime.
///
/// Hardcoding URIs would make the harness fail for licensing reasons in some markets and
/// look like a receiver bug. `market=from_token` asks Spotify for the account's own view.
async fn pick_tracks(token: &str) -> Result<[Track; 2], Error> {
    let body = api_json(
        token,
        "/v1/search?q=year%3A1970-2010&type=track&limit=20&market=from_token",
    )
    .await?;
    let mut found: Vec<Track> = body["tracks"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|t| t["is_playable"].as_bool() != Some(false))
                .filter_map(|t| {
                    Some(Track {
                        uri: t["uri"].as_str()?.to_owned(),
                        label: format!(
                            "{} — {}",
                            t["artists"][0]["name"].as_str().unwrap_or("?"),
                            t["name"].as_str().unwrap_or("?")
                        ),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    found.truncate(2);
    let [a, b] = <[Track; 2]>::try_from(found)
        .map_err(|_| "search returned fewer than two playable tracks")?;
    Ok([a, b])
}

async fn transfer_and_play(token: &str, device_id: &str, track: &Track) -> Result<(), Error> {
    // One call: move the account here *and* start something, so there is no window where
    // the device is active with nothing loaded.
    let resp = api(token, reqwest::Method::PUT, "/v1/me/player")
        .await
        .json(&serde_json::json!({ "device_ids": [device_id], "play": true }))
        .send()
        .await?;
    expect_ok(resp, "transfer").await?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let resp = api(
        token,
        reqwest::Method::PUT,
        &format!("/v1/me/player/play?device_id={device_id}"),
    )
    .await
    .json(&serde_json::json!({ "uris": [track.uri] }))
    .send()
    .await?;
    expect_ok(resp, "play").await
}

async fn queue(token: &str, track: &Track) -> Result<(), Error> {
    let path = format!("/v1/me/player/queue?uri={}", urlencode(&track.uri));
    let resp = api(token, reqwest::Method::POST, &path)
        .await
        .send()
        .await?;
    expect_ok(resp, "queue").await
}

async fn next(token: &str) -> Result<(), Error> {
    let resp = api(token, reqwest::Method::POST, "/v1/me/player/next")
        .await
        .send()
        .await?;
    expect_ok(resp, "next").await
}

async fn expect_ok(resp: reqwest::Response, what: &str) -> Result<(), Error> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default();
    Err(format!("{what} returned {status}: {text}").into())
}

/// Wait until the account reports `uri` as the current track, and playing.
async fn wait_for_playing(token: &str, uri: &str, what: &str) -> Result<(), Error> {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let mut last = String::from("(nothing)");
    while Instant::now() < deadline {
        let body = api_json(token, "/v1/me/player").await?;
        if let Some(current) = body["item"]["uri"].as_str() {
            last = current.to_owned();
            if current == uri && body["is_playing"].as_bool() == Some(true) {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(format!("{what}: expected {uri} to be playing, but it was {last}").into())
}

/// Wait until `uri` shows up in the account's queue.
///
/// Checked rather than assumed: `POST /queue` returning 204 only means the cloud accepted
/// it, and the whole question is whether the *device* took it.
async fn wait_for_queued(token: &str, uri: &str) -> Result<(), Error> {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let mut seen: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        let body = api_json(token, "/v1/me/player/queue").await?;
        seen = body["queue"]
            .as_array()
            .map(|q| {
                q.iter()
                    .filter_map(|t| t["uri"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if seen.iter().any(|u| u == uri) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(format!("{uri} never reached the queue; queue held {seen:?}").into())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}
