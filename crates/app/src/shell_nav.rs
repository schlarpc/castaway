//! What happens when someone presses something the shell could not answer itself (D38).
//!
//! Most of the shell is local: a service tile opens that service's screen with no round
//! trip, because a panel that waits on the network to redraw feels broken. This is the
//! rest — the presses that mean *go and do something*, which today is Moonlight and
//! tomorrow is a media library and a settings screen.
//!
//! It owns the navigation for those: tile → host picker → app picker → streaming. Each
//! step answers the panel immediately with a "looking…" screen and fills it in when the
//! network does, because discovery over mDNS and a pairing handshake are both slow
//! enough that a blank screen would read as a hang.
//!
//! Pairing is the one flow here that outlives its press. Pressing an unpaired host puts
//! a PIN on the glass and *waits on a human at another machine* — so it runs as a spawned
//! task reporting back through a channel, while the event loop keeps answering presses.
//! An action setting (#360) is the same pattern with more than one message: it reports a
//! stream of frames for as long as it has something to say.
//!
//! Anything that keeps talking after its press uses a **tagged** replace. `back` is
//! answered render-side and never reaches this file, so a producer running for minutes
//! cannot know the person walked off; the tag lets the stack refuse the update instead of
//! this loop guessing at it.

use std::sync::Arc;
use std::time::Duration;

use pipeline::picker::{Picker, PickerItem};
use pipeline::render_pipeline::RenderCommand;
use pipeline::shell::{Screen, ShellEvent};
use proto_gamestream::{GameStreamAdapter, GameStreamCommand, GameStreamError, PairingPin};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::settings::{self, Applied, Drilldown, Stage};

/// The id of the Moonlight tile on the Home screen. Matches what `build_attract` emits.
const MOONLIGHT_TILE: &str = "gamestream";
/// The id of the Settings tile. Matches what `build_attract` emits.
const SETTINGS_TILE: &str = "settings";

/// Picker item ids are namespaced, because one handler serves every picker and a bare id
/// would not say which list it came from — an app called `10.0.0.7` is not impossible.
const HOST_PREFIX: &str = "host:";
const APP_PREFIX: &str = "app:";
/// The "try pairing again" row on a failed pairing screen: `pair:<host>`.
const PAIR_PREFIX: &str = "pair:";
/// A row on the settings menu: `setting:<setting id>`.
const SETTING_PREFIX: &str = "setting:";
/// A row on one setting's choice list: `choose:<setting id>:<choice id>`. The setting
/// id is a slug with no `:` in it; the choice id is opaque and may contain anything.
const CHOICE_PREFIX: &str = "choose:";

/// How long a pairing waits on the glass before the panel calls it off.
///
/// The protocol layer deliberately has no timeout — Sunshine parks its response until a
/// human types the PIN, and that is correct there. But a wall panel showing "waiting…"
/// forever is a hang to anyone who walks past it, so the panel owns one: long enough to
/// walk to a PC, log into Sunshine's web UI, and type four digits; short enough that an
/// abandoned attempt does not squat on the screen (and the one-at-a-time slot) all day.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(180);

/// The one pairing the panel is running, if any.
///
/// One at a time by design: there is one person standing at one panel, and the PIN
/// screen shows one PIN — a second concurrent handshake would put a PIN nobody can see
/// behind the screen and park a second blocking request on a host. A press on a
/// *different* unpaired host while this is live gets told to finish or wait.
struct ActivePairing {
    host: String,
    /// Kept so pressing the same host again re-shows the *same* PIN — the person may be
    /// mid-typing it into Sunshine, and regenerating would invalidate their typing with
    /// no sign anything changed.
    pin: PairingPin,
}

/// One frame from a running action setting, on its way to the glass.
struct ActionReport {
    /// Which drill-in this belongs to. A frame from the one before is dropped rather
    /// than painted over its successor — the stream outlives the screen by design, so
    /// somebody who backs out and drills into something else must not be interrupted by
    /// what they left.
    epoch: u64,
    /// Which setting, so the row ids and the screen tag come out right.
    setting: String,
    report: settings::Report,
}

/// What the spawned pairing task reports back to the event loop.
struct PairingOutcome {
    host: String,
    verdict: Result<(), PairingProblem>,
}

/// Why a pairing did not complete — the panel's timeout, or the protocol's own word.
enum PairingProblem {
    /// Nobody typed the PIN before [`PAIRING_TIMEOUT`]. Dropping the pairing future
    /// abandons its held-open request; the host's pairing session goes stale and a
    /// retry starts a clean handshake with a fresh PIN.
    TimedOut,
    Failed(GameStreamError),
}

impl std::fmt::Display for PairingProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => write!(f, "no PIN was entered before the timeout"),
            Self::Failed(e) => e.fmt(f),
        }
    }
}

