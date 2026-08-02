//! Logging: the console stream an operator watches, plus a rotated file on disk.
//!
//! The two are separate sinks with separate filters, on purpose. The console filter is
//! `RUST_LOG` and belongs to whoever is looking at the box right now — turning it up to
//! `debug` is a debugging act with an audience. The file filter belongs to the machine:
//! it runs unattended for weeks, and a panel that quietly fills its own disk with
//! frame-by-frame mirroring spew is a worse failure than the one being diagnosed. So the
//! file stays at `info` unless `[log] file_level` says otherwise, whatever `RUST_LOG`
//! does.
//!
//! Both sinks sit on top of [`NOISE_FLOOR`], which holds the libraries that log per frame
//! or per packet down to a level where they still report trouble. That is what makes
//! `RUST_LOG=debug` mean "castaway at debug" rather than a thousand swapchain lines a
//! second.
//!
//! Writes to the file are synchronous rather than going through
//! `tracing_appender::non_blocking`. The interesting log lines are the last ones before
//! something died, the release profile is `panic = "abort"`, and an aborting process runs
//! no destructors — so a background writer's buffered tail is exactly the part that would
//! be lost. Local-file writes at `info` are cheap enough to pay for that.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use castaway_paths::{Dirs, Origin};
use tracing_appender::rolling::{RollingFileAppender, Rotation as AppenderRotation};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _};

use crate::config::{Log, Rotation};

/// Stem of every log file. `tracing-appender` appends the rotation's date suffix, so
/// these land as `castaway.log.2026-07-28`.
const FILE_PREFIX: &str = "castaway";
/// Suffix, kept after the date so the files sort by name and still look like logs.
const FILE_SUFFIX: &str = "log";

/// Libraries that log per frame, per packet or per poll, held down to a level where they
/// still report trouble.
///
/// A floor rather than a blanket, and it is applied *under* whatever `RUST_LOG` or
/// `[log] level` asks for, so `RUST_LOG=debug` means "castaway at debug" rather than "a
/// thousand lines a second from the swapchain". Naming a target explicitly still wins —
/// `RUST_LOG=info,wgpu_core=debug` is how you get back what this hides — because a
/// directive for the same target replaces ours rather than adding to it.
///
/// The graphics stack is the reason this exists and is the reason it says `warn` rather
/// than `info`: `wgpu_core` logs `Device::maintain: waiting for submission index` at
/// **`INFO`**, once per presented frame. At 60 fps on a panel that is up for a week, that
/// is not a debug-only problem — it would fill the on-disk log at the default settings.
const NOISE_FLOOR: &[&str] = &[
    // Per frame, and partly at INFO.
    "wgpu=warn",
    "wgpu_core=warn",
    "wgpu_hal=warn",
    "naga=warn",
    // Per input event / per event-loop turn.
    "winit=warn",
    "calloop=warn",
    "smithay_client_toolkit=warn",
    "sctk_adwaita=warn",
    // Per poll.
    "mio=warn",
    "polling=warn",
    "tokio_util=warn",
    "want=warn",
    // Per record / per HTTP frame. `info` rather than `warn`: a TLS handshake failure
    // against a sender is something we want to see without being asked.
    "rustls=info",
    "hyper=info",
    "h2=info",
    "ureq=info",
    // Per audio packet.
    "symphonia=info",
    // Per mDNS query and per response — and this one is not theoretical: with only DLNA
    // enabled and nothing on the network talking to us, `mdns_sd` was 2168 of the 2179
    // console lines in the first five seconds at `debug`. `info` rather than `warn`
    // because service registration and conflicts are worth seeing; the per-packet
    // chatter underneath it is what buries them.
    "mdns_sd=info",
    // Per *packet*, and five lines of it: the WebRTC stack logs every write as it falls
    // through the handler chain — srtp, datachannel, sctp, dtls, ice — so one connected
    // remote peer (#18) at 30 fps is about 150 lines a second, before its own media
    // traffic is counted. `info` rather than `warn` because ICE state changes and the
    // selected candidate pair are worth seeing unasked: they are the first thing to look
    // at when a peer connects and shows nothing.
    "rtc=info",
    "rtc_ice=info",
    "rtc_dtls=info",
    "rtc_srtp=info",
    "rtc_sctp=info",
    "rtc_shared=info",
    "webrtc=info",
];

