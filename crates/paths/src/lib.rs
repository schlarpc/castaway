//! Where castaway keeps its files.
//!
//! One place that answers "which directory?" for both platforms the receiver runs on,
//! because the answer used to be given twice, differently, and only for Linux: the
//! filter-list cache resolved `XDG_CACHE_HOME` on its own (falling back to the temp
//! directory), Bluetooth link keys resolved `XDG_STATE_HOME` on its own (falling back to
//! the working directory), and the GameStream pairing store was a hardcoded
//! `/var/lib/castaway/gamestream`. On Windows all three landed somewhere wrong, and
//! every one of those failures is silent by design — a missing cache is not worth
//! refusing to boot over (G31).
//!
//! Two properties this crate is built for:
//!
//! - **Both layouts compile and test everywhere.** The platform seam is [`Layout`], a
//!   value, not a `cfg` scattered through the resolution code. [`Layout::HOST`] is the
//!   only `cfg`, so Linux CI exercises the Windows layout too (ground rule 5).
//! - **Resolution is pure.** [`Dirs::resolve`] reads an [`Environment`], not the process
//!   environment, and touches no disk. Tests hand it a map instead of mutating global
//!   state (ground rule 3). [`ensure`] is the crate's only I/O.
//!
//! A resolution that found nothing to work from does not quietly pick somewhere: it is
//! marked [`Origin::Fallback`] so the caller can say so out loud.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use thiserror::Error;

/// The directory name castaway claims inside whichever base the platform gives it.
pub const APP_DIR: &str = "castaway";

/// The relative base used when the environment names no home at all — a bare `cargo run`
/// with `HOME` unset, or a service started with a scrubbed environment.
const FALLBACK_BASE: &str = ".castaway";

/// Anything that can answer "what is this environment variable?".
///
/// The seam exists so [`Dirs::resolve`] is a pure function: tests pass a fixed map rather
/// than mutating the process environment, which is both racy under a parallel test runner
/// and `unsafe` since Rust 2024.
pub trait Environment {
    /// The value of `key`, or `None` if it is unset.
    fn var(&self, key: &str) -> Option<OsString>;
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Environment for ProcessEnv {
    fn var(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
}

/// Which per-user directory convention to follow.
///
/// A value rather than a `cfg`, so the branch that will run on the deploy box is
/// reachable from the machine the code is written on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// freedesktop.org's basedir spec: `XDG_*_HOME` with `$HOME`-relative defaults.
    Xdg,
    /// Windows: everything under `%LOCALAPPDATA%\castaway\`.
    ///
    /// Local rather than roaming (`%APPDATA%`) deliberately — a cast receiver's state is
    /// a link-key database, a pairing certificate and a filter-list cache. All three are
    /// this-machine facts, and roaming them onto a domain profile would copy megabytes of
    /// cache between boxes to no end.
    LocalAppData,
}

impl Layout {
    /// The convention this build's target platform uses. The crate's only `cfg`.
    pub const HOST: Self = if cfg!(windows) {
        Self::LocalAppData
    } else {
        Self::Xdg
    };

    /// Is `path` absolute *under this layout*?
    ///
    /// Ours rather than [`Path::is_absolute`], which answers for the host: on Linux CI
    /// `C:\\Users\\kiosk` is "relative", so borrowing the host's answer would make the
    /// Windows branch untestable from the machine it is written on — and would decide a
    /// deploy-target question with a build-host rule.
    fn is_absolute(self, path: &Path) -> bool {
        match self {
            Self::Xdg => path.has_root(),
            Self::LocalAppData => {
                let bytes = path.as_os_str().as_encoded_bytes();
                match bytes {
                    // UNC or verbatim (`\\server\share`, `\\?\C:\...`).
                    [b'\\' | b'/', b'\\' | b'/', ..] => true,
                    // A drive *and* a separator. `C:relative` names the drive's current
                    // directory, which is a per-process thing we do not set — Windows
                    // itself calls that relative, and so do we.
                    [drive, b':', b'\\' | b'/', ..] => drive.is_ascii_alphabetic(),
                    _ => false,
                }
            }
        }
    }
}

/// Whether the resolved directories came from the environment or from the last resort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The environment named a base directory, and these paths are under it.
    Environment,
    /// Nothing in the environment named one, so these are relative to the working
    /// directory. State written here does not survive a restart *somewhere else*, which
    /// for link keys and GameStream pairings means silently re-pairing — worth a log line
    /// (see [`Dirs::origin`]).
    Fallback,
}