/// Whether a screen change goes deeper or swaps in place — which is the difference
/// between `back` meaning one step and `back` replaying a loading screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    Push,
    Replace,
}

/// What showing a host's apps concluded, beyond what it already put on screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AppsOutcome {
    /// The list (or its own failure screen) is showing; nothing more to do.
    Shown,
    /// The host does not trust us. Nothing is on screen for it yet — the caller decides,
    /// because "start pairing" is a navigation decision, not a list-fetching one.
    NotPaired,
}

/// Drives shell navigation until the event channel closes.
///
/// `gamestream` is `None` in a build or configuration where that adapter never started;
/// pressing its tile then says so on the panel rather than doing nothing, because a tile
/// that silently ignores a press is indistinguishable from a broken touchscreen.
pub async fn run(
    events: mpsc::Receiver<ShellEvent>,
    render: pipeline::RenderTx,
    gamestream: Option<Arc<GameStreamAdapter>>,
    gamestream_commands: mpsc::Sender<GameStreamCommand>,
    settings: settings::Catalog,
    osd: castaway_core::OsdSink,
    close_page: mpsc::UnboundedSender<()>,
) {
    // Sized for the one pairing that can be in flight; the extra slots are slack, not
    // a queue anyone fills.
    let (pairing_outcomes, outcomes_rx) = mpsc::channel(4);
    // A download reports a couple of hundred times over some minutes, so this is slack
    // rather than a backlog — and a full channel back-pressures the forwarding task,
    // never the agent.
    let (action_reports, reports_rx) = mpsc::channel(16);
    let nav = Nav {
        render,
        gamestream,
        gamestream_commands,
        settings,
        osd,
        chosen_host: None,
        pairing: None,
        pairing_outcomes,
        action_reports,
        action_epoch: 0,
        close_page,
    };
    nav.run(events, outcomes_rx, reports_rx).await;
}

/// The navigation state one `run` owns. A struct because pairing gave the loop *state
/// that outlives an event* — threading three `&mut` locals through every helper said
/// less than this does.
struct Nav {
    render: pipeline::RenderTx,
    gamestream: Option<Arc<GameStreamAdapter>>,
    gamestream_commands: mpsc::Sender<GameStreamCommand>,
    settings: settings::Catalog,
    osd: castaway_core::OsdSink,
    /// Which host the app picker is for. Set when a host row is pressed.
    chosen_host: Option<String>,
    /// The pairing in flight, if any. See [`ActivePairing`] for the one-at-a-time rule.
    pairing: Option<ActivePairing>,
    /// The sender half the spawned pairing task reports through. Held here so the
    /// channel outlives any individual task.
    pairing_outcomes: mpsc::Sender<PairingOutcome>,
    /// The same, for a running action setting's frames.
    action_reports: mpsc::Sender<ActionReport>,
    /// Bumped by every drill-in. See [`ActionReport::epoch`].
    action_epoch: u64,
    /// Where the close badge's press goes: the task holding the DIAL-launched page.
    close_page: mpsc::UnboundedSender<()>,
}