/// Compose the noise floor with what the operator asked for.
///
/// Order is the mechanism: `EnvFilter` keeps one directive per target, last one added
/// wins, and matching then prefers the most specific target. So the floor goes first
/// (anything the operator names again replaces it) and their directives go second.
fn filter(requested: &str) -> EnvFilter {
    let composed = format!("{},{requested}", NOISE_FLOOR.join(","));
    tracing_subscriber::filter::EnvFilter::builder()
        .parse(&composed)
        // A malformed `RUST_LOG` should not cost the floor as well as itself. Fall back
        // to the floor alone plus `info` rather than to an unfiltered firehose.
        .unwrap_or_else(|_| {
            eprintln!("castaway: ignoring unparseable log filter {requested:?}");
            EnvFilter::new(format!("{},info", NOISE_FLOOR.join(",")))
        })
}

/// What the console filter should be: `rust_log` (the `RUST_LOG` value) if it says
/// anything, else `[log] level`.
///
/// Takes the value rather than reading the environment so it is testable without
/// `set_var`, which is racy across threads and `unsafe` from Rust 2024.
fn requested_console_filter(log: &Log, rust_log: Option<String>) -> String {
    rust_log
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| log.level.clone())
}

/// Bring up the console layer, and the file layer if `log.to_file` and the directory can
/// be created.
///
/// Never fails: logging is not a reason to refuse to boot. A file sink that cannot be
/// opened degrades to console-only with a `warn!` naming the directory and the reason —
/// the one thing the old silently-unwritable-directory behaviour never did (G31).
pub fn init(log: &Log, dirs: &Dirs) {
    let console = tracing_subscriber::fmt::layer().with_filter(filter(&requested_console_filter(
        log,
        std::env::var(EnvFilter::DEFAULT_ENV).ok(),
    )));

    let file = if log.to_file {
        match file_layer(log, dirs) {
            Ok((layer, path)) => Some((layer, path)),
            Err(err) => {
                // No subscriber is installed yet, so this cannot be a `warn!`.
                eprintln!("castaway: file logging disabled: {err:#}");
                None
            }
        }
    } else {
        None
    };

    let (file_layer, file_dir) = match file {
        Some((layer, path)) => (Some(layer), Some(path)),
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(console)
        .with(file_layer)
        .init();

    if let Some(dir) = file_dir {
        tracing::info!(
            directory = %dir.display(),
            rotation = ?log.rotation,
            keep = log.max_files,
            filter = %log.file_level,
            "logging to disk"
        );
    }
    if dirs.origin() == Origin::Fallback {
        tracing::warn!(
            state = %dirs.state().display(),
            "no HOME/LOCALAPPDATA in the environment: state, cache and logs are relative to \
             the working directory and will not be found again from elsewhere"
        );
    }
}

/// Build the rolling file layer, returning it with the directory it writes to.
fn file_layer<S>(
    log: &Log,
    dirs: &Dirs,
) -> anyhow::Result<(
    impl tracing_subscriber::Layer<S> + Send + Sync + 'static,
    PathBuf,
)>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let dir = log
        .directory
        .clone()
        .unwrap_or_else(|| dirs.logs().to_path_buf());
    castaway_paths::ensure(&dir).with_context(|| format!("log directory {}", dir.display()))?;

    let appender = build_appender(log, &dir)?;
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(appender)
        // Nothing reads this file through a terminal, and escape codes make `grep`
        // miss lines whose level marker they wrap.
        .with_ansi(false)
        .with_filter(filter(&log.file_level));
    Ok((layer, dir))
}

