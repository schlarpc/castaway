//! The install tree: side-by-side versions, and the two pointers that name them.
//!
//! Where [`crate::Dirs`] answers "where does this receiver keep its state?", this answers
//! "which copies of the receiver are installed, and which one runs?". It is the layout
//! the launcher owns (#342) and the updater writes into (#345), and it lives here rather
//! than in either of them because a layout two programs disagree about is a panel that
//! does not come back up.
//!
//! ```text
//! <root>/
//!   launcher.exe          stable; the box's scheduled task points here and nowhere else
//!   current.txt           the version to run — written atomically, and written last
//!   previous.txt          the last known-healthy version, the rollback target
//!   hold                  present ⇒ a human is driving; the updater stands down
//!   castaway.log          the receiver's stdout, shared across versions
//!   versions/<sha>/       a release tree, extracted; `.healthy` appears once it is up
//! ```
//!
//! Two properties this is built for, both of them the reason the pattern (Chrome's,
//! Squirrel's, Velopack's) exists at all:
//!
//! - **Nothing is ever overwritten in place.** Windows permits renaming a running image
//!   and forbids only deleting or overwriting it, so a new version is a new directory and
//!   the file-locking problem never arises.
//! - **A half-finished thing is never named.** `current.txt` is written after the tree it
//!   points at is complete, and written atomically — the same principle `deploy-windows`
//!   already lives by with its `.deployed-sha256` stamp.
//!
//! Path computation here is pure and platform-independent, so the layout the Windows box
//! will run is exercised by Linux tests and by a `nixosTest` VM (ground rule 5). Only
//! [`read_pointer`] and [`write_pointer`] touch the disk.

use std::fmt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::PathError;

/// A version's identity: the full commit sha of the release it was built from.
///
/// A newtype with a validating constructor because this string becomes a *directory
/// name*, and it arrives from a file on disk that a compromised updater — or a fat finger
/// — could have written anything into. `..` in a pointer file would make the launcher
/// spawn something outside the install tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VersionId(String);

impl VersionId {
    /// Read a version id, refusing anything that is not a full lowercase sha.
    ///
    /// # Errors
    /// [`LayoutError::NotAVersion`] for the wrong length, the wrong alphabet, or upper
    /// case — the last because two spellings of one version would be two directories.
    pub fn parse(text: &str) -> Result<Self, LayoutError> {
        let text = text.trim();
        let ok = text.len() == 40
            && text
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if ok {
            Ok(Self(text.to_owned()))
        } else {
            Err(LayoutError::NotAVersion {
                text: text.to_owned(),
            })
        }
    }

    /// The sha in full.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The seven-character form the release tag and the idle screen's footer use.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..7]
    }
}

impl fmt::Display for VersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which of the two pointer files.
///
/// An enum rather than two functions taking a file name, so a caller cannot ask for a
/// pointer that does not exist and the exhaustiveness is the compiler's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    /// `current.txt` — the version the launcher spawns.
    Current,
    /// `previous.txt` — the last version that ever reported itself healthy.
    Previous,
}

impl Pointer {
    /// The file name this pointer lives in.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Current => "current.txt",
            Self::Previous => "previous.txt",
        }
    }
}

/// An install tree rooted at some directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallTree {
    root: PathBuf,
}

impl InstallTree {
    /// The tree rooted at `root` — `%LOCALAPPDATA%\castaway` on the box, a temporary
    /// directory in a test.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The tree the *running* launcher belongs to: the directory it was started from.
    ///
    /// Anchored on the executable rather than on `%LOCALAPPDATA%` so the whole layout is
    /// relocatable — which is what lets a VM test drive a real launcher against a real
    /// tree in a temporary directory, and what keeps the box's install path out of the
    /// code.
    ///
    /// # Errors
    /// [`LayoutError::NoInstallRoot`] if the executable's own path cannot be resolved or
    /// has no parent, which on any real platform means the process was started in a way
    /// this program cannot reason about.
    pub fn of_running_launcher() -> Result<Self, LayoutError> {
        let exe = std::env::current_exe().map_err(|source| LayoutError::NoInstallRoot {
            source: Some(source),
        })?;
        let root = exe
            .parent()
            .ok_or(LayoutError::NoInstallRoot { source: None })?;
        Ok(Self::at(root))
    }

    /// The tree the *running receiver* belongs to, and which version it is.
    ///
    /// A receiver under a launcher lives at `<root>/versions/<sha>/castaway.exe`, so the
    /// tree is two directories up and the version is the name in between. Deriving it
    /// this way rather than taking it on trust is what tells a receiver whether it is
    /// managed at all: a `cargo run`, a `deploy-windows` flat install, or a copy somebody
    /// unzipped into Downloads all fail this and stand the updater down, which is exactly
    /// right — none of them has a launcher to hand over to.
    ///
    /// # Errors
    /// [`LayoutError::NotManaged`] if this executable is not inside a version tree, and
    /// [`LayoutError::NoInstallRoot`] if its own path cannot be resolved at all.
    pub fn of_running_receiver() -> Result<(Self, VersionId), LayoutError> {
        let exe = std::env::current_exe().map_err(|source| LayoutError::NoInstallRoot {
            source: Some(source),
        })?;
        let version_dir = exe
            .parent()
            .ok_or(LayoutError::NoInstallRoot { source: None })?;
        let not_managed = || LayoutError::NotManaged {
            exe: exe.clone(),
        };
        let versions = version_dir.parent().ok_or_else(not_managed)?;
        let root = versions.parent().ok_or_else(not_managed)?;
        if versions.file_name().and_then(|n| n.to_str()) != Some("versions") {
            return Err(not_managed());
        }
        let name = version_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(not_managed)?;
        let id = VersionId::parse(name).map_err(|_| not_managed())?;
        Ok((Self::at(root), id))
    }