/// The set of directories castaway uses, resolved once for one [`Layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    config: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    logs: PathBuf,
    origin: Origin,
}

impl Dirs {
    /// Resolve `layout` against `env`. Pure: no disk, no process environment.
    #[must_use]
    pub fn resolve(layout: Layout, env: &impl Environment) -> Self {
        match layout {
            Layout::Xdg => Self::xdg(env),
            Layout::LocalAppData => Self::local_app_data(env),
        }
    }

    /// XDG: each kind has its own base, so an operator who has already split cache off
    /// onto a tmpfs keeps that split.
    fn xdg(env: &impl Environment) -> Self {
        let home = absolute(Layout::Xdg, env, "HOME");
        let base = |var: &str, rel: &str| {
            absolute(Layout::Xdg, env, var)
                .or_else(|| home.as_ref().map(|h| h.join(rel)))
                .map(|b| b.join(APP_DIR))
        };
        let config = base("XDG_CONFIG_HOME", ".config");
        let state = base("XDG_STATE_HOME", ".local/state");
        let cache = base("XDG_CACHE_HOME", ".cache");
        match (config, state, cache) {
            (Some(config), Some(state), Some(cache)) => Self {
                config,
                // XDG has no log directory. State is the right home for logs under it —
                // the same choice systemd makes with `LogsDirectory` vs `StateDirectory`,
                // and it keeps a single directory to persist on the deploy box.
                logs: state.join("logs"),
                state,
                cache,
                origin: Origin::Environment,
            },
            _ => Self::fallback(),
        }
    }

    /// Windows: one application directory with the kinds as subdirectories. There is no
    /// per-kind override to honour, so the split is ours to make and it may as well be
    /// the one an operator can read off a directory listing.
    fn local_app_data(env: &impl Environment) -> Self {
        let base = absolute(Layout::LocalAppData, env, "LOCALAPPDATA")
            .or_else(|| {
                absolute(Layout::LocalAppData, env, "USERPROFILE")
                    .map(|p| p.join("AppData").join("Local"))
            })
            // Roaming is a poor home for this (see `Layout::LocalAppData`), but a
            // profile with `APPDATA` and no `LOCALAPPDATA` is still a real profile, and
            // a writable wrong-ish directory beats the working directory.
            .or_else(|| absolute(Layout::LocalAppData, env, "APPDATA"));
        base.map_or_else(Self::fallback, |base| {
            let root = base.join(APP_DIR);
            Self {
                config: root.join("config"),
                state: root.join("state"),
                cache: root.join("cache"),
                logs: root.join("logs"),
                origin: Origin::Environment,
            }
        })
    }

    fn fallback() -> Self {
        let root = PathBuf::from(FALLBACK_BASE);
        Self {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
            logs: root.join("logs"),
            origin: Origin::Fallback,
        }
    }

    /// Operator-edited files. castaway itself never writes here.
    #[must_use]
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// State a running receiver writes and needs back after a restart: Bluetooth link
    /// keys, the GameStream client certificate and its per-host pairings.
    #[must_use]
    pub fn state(&self) -> &Path {
        &self.state
    }

    /// Files that may be deleted at any time at the cost of a refetch: filter lists, the
    /// browser profile.
    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// Rotated log files.
    #[must_use]
    pub fn logs(&self) -> &Path {
        &self.logs
    }

    /// Where these came from. [`Origin::Fallback`] means the environment named nothing
    /// and these paths are relative to the working directory — say so rather than
    /// discovering it later as state that never persisted.
    #[must_use]
    pub const fn origin(&self) -> Origin {
        self.origin
    }
}

/// The directories for this process, under this platform's [`Layout`], resolved once.
#[must_use]
pub fn host() -> &'static Dirs {
    static DIRS: OnceLock<Dirs> = OnceLock::new();
    DIRS.get_or_init(|| Dirs::resolve(Layout::HOST, &ProcessEnv))
}

