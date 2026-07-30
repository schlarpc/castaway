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

use std::sync::Arc;

use pipeline::picker::{Picker, PickerItem};
use pipeline::render_pipeline::RenderCommand;
use pipeline::shell::{Screen, ShellEvent};
use proto_gamestream::{GameStreamAdapter, GameStreamCommand};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::settings::{self, Applied};

/// The id of the Moonlight tile on the Home screen. Matches what `build_attract` emits.
const MOONLIGHT_TILE: &str = "gamestream";
/// The id of the Settings tile. Matches what `build_attract` emits.
const SETTINGS_TILE: &str = "settings";

/// Picker item ids are namespaced, because one handler serves every picker and a bare id
/// would not say which list it came from — an app called `10.0.0.7` is not impossible.
const HOST_PREFIX: &str = "host:";
const APP_PREFIX: &str = "app:";
/// A row on the settings menu: `setting:<setting id>`.
const SETTING_PREFIX: &str = "setting:";
/// A row on one setting's choice list: `choose:<setting id>:<choice id>`. The setting
/// id is a slug with no `:` in it; the choice id is opaque and may contain anything.
const CHOICE_PREFIX: &str = "choose:";

/// Drives shell navigation until the event channel closes.
///
/// `gamestream` is `None` in a build or configuration where that adapter never started;
/// pressing its tile then says so on the panel rather than doing nothing, because a tile
/// that silently ignores a press is indistinguishable from a broken touchscreen.
pub async fn run(
    mut events: mpsc::Receiver<ShellEvent>,
    render: pipeline::RenderTx,
    gamestream: Option<Arc<GameStreamAdapter>>,
    gamestream_commands: mpsc::Sender<GameStreamCommand>,
    settings: settings::Catalog,
    osd: castaway_core::OsdSink,
) {
    // Which host the app picker is for. Set when a host row is pressed.
    let mut chosen_host: Option<String> = None;

    while let Some(event) = events.recv().await {
        match event {
            ShellEvent::Tile(id) if id == MOONLIGHT_TILE => {
                chosen_host = None;
                show_hosts(&render, gamestream.as_deref()).await;
            }
            ShellEvent::Tile(id) if id == SETTINGS_TILE => {
                show_settings_menu(&render, &settings);
            }
            ShellEvent::Tile(id) => {
                // A tile with no local screen and nothing wired to it. Say so.
                warn!(%id, "shell: a tile nothing is listening for");
                push(
                    &render,
                    Picker::loading("Not yet", "…").with_items(
                        vec![],
                        format!("\"{id}\" is on the screen but not wired up yet"),
                    ),
                );
            }
            ShellEvent::Item(id) => {
                if let Some(host) = id.strip_prefix(HOST_PREFIX) {
                    chosen_host = Some(host.to_string());
                    show_apps(&render, gamestream.as_deref(), host).await;
                } else if let Some(app) = id.strip_prefix(APP_PREFIX) {
                    let Some(host) = chosen_host.clone() else {
                        warn!(%app, "shell: an app was chosen with no host behind it");
                        continue;
                    };
                    launch(&render, &gamestream_commands, &host, app).await;
                } else if let Some(setting_id) = id.strip_prefix(SETTING_PREFIX) {
                    show_setting(&render, &settings, setting_id).await;
                } else if let Some(rest) = id.strip_prefix(CHOICE_PREFIX) {
                    let Some((setting_id, choice_id)) = rest.split_once(':') else {
                        warn!(%id, "shell: a choice row with no setting in its id");
                        continue;
                    };
                    apply_choice(&render, &osd, &settings, setting_id, choice_id).await;
                } else {
                    debug!(%id, "shell: an item from a list nothing owns");
                }
            }
        }
    }
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
fn choice_picker(setting: &dyn settings::Setting) -> Picker {
    match setting.choices() {
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

/// Drill into one setting: answer the press, then fill the list in.
async fn show_setting(render: &pipeline::RenderTx, catalog: &settings::Catalog, setting_id: &str) {
    let Some(setting) = catalog.get(setting_id) else {
        warn!(%setting_id, "shell: a settings row for a setting this build lacks");
        return;
    };
    push(render, Picker::loading(setting.title(), "Looking…"));
    let worker = Arc::clone(&setting);
    match tokio::task::spawn_blocking(move || choice_picker(worker.as_ref())).await {
        Ok(picker) => replace(render, picker),
        Err(e) => {
            warn!(%setting_id, error = %e, "shell: enumerating a setting's choices died");
            replace(
                render,
                Picker::loading(setting.title(), "").failed("Something went wrong listing these"),
            );
        }
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
    let worker = Arc::clone(&setting);
    let chosen = choice_id.to_owned();
    let outcome = tokio::task::spawn_blocking(move || {
        let applied = worker.apply(&chosen);
        // Re-enumerate under the same blocking hop: the list shown after a pick must
        // be the list as it is now, not as it was before the device moved.
        (applied, choice_picker(worker.as_ref()))
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
async fn show_apps(render: &pipeline::RenderTx, adapter: Option<&GameStreamAdapter>, host: &str) {
    push(
        render,
        Picker::loading(host.to_string(), "Asking the host…"),
    );
    let Some(adapter) = adapter else {
        return;
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
        }
        Err(e) => {
            // The host's own words where there are any — "is a display connected and
            // turned on?" is a better thing to read than "error".
            warn!(%host, error = %e, "shell: could not list apps");
            replace(
                render,
                Picker::loading(host.to_string(), "").failed(friendly(&e)),
            );
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
        return;
    }
    // The stream composites *above* the shell, so there is no need to navigate away —
    // and every reason not to: if the launch fails, whatever the picker last said is
    // still on screen underneath, which is the thing worth reading.
}

/// Turn a protocol error into something worth reading on a wall.
fn friendly(e: &proto_gamestream::GameStreamError) -> String {
    use proto_gamestream::GameStreamError as E;
    match e {
        E::NotPaired { .. } => {
            "Not paired with this host yet. Pair it from the receiver's config.".to_string()
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
