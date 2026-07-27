//! Where the browser finds a Widevine CDM.
//!
//! There is no `--widevine-cdm-path` in CEF 147 — the switch does not exist in either
//! `libcef.so` or `libcef.dll` (it is an *Electron* extension, and passing it to CEF is a
//! no-op). Chromium finds the CDM in exactly two places, and this module models both:
//!
//! 1. **Preinstalled** — `WidevineCdm/` beside the module that contains Chromium's own
//!    code, i.e. next to `libcef`. `ComponentInstaller::StartRegistration` scans
//!    `DIR_COMPONENT_PREINSTALLED` (= `base::DIR_ASSETS` = `DIR_MODULE`) before it
//!    considers anything downloaded. This is what our packaging ships, so a panel with no
//!    internet still plays protected video.
//! 2. **Component-updated** — `<root_cache_path>/WidevineCdm/<version>/`, fetched by
//!    Chromium's component updater a few minutes into a run. Free, automatic, and the
//!    reason DRM appears to work on a dev box that was never configured for it — but it
//!    needs the network, so it is a fallback here rather than the plan.
//!
//! The platforms then diverge in *when* a found CDM becomes usable, which is the whole
//! reason this module has any code in it rather than being a path constant:
//!
//! - **Windows** registers live: `widevine_cdm_component_installer.cc` calls
//!   `CdmRegistry::RegisterCdm` from `ComponentReady`, so the CDM is usable in the same
//!   run it was discovered.
//! - **Linux** registers at startup only, before the zygote locks down, and locates the
//!   CDM through a *hint file* — `<root_cache_path>/WidevineCdm/latest-component-updated-widevine-cdm`,
//!   a JSON `{"Path": "..."}` written by the component updater. A preinstalled CDM is
//!   therefore invisible to the run that first sees it: the updater notices it, writes
//!   the hint, and it works from the *next* launch. [`ensure_hint`] writes that file
//!   ourselves so the first launch works too.
//!
//! Nothing here trusts a path it was handed: a CDM directory is only nameable as a [`Cdm`],
//! which cannot be built without a parseable manifest *and* the payload Chromium will try
//! to `dlopen`. A stale or half-unpacked directory is then a `None` at the boundary rather
//! than silent no-DRM three layers in — which is the failure this whole module exists to
//! stop, because it presents as "the video just doesn't start".

use std::path::{Path, PathBuf};

use crate::error::PipelineError;

/// The directory name Chromium looks for, in every location it looks.
const CDM_DIR: &str = "WidevineCdm";

/// The component updater's hint file, read at startup on Linux. The name is load-bearing:
/// Chromium's own comment says it must never change or existing CDMs stop being found.
const HINT_FILE: &str = "latest-component-updated-widevine-cdm";

/// Key in the hint file holding the CDM directory. Chromium also writes
/// `LastBundledVersion`, which only matters to builds that bundle a CDM; CEF does not.
const HINT_PATH_KEY: &str = "Path";

/// The `_platform_specific` subdirectory and library name Chromium builds the CDM path
/// from (`GetCdmPathFromInstallDir`). Wrong on the other platform by construction, so it
/// is a `cfg` rather than a runtime branch.
#[cfg(windows)]
const PLATFORM: (&str, &str) = ("win_x64", "widevinecdm.dll");
#[cfg(not(windows))]
const PLATFORM: (&str, &str) = ("linux_x64", "libwidevinecdm.so");

/// A CDM version, ordered the way Chromium orders component versions: numerically, one
/// component at a time. String comparison gets `4.10.3050.0` vs `4.9.x` backwards, and
/// getting it backwards means pinning an old CDM forever.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CdmVersion(Vec<u32>);

impl CdmVersion {
    /// Parse a dotted numeric version. Every component must be numeric — Chromium's
    /// `base::Version` rejects anything else, so accepting more here would mean accepting
    /// a CDM the browser will refuse.
    fn parse(text: &str) -> Option<Self> {
        let parts: Option<Vec<u32>> = text.split('.').map(|p| p.parse().ok()).collect();
        let parts = parts?;
        if parts.is_empty() {
            return None;
        }
        Some(Self(parts))
    }
}

impl std::fmt::Display for CdmVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for part in &self.0 {
            if !first {
                write!(f, ".")?;
            }
            write!(f, "{part}")?;
            first = false;
        }
        Ok(())
    }
}

/// A Widevine CDM directory that has been *checked*.
///
/// The only way to name a CDM is to construct one of these, and the only constructor
/// verifies both halves of what Chromium needs: a manifest carrying a version it can
/// parse, and the platform payload it will load. So "a path to a CDM" cannot silently
/// mean "a path that looked like one".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cdm {
    dir: PathBuf,
    version: CdmVersion,
}

