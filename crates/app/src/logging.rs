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

/// Bring up the console layer, and the file layer if `log.to_file` and the directory can
/// be created.
///
/// Never fails: logging is not a reason to refuse to boot. A file sink that cannot be
/// opened degrades to console-only with a `warn!` naming the directory and the reason —
/// the one thing the old silently-unwritable-directory behaviour never did (G31).
pub fn init(log: &Log, dirs: &Dirs) {
    let console = tracing_subscriber::fmt::layer().with_filter(
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log.level.clone())),
    );

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
        .with_filter(EnvFilter::new(log.file_level.clone()));
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

    use super::{build_appender, FILE_PREFIX};
    use crate::config::{Log, Rotation};

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
    fn the_default_file_filter_is_not_debug() {
        // The console follows RUST_LOG; the file does not, because an unattended panel
        // logging every mirrored frame fills its own disk.
        let log = Log::default();
        assert_eq!(log.file_level, "info");
    }
}
