//! The receiver platform, measured against **Google's own receiver SDK**.
//!
//! `proto-cast::platform` is a reimplementation of a protocol whose only specification is
//! `cast_receiver.js`. Unit tests against message shapes we believe are right can only
//! restate the belief; this drives the real bundle, in the real browser runtime, against
//! the real server, and asserts the SDK's own `onReady` fired and that a relayed message
//! reached the application layer and was answered.
//!
//! Both SDK generations are exercised, because both are in play: v2 is what YouTube and
//! Plex load, CAF v3 is what the Default Media Receiver loads (#16).
//!
//! Needs `CASTAWAY_ELECTRON` and `CASTAWAY_CAST_RECEIVER_SDK`, both set by the dev shell
//! and by `nix flake check`. Without them the test **fails** rather than passing quietly:
//! a browser test that skips itself is a browser test that never runs, and this is the
//! only thing in the tree that can tell whether the platform protocol is actually right.
//! Set `CASTAWAY_SKIP_BROWSER_TESTS=1` to opt out deliberately.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use proto_cast::platform::{AppIdentity, DeviceCapabilities, IpcFrame};
use proto_cast::platform_actor::{HostEvent, PlatformServer};
use tokio::sync::mpsc;

const MEDIA_NS: &str = "urn:x-cast:com.google.cast.media";

/// What the probe prints.
#[derive(Debug, serde::Deserialize)]
struct ProbeReport {
    ok: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    sdk: String,
    #[serde(default)]
    ready: Option<serde_json::Value>,
    #[serde(default)]
    events: Vec<serde_json::Value>,
    #[serde(default)]
    #[serde(rename = "appMessages")]
    app_messages: Vec<serde_json::Value>,
}

/// The environment the probe needs, or a reason it cannot run.
fn browser_env() -> Result<(String, String, String), String> {
    if std::env::var_os("CASTAWAY_SKIP_BROWSER_TESTS").is_some() {
        return Err("CASTAWAY_SKIP_BROWSER_TESTS is set".into());
    }
    let electron = std::env::var("CASTAWAY_ELECTRON")
        .map_err(|_| "CASTAWAY_ELECTRON is unset; run inside `nix develop`".to_owned())?;
    let sdk = std::env::var("CASTAWAY_CAST_RECEIVER_SDK")
        .map_err(|_| "CASTAWAY_CAST_RECEIVER_SDK is unset; run inside `nix develop`".to_owned())?;
    // Resolved from the source tree, deliberately *not* from `CASTAWAY_BROWSER_APP`.
    // That variable points at the browser host the receiver runs, which under Nix is a
    // store copy of the flake source — so in a shell that has not been re-entered since
    // the last edit it is a stale snapshot, and the test would measure a probe nobody
    // has changed. A test has to exercise the tree it is part of.
    let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../browser-host/cast-platform-probe.js");
    let probe = probe
        .canonicalize()
        .map_err(|e| format!("no probe at {}: {e}", probe.display()))?;
    Ok((electron, sdk, probe.display().to_string()))
}

fn app(app_id: &str, name: &str) -> AppIdentity {
    AppIdentity {
        application_id: app_id.to_owned(),
        application_name: name.to_owned(),
        session_id: "sess-probe".to_owned(),
        launching_sender_id: "sender-probe".to_owned(),
        icon_url: None,
    }
}