/// Read an environment variable, ignoring a relative value.
///
/// The XDG spec requires this ("If an implementation encounters a relative path it must
/// be considered invalid"), and the same reasoning applies to `%LOCALAPPDATA%`: a
/// relative base resolves against a working directory we do not control, so honouring it
/// would put state somewhere that moves.
fn absolute(layout: Layout, env: &impl Environment, key: &str) -> Option<PathBuf> {
    let value = PathBuf::from(env.var(key)?);
    layout.is_absolute(&value).then_some(value)
}

/// Errors from the one thing in this crate that touches the disk.
#[derive(Debug, Error)]
pub enum PathError {
    /// The directory did not exist and could not be created.
    #[error("creating directory {path}")]
    Create {
        /// The directory we tried to create.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
}

/// Create `dir` and its parents if they are not already there, returning it.
///
/// # Errors
/// [`PathError::Create`] if the directory does not exist and cannot be created — which
/// is the interesting case: an unwritable state directory is exactly the G31 failure,
/// and a caller that wants to carry on regardless should say so by discarding this
/// explicitly rather than by never having been told.
pub fn ensure(dir: &Path) -> Result<&Path, PathError> {
    std::fs::create_dir_all(dir).map_err(|source| PathError::Create {
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{Dirs, Environment, Layout, Origin, ProcessEnv};

    /// `Path::new`, short enough to read an assertion through.
    fn p(s: &str) -> &std::path::Path {
        std::path::Path::new(s)
    }

    /// A fixed environment. Not the process's — resolution has to be testable for a
    /// platform we are not running on, and for variables we must not set globally.
    struct Fixed(HashMap<&'static str, &'static str>);

    impl Fixed {
        fn new(vars: &[(&'static str, &'static str)]) -> Self {
            Self(vars.iter().copied().collect())
        }
    }

    impl Environment for Fixed {
        fn var(&self, key: &str) -> Option<std::ffi::OsString> {
            self.0.get(key).map(Into::into)
        }
    }

    #[test]
    fn xdg_prefers_the_explicit_variables_over_home() {
        let env = Fixed::new(&[
            ("HOME", "/home/kiosk"),
            ("XDG_STATE_HOME", "/var/lib/castaway-state"),
            ("XDG_CACHE_HOME", "/var/cache/castaway-cache"),
            ("XDG_CONFIG_HOME", "/etc/xdg-castaway"),
        ]);
        let dirs = Dirs::resolve(Layout::Xdg, &env);
        assert_eq!(dirs.state(), p("/var/lib/castaway-state/castaway"));
        assert_eq!(dirs.cache(), p("/var/cache/castaway-cache/castaway"));
        assert_eq!(dirs.config(), p("/etc/xdg-castaway/castaway"));
        assert_eq!(dirs.origin(), Origin::Environment);
    }

    #[test]
    fn xdg_falls_back_to_the_spec_defaults_under_home() {
        let env = Fixed::new(&[("HOME", "/home/kiosk")]);
        let dirs = Dirs::resolve(Layout::Xdg, &env);
        assert_eq!(dirs.state(), p("/home/kiosk/.local/state/castaway"));
        assert_eq!(dirs.cache(), p("/home/kiosk/.cache/castaway"));
        assert_eq!(dirs.config(), p("/home/kiosk/.config/castaway"));
        assert_eq!(dirs.origin(), Origin::Environment);
    }

    #[test]
    fn logs_live_under_state_on_xdg() {
        // XDG names no log directory, and this is the one path both platforms have to
        // agree exists — the file appender is configured from it unconditionally.
        let env = Fixed::new(&[
            ("XDG_STATE_HOME", "/var/lib/castaway-state"),
            ("HOME", "/h"),
        ]);
        let dirs = Dirs::resolve(Layout::Xdg, &env);
        assert_eq!(dirs.logs(), dirs.state().join("logs"));
    }

    #[test]
    fn a_relative_xdg_value_is_ignored_rather_than_honoured() {
        // Required by the basedir spec, and it matters here: a relative base resolves
        // against a working directory that is `/var/lib/castaway` under the NixOS unit
        // and the checkout under `cargo run`, so state would move between the two.
        let env = Fixed::new(&[
            ("HOME", "/home/kiosk"),
            ("XDG_STATE_HOME", "relative/state"),
        ]);
        let dirs = Dirs::resolve(Layout::Xdg, &env);
        assert_eq!(dirs.state(), p("/home/kiosk/.local/state/castaway"));
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        // systemd hands a unit `Environment=FOO=` as an empty string rather than as an
        // absent variable, and an empty base would resolve to `/castaway`.
        let env = Fixed::new(&[("HOME", "/home/kiosk"), ("XDG_CACHE_HOME", "")]);
        let dirs = Dirs::resolve(Layout::Xdg, &env);
        assert_eq!(dirs.cache(), p("/home/kiosk/.cache/castaway"));
    }

    #[test]
    fn windows_puts_every_kind_under_local_appdata() {
        // Runs on Linux CI: the deploy target's layout is a value, not a cfg.
        let env = Fixed::new(&[
            ("LOCALAPPDATA", r"C:\Users\kiosk\AppData\Local"),
            ("APPDATA", r"C:\Users\kiosk\AppData\Roaming"),
        ]);
        let dirs = Dirs::resolve(Layout::LocalAppData, &env);
        let root = p(r"C:\Users\kiosk\AppData\Local").join("castaway");
        assert_eq!(dirs.state(), root.join("state"));
        assert_eq!(dirs.cache(), root.join("cache"));
        assert_eq!(dirs.logs(), root.join("logs"));
        assert_eq!(dirs.config(), root.join("config"));
        assert_eq!(dirs.origin(), Origin::Environment);
    }

    #[test]
    fn windows_reconstructs_the_base_from_the_profile_when_it_has_to() {
        let env = Fixed::new(&[("USERPROFILE", r"C:\Users\kiosk")]);
        let dirs = Dirs::resolve(Layout::LocalAppData, &env);
        assert!(
            dirs.state()
                .starts_with(p(r"C:\Users\kiosk").join("AppData").join("Local")),
            "{}",
            dirs.state().display()
        );
        assert_eq!(dirs.origin(), Origin::Environment);
    }

    #[test]
    fn windows_absoluteness_is_judged_by_windows_rules_not_the_build_host() {
        // Each of these is decided by `Layout`, not by `Path::is_absolute` — which on
        // Linux CI calls every one of the first three relative, and on Windows calls
        // `C:relative` relative too (it names the drive's current directory, which is a
        // per-process thing we never set).
        for absolute in [
            r"C:\Users\kiosk",
            "C:/Users/kiosk",
            r"\\fileserver\profiles",
        ] {
            let env = Fixed::new(&[("LOCALAPPDATA", absolute)]);
            let dirs = Dirs::resolve(Layout::LocalAppData, &env);
            assert_eq!(dirs.origin(), Origin::Environment, "{absolute}");
            assert!(dirs.state().starts_with(p(absolute)), "{absolute}");
        }
        for relative in [r"C:relative\local", r"AppData\Local", "4:/not-a-drive"] {
            let env = Fixed::new(&[("LOCALAPPDATA", relative)]);
            let dirs = Dirs::resolve(Layout::LocalAppData, &env);
            assert_eq!(dirs.origin(), Origin::Fallback, "{relative}");
        }
    }

    #[test]
    fn nothing_in_the_environment_is_reported_rather_than_guessed() {
        // The point of Origin: `/.local/state/castaway` under a DynamicUser whose home
        // is `/` is unwritable, and every write there is swallowed. A caller that knows
        // it is on the fallback can say so once at startup instead.
        for layout in [Layout::Xdg, Layout::LocalAppData] {
            let dirs = Dirs::resolve(layout, &Fixed::new(&[]));
            assert_eq!(dirs.origin(), Origin::Fallback, "{layout:?}");
            assert!(dirs.state().is_relative(), "{layout:?}");
        }
    }

    #[test]
    fn the_host_layout_resolves_without_panicking() {
        let dirs = Dirs::resolve(Layout::HOST, &ProcessEnv);
        assert!(dirs.logs().starts_with(dirs.state()) || dirs.logs().is_absolute());
    }
}