impl Nav {
    async fn run(
        mut self,
        mut events: mpsc::Receiver<ShellEvent>,
        mut outcomes: mpsc::Receiver<PairingOutcome>,
        mut reports: mpsc::Receiver<ActionReport>,
    ) {
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.on_event(event).await,
                    // The shell is gone: shutdown.
                    None => break,
                },
                Some(outcome) = outcomes.recv() => self.on_pairing_outcome(outcome).await,
                Some(report) = reports.recv() => self.on_action_report(&report),
            }
        }
    }

    async fn on_event(&mut self, event: ShellEvent) {
        match event {
            ShellEvent::Tile(id) if id == MOONLIGHT_TILE => {
                self.chosen_host = None;
                show_hosts(&self.render, self.gamestream.as_deref()).await;
            }
            ShellEvent::Tile(id) if id == SETTINGS_TILE => {
                show_settings_menu(&self.render, &self.settings);
            }
            ShellEvent::Tile(id) => {
                // A tile with no local screen and nothing wired to it. Say so.
                warn!(%id, "shell: a tile nothing is listening for");
                push(
                    &self.render,
                    Picker::loading("Not yet", "…").with_items(
                        vec![],
                        format!("\"{id}\" is on the screen but not wired up yet"),
                    ),
                );
            }
            ShellEvent::ClosePage => {
                // The badge on the demoted page. The launch is DIAL's, so the stop is
                // too — this just forwards the press to the task holding the service.
                info!("shell: close badge pressed; stopping the launched page");
                let _ = self.close_page.send(());
            }
            ShellEvent::Item(id) => {
                if let Some(host) = id.strip_prefix(HOST_PREFIX) {
                    self.chosen_host = Some(host.to_string());
                    let shown =
                        show_apps(&self.render, self.gamestream.as_deref(), host, Entry::Push)
                            .await;
                    if shown == AppsOutcome::NotPaired {
                        // The walk-up path: an unpaired host is an invitation to pair,
                        // right here, not an error to go edit a config file over.
                        self.start_pairing(host);
                    }
                } else if let Some(host) = id.strip_prefix(PAIR_PREFIX) {
                    // "Try again" on a failed pairing screen.
                    self.chosen_host = Some(host.to_string());
                    self.start_pairing(host);
                } else if let Some(app) = id.strip_prefix(APP_PREFIX) {
                    let Some(host) = self.chosen_host.clone() else {
                        warn!(%app, "shell: an app was chosen with no host behind it");
                        return;
                    };
                    launch(&self.render, &self.gamestream_commands, &host, app).await;
                } else if let Some(setting_id) = id.strip_prefix(SETTING_PREFIX) {
                    self.show_setting(setting_id).await;
                } else if let Some(rest) = id.strip_prefix(CHOICE_PREFIX) {
                    let Some((setting_id, choice_id)) = rest.split_once(':') else {
                        warn!(%id, "shell: a choice row with no setting in its id");
                        return;
                    };
                    apply_choice(
                        &self.render,
                        &self.osd,
                        &self.settings,
                        setting_id,
                        choice_id,
                    )
                    .await;
                } else {
                    debug!(%id, "shell: an item from a list nothing owns");
                }
            }
        }
    }

    /// Put a PIN on the glass and start the handshake, or re-show the one in flight.
    ///
    /// Always `replace`s the top screen: every path here already answered its press
    /// with a screen (the "asking the host…" loader, or the failed screen whose retry
    /// row was pressed), and the PIN screen is that screen's next state, not a deeper
    /// place — `back` from it must still mean the host list.
    fn start_pairing(&mut self, host: &str) {
        let Some(adapter) = self.gamestream.as_ref() else {
            // Unreachable in practice: pairing starts from rows only an adapter put on
            // screen. But a silent return beats a panic on a wall.
            warn!(%host, "shell: pairing pressed with no GameStream adapter");
            return;
        };
        if let Some(active) = &self.pairing {
            if active.host.eq_ignore_ascii_case(host) {
                // The same host pressed again mid-pairing: same PIN, same screen.
                replace(&self.render, pin_screen(host, active.pin));
            } else {
                replace(&self.render, pairing_busy_screen(host, &active.host));
            }
            return;
        }
        let pin = PairingPin::generate();
        // The PIN's home is the glass, not the log. debug! only, so a production log
        // never carries what gates the handshake — info! records that, not what.
        debug!(%host, %pin, "shell: pairing PIN generated");
        info!(%host, "shell: pairing started from the panel");
        replace(&self.render, pin_screen(host, pin));
        self.pairing = Some(ActivePairing {
            host: host.to_string(),
            pin,
        });
        let adapter = Arc::clone(adapter);
        let outcomes = self.pairing_outcomes.clone();
        let host = host.to_string();
        tokio::spawn(async move {
            let verdict =
                match tokio::time::timeout(PAIRING_TIMEOUT, adapter.pair(&host, &pin.to_string()))
                    .await
                {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(PairingProblem::Failed(e)),
                    Err(_) => Err(PairingProblem::TimedOut),
                };
            // A closed channel is shutdown; the pairing store already has whatever the
            // handshake achieved.
            let _ = outcomes.send(PairingOutcome { host, verdict }).await;
        });
    }

    /// A pairing task finished. The screen on top is normally the PIN screen; if the
    /// person wandered off (`back` is handled render-side and never reaches here), the
    /// replace lands on whatever they wandered to — the same already-accepted race every
    /// slow fill-in on this file has.
    async fn on_pairing_outcome(&mut self, outcome: PairingOutcome) {
        // Whatever happened, the one-at-a-time slot is free again.
        self.pairing = None;
        match outcome.verdict {
            Ok(()) => {
                info!(host = %outcome.host, "shell: paired from the panel");
                self.chosen_host = Some(outcome.host.clone());
                let shown = show_apps(
                    &self.render,
                    self.gamestream.as_deref(),
                    &outcome.host,
                    Entry::Replace,
                )
                .await;
                if shown == AppsOutcome::NotPaired {
                    // Paired, and then the host said otherwise — it forgot us that
                    // fast, or answered someone else's handshake. Offer a fresh one
                    // rather than arguing; it waits on a human, so this cannot loop hot.
                    self.start_pairing(&outcome.host);
                }
            }
            Err(problem) => {
                warn!(host = %outcome.host, %problem, "shell: panel pairing failed");
                replace(&self.render, pairing_failed_screen(&outcome.host, &problem));
            }
        }
    }
}