/// Start the platform, run the probe against it, and return what the SDK reported.
///
/// The relay is driven from here rather than from the probe: the point is that a message
/// entering the *platform* from a sender comes out at the application, which is only a
/// real claim if this side is the one that sends it.
async fn run_probe(caf: bool) -> ProbeReport {
    let (electron, sdk, probe) = match browser_env() {
        Ok(env) => env,
        Err(reason) => panic!(
            "the receiver-SDK test cannot run: {reason}. This is the only test that \
             measures the platform protocol against the SDK it was written from; \
             letting it skip silently would leave that unmeasured."
        ),
    };

    let (host, task) = PlatformServer::new(DeviceCapabilities::default())
        .with_port(0)
        .bind()
        .await
        .unwrap();
    tokio::spawn(task);

    let (events_tx, mut events) = mpsc::channel(64);
    host.start(app("4F8B3483", "CastVideos"), (0.4, false), events_tx)
        .await
        .unwrap();
    // A sender that connected before the page loaded — the launching sender's own case,
    // and the one the platform has to replay rather than drop.
    host.sender_connected("sender-probe", "probe/1.0").await;

    let port = host.port();
    let child = tokio::process::Command::new(&electron)
        .arg(&probe)
        .arg("--port")
        .arg(port.to_string())
        .arg("--sdk")
        .arg(&sdk)
        .arg("--namespace")
        .arg(MEDIA_NS)
        .args(if caf { vec!["--caf"] } else { vec![] })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawning electron");

    // Relay once the page has identified itself. Sending before `ready` would be dropped
    // by the SDK, which is the behaviour `nothing_is_relayed_to_a_page_that_has_not_come_up`
    // already pins on our side; here the real SDK is the judge.
    let relayed = tokio::spawn({
        let host = host.clone();
        async move {
            let mut saw_ready = false;
            let mut answered = None;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
            loop {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                match tokio::time::timeout(left, events.recv()).await {
                    Ok(Some(HostEvent::Ready(ready))) => {
                        saw_ready = true;
                        assert!(
                            ready.active_namespaces.iter().any(|n| n == MEDIA_NS),
                            "the SDK did not declare the namespace the probe opened: {:?}",
                            ready.active_namespaces
                        );
                        host.to_page(MEDIA_NS, "sender-probe", r#"{"type":"PROBE_PING"}"#)
                            .await;
                    }
                    Ok(Some(HostEvent::ToSender { data, .. })) => {
                        answered = Some(data);
                        break;
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(_) => break,
                }
            }
            (saw_ready, answered)
        }
    });

    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("the probe did not exit")
        .expect("reading the probe's output");
    let (saw_ready, answered) = relayed.await.unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| {
            panic!("the probe printed no report.\nstdout:\n{stdout}\nstderr:\n{stderr}")
        });
    let report: ProbeReport = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("unreadable probe report: {e}\n{line}"));

    assert!(
        saw_ready,
        "the platform never saw the page identify itself{}.\nprobe: {report:?}\nstderr:\n{stderr}",
        report
            .reason
            .as_deref()
            .map(|r| format!(" (the page reported: {r})"))
            .unwrap_or_default()
    );
    // The v2 probe answers on the bus; CAF's custom-message listener does not, so the
    // return path is only asserted where the SDK offers one.
    if !caf {
        let answer = answered.unwrap_or_else(|| {
            panic!("the application's answer never came back out.\nprobe: {report:?}")
        });
        assert!(
            answer.contains("PROBE_ACK"),
            "the answer was not the application's: {answer}"
        );
    }
    report
}

/// The v2 SDK — what YouTube and Plex load — comes up against our platform, learns the
/// session we told it about, sees the sender that connected before it finished loading,
/// and exchanges a message with that sender through us.
#[tokio::test(flavor = "multi_thread")]
async fn the_v2_receiver_sdk_comes_up_against_this_platform() {
    let report = run_probe(false).await;
    assert!(report.ok, "the SDK never reached ready: {report:?}");
    assert_eq!(report.sdk, "v2");

    let ready = report.ready.expect("a ready event");
    assert_eq!(
        ready["applicationId"], "4F8B3483",
        "the SDK read back a different application than the one we host: {ready}"
    );
    assert_eq!(ready["sessionId"], "sess-probe");
    assert_eq!(ready["launchingSenderId"], "sender-probe");

    // The replay: a sender that connected while the page was still loading has to reach
    // the application anyway, or an app never sees its own launcher.
    assert!(
        report
            .events
            .iter()
            .any(|e| { e["type"] == "senderconnected" && e["senderId"] == "sender-probe" }),
        "the launching sender never reached the application: {:?}",
        report.events
    );

    // And the device volume, which an app draws its own slider from.
    let volume = report
        .events
        .iter()
        .find(|e| e["type"] == "volumechanged")
        .unwrap_or_else(|| panic!("no volume reached the application: {:?}", report.events));
    assert!(
        (volume["level"].as_f64().unwrap_or_default() - 0.4).abs() < 1e-6,
        "the application was told the wrong volume: {volume}"
    );

    assert!(
        report.app_messages.iter().any(|m| m["data"]
            .as_str()
            .unwrap_or_default()
            .contains("PROBE_PING")),
        "the relayed message never reached the application: {:?}",
        report.app_messages
    );
}

/// CAF v3 — what the Default Media Receiver loads — over the same platform, unchanged.
/// This is the claim that one implementation serves both SDK generations.
#[tokio::test(flavor = "multi_thread")]
async fn the_caf_v3_receiver_sdk_comes_up_against_the_same_platform() {
    let report = run_probe(true).await;
    assert!(
        report.ok,
        "CAF v3 never reached ready against the same platform: {report:?}"
    );
    assert_eq!(report.sdk, "caf-v3");
    assert!(
        report.app_messages.iter().any(|m| m["data"]
            .as_str()
            .unwrap_or_default()
            .contains("PROBE_PING")),
        "the relayed message never reached the CAF application: {:?}",
        report.app_messages
    );
}

/// The frame the SDK's own validator accepts. Cheap, and it fails loudly if the shape
/// ever drifts from what `cast.receiver.IpcChannel.X` checks for:
/// `a && a.namespace && a.senderId && a.data`.
#[test]
fn the_frame_carries_every_key_the_sdks_validator_requires() {
    let text = IpcFrame::app(MEDIA_NS, "sender-1", r#"{"type":"LOAD"}"#)
        .encode()
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    for key in ["namespace", "senderId", "data"] {
        assert!(
            value.get(key).and_then(serde_json::Value::as_str).is_some(),
            "{key} must be a non-empty string for the SDK to accept the frame: {text}"
        );
    }
}
