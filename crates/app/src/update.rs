//! Wiring the auto-updater into the receiver (#345).
//!
//! The policy and the machinery live in `castaway-update`; this is the part that is
//! genuinely the app's — deciding whether to arm it at all, telling it what the panel is
//! doing, and turning "it is time" into a clean shutdown and the exit code the launcher
//! is waiting for.
//!
//! Nothing here fails the receiver. Every stand-down is a log line and a receiver that
//! carries on doing its job, because an unattended panel that refuses to cast because it
//! could not update itself would be a strictly worse machine than one that never updates.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use castaway_core::CastingHandle;
use castaway_update::agent::{Agent, PanelActivity, StandDown};
use castaway_update::manifest::InstalledBuild;
use tracing::{info, warn};

use crate::config::Update as UpdateConfig;

/// This build's place in the release ordering, stamped in by `build.rs`.
#[must_use]
pub fn installed_build() -> InstalledBuild {
    InstalledBuild::from_stamp(env!("CASTAWAY_BUILD"))
}

/// What the receiver can honestly say about whether the panel is in use.
struct Panel {
    casting: CastingHandle,
}

impl PanelActivity for Panel {
    fn casting(&self) -> bool {
        self.casting.casting()
    }

    fn idle_for(&self) -> Duration {
        last_input_idle()
    }
}

/// How long since anybody touched the panel.
///
/// On Windows this is `GetLastInputInfo`, which is the whole session's answer — keyboard,
/// mouse and touch, including input to the browser child, which is the point: the
/// receiver does not see a person scrolling a dashboard in the widget card, and Windows
/// does.
// Windows glue, which ground rule 8 admits alongside `pipeline` and `control-display`:
// there is no safe wrapper for this in any crate the tree already carries, and the
// alternative is a receiver that cannot tell whether the room is empty. Both blocks carry
// their SAFETY note; nothing else in `app` is unsafe.
#[cfg(windows)]
#[allow(unsafe_code)]
fn last_input_idle() -> Duration {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: u32::try_from(std::mem::size_of::<LASTINPUTINFO>()).unwrap_or(0),
        dwTime: 0,
    };
    // SAFETY: `info` is a live, fully initialised `LASTINPUTINFO` whose `cbSize` is its
    // own size, which is the contract `GetLastInputInfo` documents. The call writes only
    // `dwTime` and does not retain the pointer.
    let ok = unsafe { GetLastInputInfo(&raw mut info) }.as_bool();
    if !ok {
        // The one documented failure is a caller that is not on an interactive desktop —
        // a service, or session 0. Reporting "busy" there is the safe answer: a receiver
        // that cannot see input must not conclude the room is empty.
        return Duration::ZERO;
    }
    // Both are 32-bit millisecond tick counts that wrap every 49 days, so the subtraction
    // has to wrap with them. `GetTickCount64` would not help: `dwTime` is 32-bit either
    // way, and mixing the two widths is how this reads as 49 days of idleness once a
    // month on a panel that is never rebooted.
    // SAFETY: `GetTickCount` takes no arguments and returns a value.
    let now = unsafe { GetTickCount() };
    Duration::from_millis(u64::from(now.wrapping_sub(info.dwTime)))
}

/// Off Windows there is no session-wide "when was the last input" to ask for, and the
/// honest answer is that this platform is not where the updater runs: a Linux build is
/// not installed under a launcher, so it stands down as `Unmanaged` before ever reaching
/// this. Reporting "idle" rather than "busy" is what lets the VM test drive the whole
/// loop — and in that test the panel genuinely is idle.
#[cfg(not(windows))]
fn last_input_idle() -> Duration {
    Duration::MAX
}

/// Arm the updater, or say why not.
///
/// Returns the shared exit code the process ends on. Zero until an update activates, at
/// which point it becomes the launcher's reserved handshake value and `main` exits with
/// it — which is how "restart me into the new version" is said, and the only way it can
/// be said: spawning a successor directly would put it outside the interactive session
/// (docs/cross-build.md, session 0).
pub fn spawn(
    runtime: &tokio::runtime::Runtime,
    config: &UpdateConfig,
    casting: CastingHandle,
    utc_offset_secs: i32,
    shutdown: crate::shutdown::Shutdown,
    kiosk_exit: Arc<std::sync::atomic::AtomicBool>,
    kiosk_wake: castaway_core::Waker,
) -> Arc<AtomicI32> {
    let exit_code = Arc::new(AtomicI32::new(0));

    let policy = match config.policy() {
        Ok(policy) => policy,
        Err(e) => {
            // A refused schedule is not a reason to fall back to the default one: an
            // operator who wrote a window meant something by it, and updating at an hour
            // nobody chose is exactly the surprise this whole feature is trying to avoid.
            warn!(error = %e, "auto-update: the configured window is not a schedule; disarmed");
            return exit_code;
        }
    };

    let installed = installed_build();
    let agent = Agent::new(
        config.enable,
        policy,
        config.source(),
        installed,
        Arc::new(Panel { casting }),
        utc_offset_secs,
    );
    let agent = match agent {
        Ok(agent) => agent,
        Err(reason) => {
            // Each of these is a state rather than a fault, so they are `info` — except
            // damaged trust anchors, which mean this build can verify nothing at all and
            // is somebody's to fix.
            match &reason {
                StandDown::TrustAnchors(_) => warn!("auto-update: {reason}"),
                _ => info!("auto-update: {reason}"),
            }
            return exit_code;
        }
    };

    // Health, on its own timer: the launcher's rollback rule reads the marker this
    // writes, and writing it early would mean a version that dies on its second minute
    // never gets rolled back.
    let healthy_after = Duration::from_secs(u64::from(config.healthy_after_minutes) * 60);
    runtime.spawn(async move {
        tokio::time::sleep(healthy_after).await;
        tokio::task::spawn_blocking(move || {
            castaway_update::agent::mark_healthy_and_tidy(installed);
        })
        .await
        .ok();
    });

    let code = Arc::clone(&exit_code);
    runtime.spawn(async move {
        let activation = agent.run().await;
        info!(
            from = %activation.replacing.short(),
            to = %activation.version.short(),
            "auto-update: shutting down so the launcher can start the new version"
        );
        // The same shutdown ctrl-c takes, for the same reason: senders are told, the
        // kiosk loop is woken so it notices, and the process leaves tidily. The exit code
        // is what makes it a handover rather than a crash.
        code.store(
            castaway_launcher::supervise::ACTIVATE_EXIT_CODE,
            Ordering::SeqCst,
        );
        kiosk_exit.store(true, std::sync::atomic::Ordering::Relaxed);
        kiosk_wake.wake();
        shutdown.fire();
    });

    exit_code
}
