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

/// The id of the Moonlight tile on the Home screen. Matches what `build_attract` emits.
const MOONLIGHT_TILE: &str = "gamestream";

/// Picker item ids are namespaced, because one handler serves every picker and a bare id
/// would not say which list it came from — an app called `10.0.0.7` is not impossible.
const HOST_PREFIX: &str = "host:";
const APP_PREFIX: &str = "app:";

/// Drives shell navigation until the event channel closes.
///
/// `gamestream` is `None` in a build or configuration where that adapter never started;
/// pressing its tile then says so on the panel rather than doing nothing, because a tile
/// that silently ignores a press is indistinguishable from a broken touchscreen.
pub async fn run(
    mut events: mpsc::Receiver<ShellEvent>,
    render: std::sync::mpsc::SyncSender<RenderCommand>,
    gamestream: Option<Arc<GameStreamAdapter>>,
    gamestream_commands: mpsc::Sender<GameStreamCommand>,
) {
    // Which host the app picker is for. Set when a host row is pressed.
    let mut chosen_host: Option<String> = None;

    while let Some(event) = events.recv().await {
        match event {
            ShellEvent::Tile(id) if id == MOONLIGHT_TILE => {
                chosen_host = None;
                show_hosts(&render, gamestream.as_deref()).await;
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
                } else {
                    debug!(%id, "shell: an item from a list nothing owns");
                }
            }
        }
    }
}

/// Show the hosts the GameStream adapter has discovered.
async fn show_hosts(
    render: &std::sync::mpsc::SyncSender<RenderCommand>,
    adapter: Option<&GameStreamAdapter>,
) {
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
    push(
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
async fn show_apps(
    render: &std::sync::mpsc::SyncSender<RenderCommand>,
    adapter: Option<&GameStreamAdapter>,
    host: &str,
) {
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
            push(
                render,
                Picker::loading(host.to_string(), "")
                    .with_items(items, "This host lists nothing to launch".to_string()),
            );
        }
        Err(e) => {
            // The host's own words where there are any — "is a display connected and
            // turned on?" is a better thing to read than "error".
            warn!(%host, error = %e, "shell: could not list apps");
            push(
                render,
                Picker::loading(host.to_string(), "").failed(friendly(&e)),
            );
        }
    }
}

/// Ask the adapter to start streaming, and hand the panel back to it.
async fn launch(
    render: &std::sync::mpsc::SyncSender<RenderCommand>,
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

fn push(render: &std::sync::mpsc::SyncSender<RenderCommand>, picker: Picker) {
    // Drop-on-full, like every other render command: a shell update that cannot get
    // through is one frame of staleness, not a reason to block.
    if render
        .try_send(RenderCommand::PushScreen(Box::new(Screen::Picker(
            Box::new(picker),
        ))))
        .is_err()
    {
        debug!("shell: the render channel is full or closed");
    }
}