/// The screen someone stands in front of while typing the PIN into Sunshine.
///
/// The task's shape (title / subtitle / busy line) rather than rows, because there is
/// nothing to choose here — the screen is an instruction, and its cancel is the back
/// affordance every picker already has.
fn pin_screen(host: &str, pin: PairingPin) -> Picker {
    Picker::loading(
        format!("Pair with {host}"),
        "Waiting for the PIN — this screen moves on by itself once it is entered",
    )
    .with_subtitle(format!(
        "Enter PIN {pin} in Sunshine's web UI on {host} (https://<that PC>:47990 → PIN)"
    ))
}

/// The screen a pairing that did not complete leaves behind. Always offers a retry row,
/// because the alternative is a dead end whose only exit is knowing to press a host row
/// again — and a fresh PIN costs nothing.
fn pairing_failed_screen(host: &str, problem: &PairingProblem) -> Picker {
    let why = match problem {
        PairingProblem::TimedOut => format!(
            "No PIN was entered within {} minutes, so pairing stopped",
            PAIRING_TIMEOUT.as_secs() / 60
        ),
        // The one failure whose recovery is "type more carefully", so say that, not
        // "error" (the same split GameStreamError::WrongPin exists for).
        PairingProblem::Failed(GameStreamError::WrongPin) => {
            "The PIN typed on the host did not match the one shown here".to_string()
        }
        PairingProblem::Failed(e) => friendly(e),
    };
    Picker::loading(format!("Pair with {host}"), "")
        .with_items(
            vec![PickerItem::new(format!("{PAIR_PREFIX}{host}"), "Try again")
                .with_detail("Shows a fresh PIN")],
            String::new(),
        )
        .failed(why)
}

/// What a press on a second unpaired host sees while a pairing is already in flight.
fn pairing_busy_screen(host: &str, busy_with: &str) -> Picker {
    Picker::loading(format!("Pair with {host}"), "").failed(format!(
        "Already pairing with {busy_with} — finish that first, or let it time out"
    ))
}

/// The settings menu: one row per setting, its current value underneath.
///
/// Synchronous, because it is all local: titles and summaries come from state already
/// in hand, and a menu that flashed "loading…" for that would be theatre.
fn show_settings_menu(render: &pipeline::RenderTx, catalog: &settings::Catalog) {
    let items: Vec<PickerItem> = catalog
        .all()
        .iter()
        .map(|s| {
            PickerItem::new(format!("{SETTING_PREFIX}{}", s.id()), s.title())
                .with_detail(s.summary())
        })
        .collect();
    push(
        render,
        Picker::loading("Settings", "")
            .with_items(items, "This build has nothing to configure yet".to_string()),
    );
}

/// One setting's choice list, freshly enumerated. Blocking work (device enumeration
/// asks a sound server), so it runs off the async loop.
///
/// Takes both halves rather than re-deriving the second: the drill-down borrow cannot be
/// held across the `spawn_blocking` hop, so the caller inside that hop is where it is
/// taken, and this is what it hands over.
fn choice_picker(setting: &dyn settings::Setting, choices: &dyn settings::Choices) -> Picker {
    match choices.choices() {
        Ok(list) => {
            let items: Vec<PickerItem> = list
                .choices
                .into_iter()
                .map(|c| {
                    let mut item = PickerItem::new(
                        format!("{CHOICE_PREFIX}{}:{}", setting.id(), c.id),
                        c.label,
                    )
                    .with_marked(c.current);
                    if let Some(detail) = c.detail {
                        item = item.with_detail(detail);
                    }
                    item
                })
                .collect();
            let picker = match list.subtitle {
                Some(sub) => Picker::loading(setting.title(), "").with_subtitle(sub),
                None => Picker::loading(setting.title(), ""),
            };
            picker.with_items(items, list.empty_message)
        }
        Err(why) => Picker::loading(setting.title(), "").failed(why),
    }
}

/// A frame from a running action setting, as a screen.
///
/// Pure, and the only thing that turns the catalog's words into a picker — which is what
/// makes every screen this feature can put on the glass assertable without an agent, a
/// network or a clock.
fn action_screen(title: String, setting_id: &str, report: &settings::Report) -> Picker {
    let items: Vec<PickerItem> = report
        .rows
        .iter()
        .map(|row| {
            let item = PickerItem::new(
                format!("{CHOICE_PREFIX}{setting_id}:{}", row.id),
                row.label.clone(),
            );
            match &row.detail {
                Some(detail) => item.with_detail(detail.clone()),
                None => item,
            }
        })
        .collect();
    let picker = Picker::loading(title, "").with_tag(setting_id);
    match report.stage {
        // A wait, and it must read as one: the sentence is the busy line, and there is
        // nothing to press while it is going on.
        Stage::Working => picker.busy(report.say.clone()),
        // With rows, the sentence is what the person is deciding on, so it goes above
        // them; with none, it *is* the screen.
        Stage::Settled if items.is_empty() => picker.with_items(items, report.say.clone()),
        Stage::Settled => picker
            .with_subtitle(report.say.clone())
            .with_items(items, String::new()),
        // Rows survive a failure, the way a failed pairing keeps its retry.
        Stage::Failed => picker
            .with_items(items, String::new())
            .failed(report.say.clone()),
    }
}

