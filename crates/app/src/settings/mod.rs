//! The settings the panel can change about itself, and how they persist.
//!
//! Shape: the settings screen is a menu of settings; pressing one drills into a choice
//! list; pressing a choice applies it — to the running process *and* to the config
//! file, through [`ConfigStore`], so a restart agrees with the screen.
//!
//! Each setting is a [`Setting`]: something that can say what it is, what its options
//! are, which one is in effect, and how to apply a pick. The navigation in
//! `shell_nav` knows none of that — it renders whatever the catalog describes, the
//! same way the picker knows nothing about GameStream hosts. Adding a setting is
//! implementing the trait and adding it to the catalog in `main`; no screen code
//! changes hands.

mod output_device;
mod store;

use std::sync::Arc;

pub use output_device::OutputDeviceSetting;
pub use store::ConfigStore;

/// One option inside a setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Opaque to everything but the setting that minted it; echoed back to
    /// [`Setting::apply`].
    pub id: String,
    /// The line a person reads.
    pub label: String,
    /// A dimmer second line, where one helps.
    pub detail: Option<String>,
    /// Whether this is the option in effect right now.
    pub current: bool,
}

/// A setting's options, ready to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceList {
    /// A line under the screen title — where these options come from.
    pub subtitle: Option<String>,
    /// The options. May be empty; then `empty_message` says why.
    pub choices: Vec<Choice>,
    /// What to say instead of an empty list.
    pub empty_message: String,
}

/// What happened to an applied choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// In effect, and written to the config file.
    Saved,
    /// In effect now, but not persisted — carries why, in words fit for the panel.
    /// (The NixOS module points `$CASTAWAY_CONFIG` into the read-only store; the
    /// setting still works until restart, and the screen should say exactly that.)
    NotSaved(String),
}

/// One configurable thing.
///
/// Methods may block briefly (device enumeration asks a sound server); the navigation
/// calls them off the async loop.
pub trait Setting: Send + Sync {
    /// Stable slug, used in screen-row ids. No `:` — the row encoding uses it.
    fn id(&self) -> &'static str;
    /// The menu row's title.
    fn title(&self) -> String;
    /// The menu row's second line: the current value, in words.
    fn summary(&self) -> String;
    /// The options to drill into.
    ///
    /// # Errors
    /// A person-readable reason the options could not be listed.
    fn choices(&self) -> Result<ChoiceList, String>;
    /// Apply a choice by id, to the running process and the config file.
    ///
    /// # Errors
    /// A person-readable reason nothing changed.
    fn apply(&self, choice_id: &str) -> Result<Applied, String>;
}

/// Every setting this build offers, in menu order.
#[derive(Clone, Default)]
pub struct Catalog(Arc<Vec<Arc<dyn Setting>>>);

impl Catalog {
    /// A catalog of `settings`, shown in the order given.
    #[must_use]
    pub fn new(settings: Vec<Arc<dyn Setting>>) -> Self {
        Self(Arc::new(settings))
    }

    /// The settings, in menu order.
    #[must_use]
    pub fn all(&self) -> &[Arc<dyn Setting>] {
        &self.0
    }

    /// The setting with this id, if the build has it.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn Setting>> {
        self.0.iter().find(|s| s.id() == id).cloned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct Fake;
    impl Setting for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn title(&self) -> String {
            "Fake".into()
        }
        fn summary(&self) -> String {
            "off".into()
        }
        fn choices(&self) -> Result<ChoiceList, String> {
            Ok(ChoiceList {
                subtitle: None,
                choices: vec![],
                empty_message: "nothing".into(),
            })
        }
        fn apply(&self, _: &str) -> Result<Applied, String> {
            Ok(Applied::Saved)
        }
    }

    #[test]
    fn the_catalog_finds_settings_by_id_and_keeps_order() {
        let catalog = Catalog::new(vec![Arc::new(Fake)]);
        assert_eq!(catalog.all().len(), 1);
        assert!(catalog.get("fake").is_some());
        assert!(catalog.get("real").is_none());
    }
}
