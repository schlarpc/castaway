//! The first setting: which device sound comes out of.
//!
//! "Default or a specific device", for whichever backend this build has — PipeWire
//! sinks on the Linux box, WASAPI devices on the Windows panel. Applying a choice does
//! two things: points the shared [`OutputSelector`] at it (every source's *next*
//! session opens the new device — sessions already playing keep theirs, like they keep
//! their sample rate), and writes it under `[audio.output]` keyed by backend, so the
//! same file keeps working when it travels to the other machine.

use pipeline::audio_select::{
    active_backend, list_output_devices, OutputBackendKind, OutputSelection, OutputSelector,
};

use super::{Applied, Choice, ChoiceList, ConfigStore, Setting};
use crate::config::AudioOutput;

/// The choice id for "follow the system default".
const DEFAULT_ID: &str = "default";
/// Device choice ids are prefixed so a device cannot collide with [`DEFAULT_ID`],
/// whatever it is named.
const DEVICE_PREFIX: &str = "device:";

/// Output device selection, over whichever backend the build has.
pub struct OutputDeviceSetting {
    selector: OutputSelector,
    store: ConfigStore,
    backend: OutputBackendKind,
}

impl OutputDeviceSetting {
    /// A setting driving `selector` and persisting through `store`, for this build's
    /// backend.
    #[must_use]
    pub fn new(selector: OutputSelector, store: ConfigStore) -> Self {
        Self {
            selector,
            store,
            backend: active_backend(),
        }
    }

    /// As [`Self::new`], but for a stated backend — the testable constructor.
    #[cfg(test)]
    fn for_backend(
        selector: OutputSelector,
        store: ConfigStore,
        backend: OutputBackendKind,
    ) -> Self {
        Self {
            selector,
            store,
            backend,
        }
    }
}

impl Setting for OutputDeviceSetting {
    fn id(&self) -> &'static str {
        "audio-output"
    }

    fn title(&self) -> String {
        "Audio output".into()
    }

    fn summary(&self) -> String {
        match self.selector.get() {
            OutputSelection::SystemDefault => "System default".into(),
            OutputSelection::Device(id) => id,
        }
    }

    fn choices(&self) -> Result<ChoiceList, String> {
        if let Some(reason) = self.backend.unavailable_reason() {
            return Ok(ChoiceList {
                subtitle: None,
                choices: Vec::new(),
                empty_message: reason.into(),
            });
        }
        let current = self.selector.get();
        let mut choices = vec![Choice {
            id: DEFAULT_ID.into(),
            label: "System default".into(),
            detail: Some("Follow the operating system's choice".into()),
            current: current == OutputSelection::SystemDefault,
        }];
        let devices =
            list_output_devices().map_err(|e| format!("could not list output devices: {e}"))?;
        choices.extend(devices.into_iter().map(|d| Choice {
            current: current == OutputSelection::Device(d.id.clone()),
            detail: (d.label != d.id).then(|| d.id.clone()),
            id: format!("{DEVICE_PREFIX}{}", d.id),
            label: d.label,
        }));
        Ok(ChoiceList {
            subtitle: Some("Where sound comes out. Takes effect for the next thing played.".into()),
            choices,
            empty_message: "No output devices found".into(),
        })
    }

    fn apply(&self, choice_id: &str) -> Result<Applied, String> {
        let Some(key) = AudioOutput::key_for(self.backend) else {
            return Err("this build has no audio output device to select".into());
        };
        let choice = if choice_id == DEFAULT_ID {
            crate::config::OutputChoice::Default
        } else if let Some(id) = choice_id.strip_prefix(DEVICE_PREFIX) {
            crate::config::OutputChoice::Device(id.to_owned())
        } else {
            return Err(format!("\"{choice_id}\" is not an output choice"));
        };

        // Runtime first: even if the disk refuses, the person at the panel asked for
        // this device *now*, and the next session should use it.
        self.selector.set(choice.selection());

        match self.store.set(&["audio", "output", key], choice.as_str()) {
            Ok(()) => Ok(Applied::Saved),
            Err(e) => Ok(Applied::NotSaved(format!(
                "In effect until restart, but not saved: {e}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn scratch(name: &str) -> (std::path::PathBuf, ConfigStore) {
        let path = std::env::temp_dir().join(format!(
            "castaway-outdev-{}-{name}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        (path.clone(), ConfigStore::new(path))
    }

    #[test]
    fn applying_a_device_moves_the_selector_and_the_file_in_the_backend_key() {
        let (path, store) = scratch("apply");
        let selector = OutputSelector::default();
        let setting =
            OutputDeviceSetting::for_backend(selector.clone(), store, OutputBackendKind::PipeWire);

        let applied = setting
            .apply("device:alsa_output.usb-DAC.analog-stereo")
            .unwrap();
        assert_eq!(applied, Applied::Saved);
        assert_eq!(
            selector.get(),
            OutputSelection::Device("alsa_output.usb-DAC.analog-stereo".into())
        );
        let cfg: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            cfg.audio.output.pipewire,
            crate::config::OutputChoice::Device("alsa_output.usb-DAC.analog-stereo".into())
        );
        // The other backends' keys were not invented on the way through.
        assert_eq!(
            cfg.audio.output.windows,
            crate::config::OutputChoice::Default
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn going_back_to_default_is_a_stated_position_in_the_file() {
        let (path, store) = scratch("default");
        let selector = OutputSelector::new(OutputSelection::Device("x".into()));
        let setting =
            OutputDeviceSetting::for_backend(selector.clone(), store, OutputBackendKind::Windows);
        setting.apply("default").unwrap();
        assert_eq!(selector.get(), OutputSelection::SystemDefault);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("windows = \"default\""));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_failed_save_still_applies_and_says_both_halves() {
        // Point the store somewhere it cannot write: the config the NixOS module generates
        // lives in the read-only store, and that must not make the setting inert.
        //
        // A *file* standing where the directory should be, rather than the missing
        // directory this used to use. A missing directory is no longer a failure — the
        // store creates it, which is #179 — and it was never the case this test meant;
        // a chmod'd directory would be, but only when the tests do not run as root.
        let blocker =
            std::env::temp_dir().join(format!("castaway-outdev-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&blocker);
        std::fs::write(&blocker, "a file, so nothing can be created beneath it").unwrap();
        let store = ConfigStore::new(blocker.join("castaway.toml"));
        let selector = OutputSelector::default();
        let setting =
            OutputDeviceSetting::for_backend(selector.clone(), store, OutputBackendKind::PipeWire);

        let applied = setting.apply("device:sink").unwrap();
        assert!(matches!(applied, Applied::NotSaved(_)), "{applied:?}");
        assert_eq!(selector.get(), OutputSelection::Device("sink".into()));
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn nonsense_choices_change_nothing() {
        let (_path, store) = scratch("nonsense");
        let selector = OutputSelector::default();
        let setting =
            OutputDeviceSetting::for_backend(selector.clone(), store, OutputBackendKind::PipeWire);
        assert!(setting.apply("device").is_err());
        assert_eq!(selector.get(), OutputSelection::SystemDefault);
    }

    #[test]
    fn a_build_with_no_device_backend_says_so_instead_of_listing() {
        let (_path, store) = scratch("null");
        let setting = OutputDeviceSetting::for_backend(
            OutputSelector::default(),
            store,
            OutputBackendKind::Null,
        );
        let list = setting.choices().unwrap();
        assert!(list.choices.is_empty());
        assert!(!list.empty_message.is_empty());
        assert!(setting.apply("default").is_err());
    }
}