impl Nav {
    /// Drill into one setting: answer the press, then fill it in — either with a list, or
    /// with the first frame of something that has started happening.
    async fn show_setting(&mut self, setting_id: &str) {
        let Some(setting) = self.settings.get(setting_id) else {
            warn!(%setting_id, "shell: a settings row for a setting this build lacks");
            return;
        };
        // Any drill-in retires whatever was being followed. The work it was following is
        // not stopped — this loop never owned it — only its frames stop landing.
        self.action_epoch = self.action_epoch.wrapping_add(1);
        match setting.drilldown() {
            settings::Drilldown::Choices(_) => {
                push(&self.render, Picker::loading(setting.title(), "Looking…"));
                let worker = Arc::clone(&setting);
                let listed = tokio::task::spawn_blocking(move || match worker.drilldown() {
                    Drilldown::Choices(choices) => Some(choice_picker(worker.as_ref(), choices)),
                    // Only reachable if a setting changed shape between two calls, which
                    // no implementation does; the alternative is an `unwrap` on a wall.
                    Drilldown::Action(_) => None,
                })
                .await;
                match listed {
                    Ok(Some(picker)) => replace(&self.render, picker),
                    Ok(None) | Err(_) => {
                        warn!(%setting_id, "shell: enumerating a setting's choices did not answer");
                        replace(
                            &self.render,
                            Picker::loading(setting.title(), "")
                                .failed("Something went wrong listing these"),
                        );
                    }
                }
            }
            settings::Drilldown::Action(action) => {
                // Answered now, tagged, and filled in by the stream. Tagged from the very
                // first screen, because the frame that replaces it may arrive after the
                // person has already left.
                push(
                    &self.render,
                    Picker::loading(setting.title(), "…").with_tag(setting.id()),
                );
                let mut frames = action.start();
                let epoch = self.action_epoch;
                let out = self.action_reports.clone();
                let id = setting.id().to_string();
                info!(%setting_id, "shell: a settings action started");
                tokio::spawn(async move {
                    while let Some(report) = frames.recv().await {
                        let hop = ActionReport {
                            epoch,
                            setting: id.clone(),
                            report,
                        };
                        // A closed channel is shutdown. The action carries on regardless
                        // — it is not ours to stop, and a download half-done is still a
                        // download the next boot can use.
                        if out.send(hop).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }
    }

    /// One frame, on the glass — if the person is still looking at the screen it is for.
    fn on_action_report(&self, report: &ActionReport) {
        if report.epoch != self.action_epoch {
            debug!(setting = %report.setting, "shell: a frame from an action nobody is on");
            return;
        }
        let Some(setting) = self.settings.get(&report.setting) else {
            return;
        };
        // Tagged rather than plain: `back` never reaches this file, so the stack is what
        // decides whether this screen is still the one on top.
        replace_tagged(
            &self.render,
            &report.setting,
            action_screen(setting.title(), &report.setting, &report.report),
        );
    }
}

/// Apply a picked choice, then show the list again with the mark moved.
///
/// The refreshed list *replaces* the one on screen: the person is exactly where they
/// were, with the check on the row they pressed. A choice that applied but could not be
/// *saved* still shows the moved check — the running receiver honours it — and the save
/// failure goes to the OSD as a toast, because it is news about a file, not about the
/// list, and the screen should carry on working either way.
async fn apply_choice(
    render: &pipeline::RenderTx,
    osd: &castaway_core::OsdSink,
    catalog: &settings::Catalog,
    setting_id: &str,
    choice_id: &str,
) {
    let Some(setting) = catalog.get(setting_id) else {
        warn!(%setting_id, "shell: a choice for a setting this build lacks");
        return;
    };
    if let Drilldown::Action(action) = setting.drilldown() {
        // An action's rows are not choices: pressing one starts work whose answer arrives
        // on the stream that is already running, so there is nothing to render here and
        // nothing to wait for. Rendering anything would race that stream.
        info!(%setting_id, %choice_id, "settings: an action row was pressed");
        action.press(choice_id);
        return;
    }
    let worker = Arc::clone(&setting);
    let chosen = choice_id.to_owned();
    let outcome = tokio::task::spawn_blocking(move || match worker.drilldown() {
        Drilldown::Choices(choices) => {
            let applied = choices.apply(&chosen);
            // Re-enumerate under the same blocking hop: the list shown after a pick must
            // be the list as it is now, not as it was before the device moved.
            (applied, choice_picker(worker.as_ref(), choices))
        }
        // Unreachable: the shape was checked a line ago and no setting changes it.
        Drilldown::Action(_) => (
            Err("this setting is not a list".to_string()),
            Picker::loading(worker.title(), ""),
        ),
    })
    .await;
    match outcome {
        Ok((Ok(Applied::Saved), picker)) => {
            info!(%setting_id, %choice_id, "settings: applied and saved");
            replace(render, picker);
        }
        Ok((Ok(Applied::NotSaved(why)), picker)) => {
            // Applied but not persisted — the panel works and the file disagrees, and
            // the toast is the only thing standing between that and a mystery on the
            // next restart.
            warn!(%setting_id, %why, "settings: applied but not saved");
            osd.banner(why, std::time::Duration::from_secs(8));
            replace(render, picker);
        }
        Ok((Err(why), picker)) => {
            // Nothing changed; this one belongs to the list it came from.
            warn!(%setting_id, %choice_id, %why, "settings: refused");
            replace(render, picker.failed(why));
        }
        Err(e) => {
            warn!(%setting_id, error = %e, "shell: applying a setting died");
            replace(
                render,
                Picker::loading(setting.title(), "").failed("Something went wrong applying that"),
            );
        }
    }
}

/// Show the hosts the GameStream adapter has discovered.
async fn show_hosts(render: &pipeline::RenderTx, adapter: Option<&GameStreamAdapter>) {
    let Some(adapter) = adapter else {
        push(
            render,
            Picker::loading("Moonlight", "…").with_items(
                vec![],
                "Moonlight is switched off in this receiver's config".to_string(),
            ),
        );
        return;
    };

    // Answer the press first, then fill it in. mDNS discovery is not instant, and a
    // screen that appears only once it finishes reads as a panel that ignored the tap.
    push(
        render,
        Picker::loading("Moonlight", "Looking for hosts…")
            .with_subtitle("Gaming PCs running Sunshine on this network"),
    );

    let hosts = adapter.hosts().await;
    let items: Vec<PickerItem> = hosts
        .iter()
        .map(|h| {
            PickerItem::new(format!("{HOST_PREFIX}{}", h.name), h.name.clone())
                .with_detail(h.address.to_string())
        })
        .collect();
    info!(count = items.len(), "shell: showing Moonlight hosts");
    // Replace, not push: the "looking…" screen and this are one step, and `back` must
    // mean one step too (`ScreenStack::replace_top`'s whole reason to exist).
    replace(
        render,
        Picker::loading("Moonlight", "")
            .with_subtitle("Gaming PCs running Sunshine on this network")
            .with_items(
                items,
                "No hosts found. Is Sunshine running, and on this network?".to_string(),
            ),
    );
}

/// Show what a host offers.
///
/// `entry` is [`Entry::Push`] from a host-row press and [`Entry::Replace`] when a
/// finished pairing refreshes into this — same screen, different way of arriving.
async fn show_apps(
    render: &pipeline::RenderTx,
    adapter: Option<&GameStreamAdapter>,
    host: &str,
    entry: Entry,
) -> AppsOutcome {
    let asking = Picker::loading(host.to_string(), "Asking the host…");
    match entry {
        Entry::Push => push(render, asking),
        Entry::Replace => replace(render, asking),
    }
    let Some(adapter) = adapter else {
        return AppsOutcome::Shown;
    };
    match adapter.apps_for(host).await {
        Ok(apps) => {
            let items: Vec<PickerItem> = apps
                .iter()
                .map(|a| PickerItem::new(format!("{APP_PREFIX}{}", a.title), a.title.clone()))
                .collect();
            replace(
                render,
                Picker::loading(host.to_string(), "")
                    .with_items(items, "This host lists nothing to launch".to_string()),
            );
            AppsOutcome::Shown
        }
        // Not an error to print — the caller starts pairing, which is what an unpaired
        // host on a walk-up panel *means*.
        Err(GameStreamError::NotPaired { .. }) => AppsOutcome::NotPaired,
        Err(e) => {
            // The host's own words where there are any — "is a display connected and
            // turned on?" is a better thing to read than "error".
            warn!(%host, error = %e, "shell: could not list apps");
            replace(
                render,
                Picker::loading(host.to_string(), "").failed(friendly(&e)),
            );
            AppsOutcome::Shown
        }
    }
}

/// Ask the adapter to start streaming, and hand the panel back to it.
async fn launch(
    render: &pipeline::RenderTx,
    commands: &mpsc::Sender<GameStreamCommand>,
    host: &str,
    app: &str,
) {
    info!(%host, %app, "shell: launching");
    push(
        render,
        Picker::loading(app.to_string(), format!("Starting on {host}…")),
    );
    if commands
        .send(GameStreamCommand::Start {
            host: host.to_string(),
            app: Some(app.to_string()),
        })
        .await
        .is_err()
    {
        warn!("shell: the GameStream adapter is gone");
    }
    // The stream composites *above* the shell, so there is no need to navigate away —
    // and every reason not to: if the launch fails, whatever the picker last said is
    // still on screen underneath, which is the thing worth reading.
}

/// Turn a protocol error into something worth reading on a wall.
fn friendly(e: &GameStreamError) -> String {
    use GameStreamError as E;
    match e {
        E::NotPaired { .. } => {
            "Not paired with this host yet. Press it in the host list to pair.".to_string()
        }
        E::Nvhttp { message, .. } if !message.is_empty() => message.clone(),
        other => other.to_string(),
    }
}

fn push(render: &pipeline::RenderTx, picker: Picker) {
    // A screen push is a state transition, so it rides the lossless control lane —
    // a tile press that silently went nowhere is exactly what the lane exists to end.
    render.send(RenderCommand::PushScreen(Box::new(Screen::Picker(
        Box::new(picker),
    ))));
}

/// Swap the screen on top for `picker` without going deeper — what every "answered the
/// press, now filling it in" update uses, so `back` stays one step.
fn replace(render: &pipeline::RenderTx, picker: Picker) {
    render.send(RenderCommand::ReplaceScreen(Box::new(Screen::Picker(
        Box::new(picker),
    ))));
}

/// The same, but only while `tag`'s screen is still the one on top.
///
/// What a producer that outlives its press uses. `back` is answered render-side and never
/// arrives here, so an unconditional replace from a four-minute download would paint
/// progress over the settings menu — or, from Home, push a picker over the idle screen.
fn replace_tagged(render: &pipeline::RenderTx, tag: &str, picker: Picker) {
    render.send(RenderCommand::ReplaceScreenTagged {
        screen: Box::new(Screen::Picker(Box::new(picker))),
        tag: tag.to_string(),
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use pipeline::picker::PickerStatus;

    use super::*;

    #[test]
    fn the_pin_screen_puts_the_pin_in_the_instructions_zero_padded() {
        // The PIN on the glass and the PIN in the handshake must be the same text; a
        // screen that showed "7" while the handshake hashed "0007" would fail every
        // pairing whose PIN starts with a zero. `PairingPin`'s Display is the one
        // rendering, and this pins the screen to it.
        let pin = PairingPin::from_value(7).unwrap();
        let screen = pin_screen("somepc", pin);
        assert_eq!(screen.title, "Pair with somepc");
        let sub = screen.subtitle.unwrap();
        assert!(
            sub.contains("0007"),
            "the PIN is not in the instructions: {sub}"
        );
        assert!(
            sub.contains("47990"),
            "the instructions do not say where to type it: {sub}"
        );
        // Busy, not Ready: the screen is a wait, and must read as one.
        assert!(matches!(screen.status, PickerStatus::Busy(_)));
        assert!(screen.items.is_empty(), "nothing to choose while waiting");
    }

    #[test]
    fn a_failed_pairing_offers_a_retry_row_that_routes_back_to_pairing() {
        // The retry row's id must survive the same strip_prefix the handler does, or
        // the failed screen's one exit goes to the "list nothing owns" branch.
        let screen = pairing_failed_screen("somepc", &PairingProblem::TimedOut);
        assert!(matches!(&screen.status, PickerStatus::Failed(why) if why.contains("minutes")));
        assert_eq!(screen.items.len(), 1);
        assert_eq!(screen.items[0].id.strip_prefix(PAIR_PREFIX), Some("somepc"));
    }

    #[test]
    fn a_wrong_pin_reads_as_a_typo_not_a_generic_failure() {
        // GameStreamError keeps WrongPin apart from Pairing because the recoveries
        // differ; the screen must keep them apart too, or the split upstream buys
        // nothing on the glass.
        let wrong = pairing_failed_screen("pc", &PairingProblem::Failed(GameStreamError::WrongPin));
        let PickerStatus::Failed(why) = &wrong.status else {
            panic!("not failed: {:?}", wrong.status);
        };
        assert!(
            why.contains("did not match"),
            "unhelpful wrong-PIN text: {why}"
        );
        let timed_out = pairing_failed_screen("pc", &PairingProblem::TimedOut);
        assert_ne!(wrong.status, timed_out.status, "two causes, one message");
    }

    #[test]
    fn a_second_hosts_pairing_press_names_the_host_already_pairing() {
        let screen = pairing_busy_screen("otherpc", "somepc");
        let PickerStatus::Failed(why) = &screen.status else {
            panic!("not failed: {:?}", screen.status);
        };
        assert!(
            why.contains("somepc"),
            "the message must say which pairing is in the way: {why}"
        );
    }

    /// The three shapes an action's frame can take, as the settings catalog hands them
    /// over.
    fn frame(stage: Stage, say: &str, rows: Vec<settings::Row>) -> settings::Report {
        settings::Report {
            say: say.to_string(),
            stage,
            rows,
        }
    }

    #[test]
    fn an_action_in_flight_reads_as_a_wait_with_its_own_words_and_nothing_to_press() {
        // Four minutes of a spinner reads as a hang; four minutes of a number that
        // climbs reads as a download. The words are the setting's, not this file's.
        let screen = action_screen(
            "Check for updates".into(),
            "update-check",
            &frame(Stage::Working, "46% — 108 of 236 MB", vec![]),
        );
        assert_eq!(
            screen.status,
            PickerStatus::Busy("46% — 108 of 236 MB".into())
        );
        assert!(screen.items.is_empty(), "nothing to choose mid-download");
    }

    #[test]
    fn an_answer_with_something_to_press_shows_both_the_sentence_and_the_row() {
        // The offer: the sentence is what the person is deciding on, so it has to be
        // visible *alongside* the row rather than in the empty-list message, which a
        // screen with a row never shows.
        let screen = action_screen(
            "Check for updates".into(),
            "update-check",
            &frame(
                Stage::Settled,
                "Build 957 (efc4316) is available — 237 MB.",
                vec![settings::Row {
                    id: "take-it".into(),
                    label: "Download and restart now".into(),
                    detail: Some("The panel goes blank".into()),
                }],
            ),
        );
        assert_eq!(
            screen.subtitle.as_deref(),
            Some("Build 957 (efc4316) is available — 237 MB.")
        );
        assert_eq!(screen.status, PickerStatus::Ready);
        let [row] = screen.items.as_slice() else {
            panic!("{:?}", screen.items);
        };
        // The id must survive the same `strip_prefix` and `split_once` the handler does,
        // or the offer's one exit goes to the "list nothing owns" branch.
        let rest = row.id.strip_prefix(CHOICE_PREFIX).expect("namespaced");
        assert_eq!(rest.split_once(':'), Some(("update-check", "take-it")));
        assert_eq!(row.detail.as_deref(), Some("The panel goes blank"));
    }

    #[test]
    fn an_answer_with_nothing_to_press_is_the_sentence_itself() {
        let screen = action_screen(
            "Check for updates".into(),
            "update-check",
            &frame(Stage::Settled, "This panel is running build 957.", vec![]),
        );
        assert_eq!(
            screen.status,
            PickerStatus::Empty("This panel is running build 957.".into())
        );
        // Not repeated above the (absent) list as well — one sentence, once.
        assert_eq!(screen.subtitle, None);
    }

    #[test]
    fn a_failed_action_keeps_its_reason_and_whatever_it_can_still_offer() {
        let screen = action_screen(
            "Check for updates".into(),
            "update-check",
            &frame(
                Stage::Failed,
                "fetching the release listing: dns error",
                vec![settings::Row {
                    id: "again".into(),
                    label: "Try again".into(),
                    detail: None,
                }],
            ),
        );
        assert!(
            matches!(&screen.status, PickerStatus::Failed(why) if why.contains("dns error")),
            "{:?}",
            screen.status
        );
        assert_eq!(screen.items.len(), 1, "a retry survives the failure");
    }

    #[test]
    fn every_frame_of_an_action_carries_the_tag_that_stops_it_landing_on_the_wrong_screen() {
        // The one property the whole tagged-replace mechanism rests on. An untagged
        // frame would be refused by the stack outright — worse, it would be *silently*
        // refused, so a screen that never updates is the symptom.
        for stage in [Stage::Working, Stage::Settled, Stage::Failed] {
            let screen = action_screen(
                "Check for updates".into(),
                "update-check",
                &frame(stage, "x", vec![]),
            );
            assert_eq!(
                screen.tag.as_deref(),
                Some("update-check"),
                "an untagged frame at {stage:?}"
            );
        }
    }

    #[test]
    fn the_not_paired_error_no_longer_sends_people_to_a_config_file() {
        // The whole point of panel-initiated pairing: the config file is not the
        // answer a wall panel gives.
        let text = friendly(&GameStreamError::NotPaired {
            host: "somepc".into(),
        });
        assert!(!text.to_lowercase().contains("config"), "{text}");
    }
}