fn build_appender(log: &Log, dir: &Path) -> anyhow::Result<RollingFileAppender> {
    let mut builder = RollingFileAppender::builder()
        .rotation(rotation(log.rotation))
        .filename_prefix(FILE_PREFIX)
        .filename_suffix(FILE_SUFFIX);
    // Pruning happens *at* a rotation, so it is meaningless without one — and
    // `Rotation::NEVER` writes one file that grows forever, which the config
    // documents rather than pretends otherwise about.
    if log.rotation != Rotation::Never {
        builder = builder.max_log_files(usize::from(log.max_files.max(1)));
    }
    builder
        .build(dir)
        .with_context(|| format!("opening a log file in {}", dir.display()))
}

const fn rotation(rotation: Rotation) -> AppenderRotation {
    match rotation {
        Rotation::Minutely => AppenderRotation::MINUTELY,
        Rotation::Hourly => AppenderRotation::HOURLY,
        Rotation::Daily => AppenderRotation::DAILY,
        Rotation::Never => AppenderRotation::NEVER,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::Layer as _;

    use super::{build_appender, filter, requested_console_filter, FILE_PREFIX};
    use crate::config::{Log, Rotation};

    /// A writer that keeps what was written, so a test can assert on which events a
    /// filter actually let through rather than on the filter's own string.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for Captured {
        type Writer = Self;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `body` under a subscriber filtered by `requested`, returning what was logged.
    fn under(requested: &str, body: impl FnOnce()) -> String {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(captured.clone())
                .with_filter(filter(requested)),
        );
        tracing::subscriber::with_default(subscriber, body);
        captured.text()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("castaway-log-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_appender_writes_a_prefixed_file() {
        let dir = scratch("writes");
        let mut appender = build_appender(&Log::default(), &dir).unwrap();
        writeln!(appender, "hello").unwrap();
        appender.flush().unwrap();

        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with(FILE_PREFIX), "{names:?}");
        // Dated, so a restart appends to today's file rather than truncating it and
        // yesterday's evidence survives the reboot that followed it.
        assert!(names[0].ends_with(".log"), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_is_not_requested_when_nothing_ever_rotates() {
        // `max_log_files(0)` panics inside tracing-appender, and NEVER + a retention
        // count is a config an operator can plausibly write. Neither may reach it.
        let dir = scratch("never");
        let log = Log {
            rotation: Rotation::Never,
            max_files: 0,
            ..Log::default()
        };
        assert!(build_appender(&log, &dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_retention_count_still_keeps_one_file() {
        let dir = scratch("zero");
        let log = Log {
            max_files: 0,
            ..Log::default()
        };
        assert!(build_appender(&log, &dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_means_castaway_at_debug_not_the_swapchain_at_debug() {
        // The reported symptom: `RUST_LOG=debug` and the console fills with per-frame
        // wgpu lines. Ours come through; theirs do not, at either level — `wgpu_core`
        // logs `Device::maintain` at INFO once per presented frame, so holding it at
        // `debug` would not have been enough.
        let logged = under("debug", || {
            tracing::debug!(target: "castaway", "ours");
            tracing::debug!(target: "wgpu_core::present", "Presented. End of Frame");
            tracing::info!(target: "wgpu_core::device::resource", "Device::maintain");
            tracing::debug!(target: "naga::front", "translating");
            tracing::debug!(target: "winit::platform_impl", "event");
            tracing::debug!(target: "mdns_sd::service_daemon", "handle_query");
        });
        assert!(logged.contains("ours"), "{logged}");
        assert!(!logged.contains("End of Frame"), "{logged}");
        assert!(!logged.contains("Device::maintain"), "{logged}");
        assert!(!logged.contains("translating"), "{logged}");
        assert!(!logged.contains("event"), "{logged}");
        assert!(!logged.contains("handle_query"), "{logged}");
    }

    #[test]
    fn the_floor_still_reports_trouble() {
        // Suppressing noise must not suppress the thing you would want paged about. A
        // GPU that stops working says so at `warn`/`error`, and that still arrives even
        // at the default `info`.
        let logged = under("info", || {
            tracing::warn!(target: "wgpu_core::device", "device lost");
            tracing::error!(target: "wgpu_hal::vulkan", "VK_ERROR_DEVICE_LOST");
        });
        assert!(logged.contains("device lost"), "{logged}");
        assert!(logged.contains("VK_ERROR_DEVICE_LOST"), "{logged}");
    }

    #[test]
    fn the_webrtc_stack_does_not_narrate_every_packet() {
        // One connected remote peer (#18) logs five DEBUG lines per packet as the write
        // falls through the handler chain, so at 30 fps that is ~150 lines a second before
        // any of its own traffic. `RUST_LOG=debug` for our own crates must not turn the
        // panel's log into that.
        let logged = under("debug", || {
            // The five handlers one write actually falls through, verbatim.
            tracing::debug!(target: "rtc::peer_connection::handler::srtp", "bypass write");
            tracing::debug!(target: "rtc::peer_connection::handler::datachannel", "bypass write");
            tracing::debug!(target: "rtc::peer_connection::handler::sctp", "bypass write");
            tracing::debug!(target: "rtc::peer_connection::handler::dtls", "bypass write");
            tracing::debug!(target: "rtc::peer_connection::handler::ice", "bypass write");
            tracing::debug!(target: "webrtc::peer_connection::driver", "bypass write");
            tracing::debug!(target: "rtc_srtp::session", "bypass write");
            tracing::debug!(target: "pipeline::remote::service", "ours survives");
        });
        assert!(!logged.contains("bypass write"), "{logged}");
        assert!(logged.contains("ours survives"), "{logged}");
    }

    #[test]
    fn what_a_silent_peer_needs_is_still_logged_unasked() {
        // The reason the floor is `info` and not `warn`: when a peer connects and shows
        // nothing, the ICE state and the selected pair are the first things to look at,
        // and nobody thinks to turn them on beforehand.
        let logged = under("info", || {
            tracing::info!(target: "rtc_ice::agent", "Setting new connection state: Connected");
            tracing::warn!(target: "webrtc::peer_connection::driver", "Failed to send RTP");
        });
        assert!(logged.contains("Connected"), "{logged}");
        assert!(logged.contains("Failed to send RTP"), "{logged}");
    }

    #[test]
    fn naming_a_muted_target_explicitly_gets_it_back() {
        // The escape hatch, and the reason the floor is composed *before* the operator's
        // directives: someone actually debugging the renderer must be able to ask for it.
        let logged = under("info,wgpu_core=debug", || {
            tracing::debug!(target: "wgpu_core::present", "Presented. End of Frame");
            tracing::debug!(target: "naga::front", "still muted");
        });
        assert!(logged.contains("End of Frame"), "{logged}");
        assert!(!logged.contains("still muted"), "{logged}");
    }

    #[test]
    fn an_unparseable_filter_falls_back_to_the_floor_rather_than_to_everything() {
        // A typo in RUST_LOG should not silently become a firehose — which is what
        // `EnvFilter`'s own lenient parse would leave, since it drops bad directives and
        // keeps going.
        let logged = under("=not a level=", || {
            tracing::info!(target: "castaway", "ours");
            tracing::info!(target: "wgpu_core::device::resource", "Device::maintain");
        });
        assert!(logged.contains("ours"), "{logged}");
        assert!(!logged.contains("Device::maintain"), "{logged}");
    }

    #[test]
    fn rust_log_wins_over_the_configured_level_but_only_when_it_says_something() {
        let log = Log {
            level: "warn".to_owned(),
            ..Log::default()
        };
        assert_eq!(
            requested_console_filter(&log, Some("debug".into())),
            "debug"
        );
        assert_eq!(requested_console_filter(&log, None), "warn");
        // systemd hands `Environment=RUST_LOG=` through as an empty string, which
        // `EnvFilter` reads as "log nothing" rather than as "unset".
        assert_eq!(requested_console_filter(&log, Some(String::new())), "warn");
        assert_eq!(requested_console_filter(&log, Some("  ".into())), "warn");
    }

    #[test]
    fn the_default_file_filter_is_not_debug() {
        // The console follows RUST_LOG; the file does not, because an unattended panel
        // logging every mirrored frame fills its own disk.
        let log = Log::default();
        assert_eq!(log.file_level, "info");
    }
}