impl Cdm {
    /// Check `dir` and describe the CDM in it, if there is one.
    #[must_use]
    pub fn at(dir: impl Into<PathBuf>) -> Option<Self> {
        let dir = dir.into();
        let manifest = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
        let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
        let version = CdmVersion::parse(manifest.get("version")?.as_str()?)?;
        // `VerifyInstallation` checks for the library too, and a directory that fails it
        // is not registered — so a manifest on its own is not a CDM.
        if !dir
            .join("_platform_specific")
            .join(PLATFORM.0)
            .join(PLATFORM.1)
            .is_file()
        {
            return None;
        }
        Some(Self { dir, version })
    }

    /// The directory holding `manifest.json` and `_platform_specific/`.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The version its manifest declares.
    #[must_use]
    pub fn version(&self) -> &CdmVersion {
        &self.version
    }
}

/// How this run would get a CDM, in the order Chromium prefers them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdmSource {
    /// Shipped beside `libcef` by our packaging: present on first launch, works offline.
    Preinstalled(Cdm),
    /// Downloaded into the profile by Chromium's component updater on an earlier run.
    ComponentUpdated(Cdm),
}

impl CdmSource {
    /// The CDM itself, whichever way it arrived.
    #[must_use]
    pub fn cdm(&self) -> &Cdm {
        match self {
            Self::Preinstalled(cdm) | Self::ComponentUpdated(cdm) => cdm,
        }
    }

    /// A word for logs, so "which of the two mechanisms fed us" is answerable from a
    /// panel's stderr rather than by inspecting its disk.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Preinstalled(_) => "preinstalled",
            Self::ComponentUpdated(_) => "component-updated",
        }
    }
}

/// The CDM shipped beside `libcef`, if the packaging staged one.
#[must_use]
pub fn preinstalled(module_dir: &Path) -> Option<Cdm> {
    Cdm::at(module_dir.join(CDM_DIR))
}