    /// The install root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The stable launcher binary — the one thing the box's scheduled task names.
    #[must_use]
    pub fn launcher(&self) -> PathBuf {
        self.root
            .join(format!("launcher{}", std::env::consts::EXE_SUFFIX))
    }

    /// Where a pointer lives.
    #[must_use]
    pub fn pointer(&self, which: Pointer) -> PathBuf {
        self.root.join(which.file_name())
    }

    /// The receiver's stdout, shared across versions so a log tail survives an update.
    #[must_use]
    pub fn log(&self) -> PathBuf {
        self.root.join("castaway.log")
    }

    /// The one previous generation of [`Self::log`], rolled when it gets large. A
    /// crash-looping kiosk writes the same page of text for weeks otherwise.
    #[must_use]
    pub fn rolled_log(&self) -> PathBuf {
        self.root.join("castaway.log.1")
    }

    /// The stand-down marker. Present ⇒ a human deployed this tree by hand and the
    /// updater must not replace it at 4 a.m.; deleting it is how they re-arm.
    #[must_use]
    pub fn hold(&self) -> PathBuf {
        self.root.join("hold")
    }

    /// Where version trees live.
    #[must_use]
    pub fn versions(&self) -> PathBuf {
        self.root.join("versions")
    }

    /// One version's tree.
    #[must_use]
    pub fn version(&self, id: &VersionId) -> VersionTree {
        VersionTree {
            path: self.versions().join(id.as_str()),
        }
    }

    /// Where a version is assembled before it is named. Not under [`Self::version`]'s
    /// naming scheme on purpose: a directory whose name is a sha is a directory the
    /// launcher would spawn, and a half-extracted one must never be nameable that way.
    #[must_use]
    pub fn staging(&self, id: &VersionId) -> PathBuf {
        self.versions().join(format!(".staging-{}", id.as_str()))
    }
}

/// One installed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionTree {
    path: PathBuf,
}

impl VersionTree {
    /// The directory itself.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The receiver binary inside it.
    #[must_use]
    pub fn receiver(&self) -> PathBuf {
        self.path
            .join(format!("castaway{}", std::env::consts::EXE_SUFFIX))
    }

    /// The health marker the receiver touches once every enabled adapter is up.
    ///
    /// Its *absence* is what the launcher's rollback rule is about: a version that has
    /// never written this and dies quickly, repeatedly, is bad bits. A version that has
    /// written it once is a working receiver having a bad day, and its crashes are
    /// ordinary kiosk restarts.
    #[must_use]
    pub fn health_marker(&self) -> PathBuf {
        self.path.join(".healthy")
    }

    /// Has this version ever come all the way up?
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.health_marker().exists()
    }
}

/// Why a layout operation failed.
#[derive(Debug, Error)]
pub enum LayoutError {
    /// A pointer file held something that is not a version id.
    #[error("{text:?} is not a version id")]
    NotAVersion {
        /// What was in the file.
        text: String,
    },
    /// The pointer file is not there. Ordinary for `previous.txt` on a fresh install;
    /// fatal for `current.txt`, which is the caller's judgement to make rather than this
    /// module's.
    #[error("no {0} in the install tree")]
    NoPointer(&'static str),
    /// Reading or writing a pointer failed.
    #[error("{what} {path}")]
    Io {
        /// Which operation.
        what: &'static str,
        /// Which file.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
    /// This executable is not inside a `versions/<sha>/` tree, so nothing is managing it.
    #[error("{exe} is not inside an install tree")]
    NotManaged {
        /// Where this executable actually is.
        exe: PathBuf,
    },
    /// The running executable's own directory could not be resolved.
    #[error("cannot tell which directory this executable was started from")]
    NoInstallRoot {
        /// What the OS said, where it said anything.
        #[source]
        source: Option<std::io::Error>,
    },
}

/// Read a pointer.
///
/// # Errors
/// [`LayoutError::NoPointer`] if the file is absent, [`LayoutError::NotAVersion`] if it
/// holds something else, and [`LayoutError::Io`] for anything the filesystem refuses.
pub fn read_pointer(tree: &InstallTree, which: Pointer) -> Result<VersionId, LayoutError> {
    let path = tree.pointer(which);
    match std::fs::read_to_string(&path) {
        Ok(text) => VersionId::parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(LayoutError::NoPointer(which.file_name()))
        }
        Err(source) => Err(LayoutError::Io {
            what: "reading",
            path,
            source,
        }),
    }
}

/// Write a pointer, atomically.
///
/// Through a temporary file and a rename, because the alternative is a truncated
/// `current.txt` — a machine that loses power between `open` and `write` would otherwise
/// come back up naming nothing at all, which is the one state neither the launcher nor a
/// human at the panel can recover from without a keyboard.
///
/// # Errors
/// [`LayoutError::Io`] if the temporary file cannot be written or the rename refused.
pub fn write_pointer(
    tree: &InstallTree,
    which: Pointer,
    id: &VersionId,
) -> Result<(), LayoutError> {
    let path = tree.pointer(which);
    let temp = path.with_extension("new");
    std::fs::write(&temp, format!("{id}\n")).map_err(|source| LayoutError::Io {
        what: "writing",
        path: temp.clone(),
        source,
    })?;
    std::fs::rename(&temp, &path).map_err(|source| LayoutError::Io {
        what: "renaming into place",
        path,
        source,
    })
}

/// Create the install tree's directories.
///
/// # Errors
/// [`PathError::Create`] if the root or its `versions/` cannot be created.
pub fn ensure_tree(tree: &InstallTree) -> Result<(), PathError> {
    crate::ensure(tree.root())?;
    crate::ensure(&tree.versions())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_tree, read_pointer, write_pointer, InstallTree, LayoutError, Pointer, VersionId,
    };

