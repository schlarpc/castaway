//! The settings the panel can change about itself, and how they persist.
//!
//! Shape: the settings screen is a menu of settings; pressing one drills in. What it
//! drills *into* is one of two things (D40 survives this, because `shell_nav` still knows
//! no setting by name):
//!
//! - a **choice list** — enumerate, pick one, apply it to the running process *and* to
//!   the config file through [`ConfigStore`], so a restart agrees with the screen;
//! - an **action** — start something and watch it happen, reporting through a channel the
//!   navigation loop selects on (#360). "Check for updates" is not a list of options: it
//!   is a thing with a lifecycle, whose screen changes several times without anybody
//!   touching it.
//!
//! Each setting is a [`Setting`]: something that can say what it is, what is in effect,
//! and which of the two shapes it has. The navigation in `shell_nav` knows none of that —
//! it renders whatever the catalog describes, the same way the picker knows nothing about
//! GameStream hosts. Adding a setting is implementing the trait and adding it to the
//! catalog in `main`; no screen code changes hands.

mod output_device;
mod store;
mod update_check;

use std::sync::Arc;

use tokio::sync::mpsc;

pub use output_device::OutputDeviceSetting;
pub use store::ConfigStore;
pub use update_check::UpdateCheckSetting;

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

/// A row on an action's screen, and what pressing it means to that action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Opaque to everything but the action that minted it; echoed back to
    /// [`Action::press`].
    pub id: String,
    /// The line a person reads.
    pub label: String,
    /// A dimmer second line, where one helps.
    pub detail: Option<String>,
}

/// What an action's screen is doing, when a frame arrives.
///
/// Three, because "still working", "here is the answer" and "it did not work" are three
/// different things to somebody standing at the panel — the same split
/// `PickerStatus` already makes, said in the catalog's own terms so the settings
/// layer does not depend on the renderer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Still going. Another frame is coming.
    Working,
    /// Settled: [`Report::say`] is the answer.
    Settled,
    /// It could not be done, and [`Report::say`] is why.
    Failed,
}

/// One frame of an action's lifecycle: what the screen should show now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The sentence a person reads — what is happening, what the answer is, or what went
    /// wrong. Carries the words in all three [`Stage`]s, because a screen with a status
    /// and no words is a spinner.
    pub say: String,
    /// Which of the three this is.
    pub stage: Stage,
    /// What can be pressed. Empty while working, because an action in flight has nothing
    /// to offer yet.
    pub rows: Vec<Row>,
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
    /// What drilling into it does.
    fn drilldown(&self) -> Drilldown<'_>;
}

/// The two shapes a setting can have.
///
/// An enum rather than an optional method, so `shell_nav`'s `match` stops compiling if a
/// third is added — there is no default arm for a new shape to fall silently into
/// (ground rule 1).
pub enum Drilldown<'a> {
    /// Enumerate options and pick one.
    Choices(&'a dyn Choices),
    /// Start something and watch it happen.
    Action(&'a dyn Action),
}

/// A setting that drills into a list.
pub trait Choices: Send + Sync {
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

/// A setting that drills into something happening.
///
/// The precedent is `shell_nav`'s pairing: a press that spawns work outliving it, which
/// reports back through a channel while the event loop keeps answering presses. This is
/// the same pattern with more than one message.
pub trait Action: Send + Sync {
    /// Start it, and hand back the frames it will report.
    ///
    /// Returns immediately. **Dropping the receiver must not stop the work** — somebody
    /// who wanders off mid-download should come back to a panel that took the update,
    /// not one that threw three minutes of a shared uplink away (#360).
    fn start(&self) -> mpsc::UnboundedReceiver<Report>;
    /// A row from one of those frames was pressed. What happens is reported through the
    /// stream that is already running, not returned here.
    fn press(&self, row_id: &str);
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
        fn drilldown(&self) -> Drilldown<'_> {
            Drilldown::Choices(self)
        }
    }
    impl Choices for Fake {
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