/// The newest CDM the component updater has unpacked into the profile.
///
/// Newest, not first: the updater keeps old versions around until it prunes them, and
/// Chromium selects by version.
#[must_use]
pub fn component_updated(user_data_dir: &Path) -> Option<Cdm> {
    let base = user_data_dir.join(CDM_DIR);
    std::fs::read_dir(base)
        .ok()?
        .flatten()
        .filter_map(|entry| Cdm::at(entry.path()))
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// The CDM the Linux startup path will actually register: whatever the hint file points at.
#[must_use]
pub fn hinted(user_data_dir: &Path) -> Option<Cdm> {
    let text = std::fs::read_to_string(user_data_dir.join(CDM_DIR).join(HINT_FILE)).ok()?;
    let hint: serde_json::Value = serde_json::from_str(&text).ok()?;
    Cdm::at(hint.get(HINT_PATH_KEY)?.as_str()?)
}

/// Point the hint file at `cdm`, unless something at least as new is already hinted.
///
/// Returns whether it wrote. The version guard is what keeps this from fighting the
/// component updater: once the updater downloads something newer than what we ship, its
/// hint stands, and re-pinning ours on every launch would freeze the panel on an old CDM.
///
/// # Errors
/// [`PipelineError::Widevine`] if the hint could not be written — a read-only or missing
/// profile directory. Reported rather than ignored, because the symptom otherwise is
/// simply that protected video never plays.
pub fn ensure_hint(user_data_dir: &Path, cdm: &Cdm) -> Result<bool, PipelineError> {
    if let Some(current) = hinted(user_data_dir) {
        if current.version >= cdm.version {
            return Ok(false);
        }
    }

    let dir = user_data_dir.join(CDM_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| PipelineError::Widevine(format!("creating {}: {e}", dir.display())))?;

    // Chromium reads this with a JSON deserializer and takes `Path` verbatim, so the
    // encoding of a non-UTF-8 path is the serializer's problem, not ours: `to_string_lossy`
    // would hand it a path that does not exist.
    let path = cdm
        .dir
        .to_str()
        .ok_or_else(|| PipelineError::Widevine("CDM path is not valid UTF-8".to_string()))?;
    let body = serde_json::json!({ HINT_PATH_KEY: path }).to_string();

    let hint = dir.join(HINT_FILE);
    std::fs::write(&hint, body)
        .map_err(|e| PipelineError::Widevine(format!("writing {}: {e}", hint.display())))?;
    Ok(true)
}

/// What this run has, without touching anything.
///
/// On Linux this deliberately reports a preinstalled CDM as available even before the hint
/// exists, because [`configure`] runs before CEF starts and will have written it by the
/// time anything can ask for a key system.
#[must_use]
pub fn available(user_data_dir: &Path, module_dir: Option<&Path>) -> Option<CdmSource> {
    module_dir
        .and_then(preinstalled)
        .map(CdmSource::Preinstalled)
        .or_else(|| component_updated(user_data_dir).map(CdmSource::ComponentUpdated))
}

/// Settle the CDM situation for this run, before CEF initializes, and say what it is.
///
/// The only side effect is the Linux hint file, and only when we ship something newer than
/// what is already hinted.
#[must_use]
pub fn configure(user_data_dir: &Path, module_dir: Option<&Path>) -> Option<CdmSource> {
    let source = available(user_data_dir, module_dir)?;

    // Windows registers a discovered CDM live, from `ComponentReady`; there is nothing to
    // nudge. Linux would not see ours until the next launch without this.
    #[cfg(not(windows))]
    if let CdmSource::Preinstalled(cdm) = &source {
        match ensure_hint(user_data_dir, cdm) {
            Ok(true) => tracing::debug!(
                target: "castaway::cef",
                dir = %cdm.dir().display(),
                version = %cdm.version(),
                "pointed the widevine hint file at the CDM we ship"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                target: "castaway::cef",
                error = %e,
                "could not write the widevine hint file; DRM-protected video will not play"
            ),
        }
    }

    Some(source)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A scratch directory, without taking a dev-dependency for four tests. Named by pid
    /// and a counter so concurrent test threads (and concurrent `cargo test` runs) do not
    /// collide.
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "castaway-widevine-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Lay out a CDM directory the way the CRX unpacks: manifest plus platform payload.
    fn make_cdm(dir: &Path, version: &str) {
        let lib = dir.join("_platform_specific").join(PLATFORM.0);
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join(PLATFORM.1), b"not really a cdm").unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(r#"{{"name":"WidevineCdm","version":"{version}"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn versions_order_numerically_not_lexically() {
        let older = CdmVersion::parse("4.9.9999.0").unwrap();
        let newer = CdmVersion::parse("4.10.3050.0").unwrap();
        assert!(newer > older, "{newer} should outrank {older}");
        assert_eq!(newer.to_string(), "4.10.3050.0");
        assert!(CdmVersion::parse("4.10.beta").is_none());
        assert!(CdmVersion::parse("").is_none());
    }

    #[test]
    fn a_manifest_without_the_library_is_not_a_cdm() {
        let dir = scratch("manifest-only");
        std::fs::write(
            dir.join("manifest.json"),
            br#"{"name":"WidevineCdm","version":"4.10.3050.0"}"#,
        )
        .unwrap();
        assert!(
            Cdm::at(&dir).is_none(),
            "a directory Chromium would refuse must not be nameable as a CDM"
        );

        make_cdm(&dir, "4.10.3050.0");
        let cdm = Cdm::at(&dir).expect("now it is a CDM");
        assert_eq!(cdm.version(), &CdmVersion::parse("4.10.3050.0").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn preinstalled_is_found_and_hinted_round_trips() {
        let root = scratch("preinstall");
        let module_dir = root.join("bin");
        let user_data = root.join("profile");
        std::fs::create_dir_all(&user_data).unwrap();
        make_cdm(&module_dir.join(CDM_DIR), "4.10.3050.0");

        let found = preinstalled(&module_dir).expect("staged CDM is found beside libcef");
        assert!(hinted(&user_data).is_none(), "nothing hints at it yet");

        assert!(ensure_hint(&user_data, &found).unwrap(), "first write");
        assert_eq!(
            hinted(&user_data).as_ref(),
            Some(&found),
            "Chromium's startup path would now find exactly what we staged"
        );
        assert!(
            !ensure_hint(&user_data, &found).unwrap(),
            "an unchanged hint is left alone"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_newer_component_updated_cdm_is_not_downgraded() {
        let root = scratch("no-downgrade");
        let module_dir = root.join("bin");
        let user_data = root.join("profile");
        make_cdm(&module_dir.join(CDM_DIR), "4.10.3050.0");
        // What the component updater would have unpacked and hinted, later than our ship date.
        let downloaded = user_data.join(CDM_DIR).join("4.11.1.0");
        make_cdm(&downloaded, "4.11.1.0");
        let newer = Cdm::at(&downloaded).unwrap();
        ensure_hint(&user_data, &newer).unwrap();

        let ours = preinstalled(&module_dir).unwrap();
        assert!(
            !ensure_hint(&user_data, &ours).unwrap(),
            "re-pinning our older CDM every launch would freeze the panel on it"
        );
        assert_eq!(hinted(&user_data).as_ref(), Some(&newer));

        // And the scan that backs the fallback picks the newest, not the first read_dir hit.
        make_cdm(&user_data.join(CDM_DIR).join("4.10.3050.0"), "4.10.3050.0");
        assert_eq!(component_updated(&user_data).as_ref(), Some(&newer));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
