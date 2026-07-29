//! Choosing an audio output device — the model, without the audio stack.
//!
//! Deliberately not gated behind the `audio` feature, for the same reason `theme` is
//! not behind `render`: the selection is a field in the app's config file, which has to
//! parse (and the settings screen has to describe) on a build with no audio in it at
//! all. Nothing here opens a device — the backends that do live in `audio_out`.

use std::sync::{Arc, PoisonError, RwLock};

use crate::error::PipelineError;

/// Which device a session's stream should open.
///
/// An enum rather than an `Option<String>` so "follow the system default" is a stated
/// position, not the absence of one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OutputSelection {
    /// The operating system's default device, wherever that points today.
    #[default]
    SystemDefault,
    /// A specific device, named in the active backend's own vocabulary — a PipeWire
    /// `node.name`, a WASAPI device name. Meaningless on any other machine, which is
    /// why config keys it per backend.
    Device(String),
}

/// The current output selection, shared between whoever chooses (the settings screen)
/// and whoever opens devices (each new session's stream).
///
/// A handle rather than a value so a change applies to the *next* stream without
/// restarting anything: sessions already playing keep the device they opened, the same
/// way they keep their sample rate.
#[derive(Debug, Clone, Default)]
pub struct OutputSelector(Arc<RwLock<OutputSelection>>);

impl OutputSelector {
    /// A selector starting at `initial`.
    #[must_use]
    pub fn new(initial: OutputSelection) -> Self {
        Self(Arc::new(RwLock::new(initial)))
    }

    /// Change the selection. Takes effect for streams opened after this call.
    pub fn set(&self, selection: OutputSelection) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = selection;
    }

    /// What is currently selected.
    #[must_use]
    pub fn get(&self) -> OutputSelection {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One selectable output device, as a settings screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDeviceInfo {
    /// Stable identity: what [`OutputSelection::Device`] carries and config persists.
    pub id: String,
    /// What a person reads. Often the same string as `id`; PipeWire has a nicer one.
    pub label: String,
}

/// The device namespace this build selects from.
///
/// Compile-time, because the backend is: the config file keys a choice per namespace so
/// one file can travel between the Linux box and the Windows panel, and this is how the
/// app asks which key applies here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputBackendKind {
    /// The native PipeWire backend (Linux, `audio-pipewire`).
    PipeWire,
    /// cpal's WASAPI host (Windows, `audio-out`).
    Windows,
    /// cpal's ALSA host — the Linux build without the PipeWire backend.
    Alsa,
    /// No real device in this build; the null sink plays nothing and selects nothing.
    Null,
}

impl OutputBackendKind {
    /// A short human answer to "why can't I pick a device here", or `None` if you can.
    #[must_use]
    pub const fn unavailable_reason(self) -> Option<&'static str> {
        match self {
            Self::PipeWire | Self::Windows | Self::Alsa => None,
            Self::Null => Some("this build has no audio output device"),
        }
    }
}

/// Which backend `audio_out::selected_output` opens in this build.
///
/// PipeWire outranks cpal where both are compiled: the kiosk feature list carries both
/// so one list cross-builds, and on Linux the native backend is the one that can
/// actually name sinks.
#[must_use]
pub const fn active_backend() -> OutputBackendKind {
    #[cfg(all(feature = "audio-pipewire", target_os = "linux"))]
    {
        OutputBackendKind::PipeWire
    }
    #[cfg(all(
        feature = "audio-out",
        not(all(feature = "audio-pipewire", target_os = "linux"))
    ))]
    {
        #[cfg(target_os = "windows")]
        {
            OutputBackendKind::Windows
        }
        #[cfg(not(target_os = "windows"))]
        {
            OutputBackendKind::Alsa
        }
    }
    #[cfg(not(any(
        feature = "audio-out",
        all(feature = "audio-pipewire", target_os = "linux")
    )))]
    {
        OutputBackendKind::Null
    }
}

/// The devices the active backend can open, for a settings screen to offer.
///
/// The system default is *not* an entry: it is not a device, it is a policy, and the
/// screen offers it separately so a device that happens to be the default is still
/// selectable by name.
///
/// # Errors
/// [`PipelineError::Audio`] if the backend cannot be asked — no PipeWire daemon, no
/// audio host. An empty list is not an error; a build with no backend returns one.
pub fn list_output_devices() -> Result<Vec<OutputDeviceInfo>, PipelineError> {
    #[cfg(all(feature = "audio-pipewire", target_os = "linux"))]
    {
        crate::audio_pw::list_sinks()
    }
    #[cfg(all(
        feature = "audio-out",
        not(all(feature = "audio-pipewire", target_os = "linux"))
    ))]
    {
        crate::audio_out::cpal_devices()
    }
    #[cfg(not(any(
        feature = "audio-out",
        all(feature = "audio-pipewire", target_os = "linux")
    )))]
    {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_is_one_shared_choice_not_a_copy() {
        // The settings screen and the session factories hold clones of one selector;
        // if cloning copied the value, a picked device would reach nobody.
        let sel = OutputSelector::default();
        assert_eq!(sel.get(), OutputSelection::SystemDefault);
        let settings_side = sel.clone();
        settings_side.set(OutputSelection::Device("dac".into()));
        assert_eq!(sel.get(), OutputSelection::Device("dac".into()));
    }

    #[test]
    fn only_the_null_backend_declines_to_select() {
        for kind in [
            OutputBackendKind::PipeWire,
            OutputBackendKind::Windows,
            OutputBackendKind::Alsa,
        ] {
            assert!(kind.unavailable_reason().is_none());
        }
        assert!(OutputBackendKind::Null.unavailable_reason().is_some());
    }
}