    const SHA: &str = "ae2f19ef1f9d9a2488008f1075b252178ae7ef85";

    fn id() -> VersionId {
        VersionId::parse(SHA).expect("a real sha")
    }

    #[test]
    fn a_pointer_file_cannot_name_somewhere_outside_the_tree() {
        for hostile in [
            "..",
            "../../Windows/System32",
            "..\\..\\evil",
            "ae2f19e",
            "AE2F19EF1F9D9A2488008F1075B252178AE7EF85",
            "",
        ] {
            assert!(
                matches!(
                    VersionId::parse(hostile),
                    Err(LayoutError::NotAVersion { .. })
                ),
                "{hostile:?} was accepted as a version id"
            );
        }
    }

    #[test]
    fn surrounding_whitespace_is_not_a_different_version() {
        // `(echo %sha%)> current.txt` in cmd leaves a CRLF, and a deploy script that
        // wrote one would otherwise install a version the launcher cannot find.
        assert_eq!(
            VersionId::parse(&format!("{SHA}\r\n")).expect("parses"),
            id()
        );
    }

    #[test]
    fn a_staging_directory_is_never_a_name_the_launcher_would_spawn() {
        let tree = InstallTree::at("/opt/castaway");
        let staged = tree.staging(&id());
        let named = tree.version(&id());
        assert_ne!(staged, named.path());
        // The distinguishing character is the leading dot, which `VersionId::parse`
        // refuses — so a half-extracted tree cannot be pointed at even by hand.
        let stem = staged
            .file_name()
            .and_then(|s| s.to_str())
            .expect("a file name");
        assert!(matches!(
            VersionId::parse(stem),
            Err(LayoutError::NotAVersion { .. })
        ));
    }

    #[test]
    fn a_pointer_written_here_reads_back_here() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = InstallTree::at(dir.path());
        ensure_tree(&tree).expect("create");
        write_pointer(&tree, Pointer::Current, &id()).expect("write");
        assert_eq!(read_pointer(&tree, Pointer::Current).expect("read"), id());
        // And the two pointers are genuinely different files.
        assert!(matches!(
            read_pointer(&tree, Pointer::Previous),
            Err(LayoutError::NoPointer("previous.txt"))
        ));
    }

    #[test]
    fn writing_a_pointer_leaves_no_half_written_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = InstallTree::at(dir.path());
        ensure_tree(&tree).expect("create");
        write_pointer(&tree, Pointer::Current, &id()).expect("write");
        write_pointer(&tree, Pointer::Current, &id()).expect("rewrite");
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".new"))
            .collect();
        assert!(
            left.is_empty(),
            "temporary pointer files left behind: {left:?}"
        );
    }

    #[test]
    fn a_receiver_outside_a_version_tree_knows_it_is_unmanaged() {
        // The real `of_running_receiver` reads `current_exe`, which under a test runner
        // is the test binary — so what is asserted here is the shape rule it applies, on
        // paths, which is the part that can be wrong. The test binary itself is the
        // negative case and it costs nothing to say so.
        let err = InstallTree::of_running_receiver().expect_err("a test binary is not managed");
        assert!(
            matches!(err, LayoutError::NotManaged { .. }),
            "expected NotManaged, got {err}"
        );
    }

    #[test]
    fn a_version_is_healthy_only_once_it_has_said_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = InstallTree::at(dir.path());
        let version = tree.version(&id());
        std::fs::create_dir_all(version.path()).expect("mkdir");
        assert!(!version.is_healthy());
        std::fs::write(version.health_marker(), b"").expect("touch");
        assert!(version.is_healthy());
    }
}
