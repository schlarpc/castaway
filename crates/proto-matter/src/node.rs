//! The endpoint tree a Casting Client sees, and the cluster handlers behind it.
//!
//! Three shapes of endpoint:
//!
//! - **0** — the root node. `rs-matter`'s own system clusters: Basic Information,
//!   Operational Credentials, Access Control, General Commissioning. Untouched by us.
//! - **1** — the Casting Video Player. `ContentLauncher` (a URL the panel plays itself),
//!   `MediaPlayback` (the transport), and `TargetNavigator` (the list of content apps,
//!   which is how a client that has not read the descriptor finds them).
//! - **6…** — one Content App per thing the panel can open. `ApplicationBasic` is what a
//!   client matches its own app against, so those fields are the endpoint's address, not
//!   decoration.
//!
//! Every handler here is a shell over [`crate::player`]: it decodes the invoke, asks the
//! catalogue what it means, pushes a [`CastCommand`], and answers in the cluster's own
//! vocabulary. Nothing in this module decides anything.
//!
//! ## One handler, many endpoints
//!
//! `rs-matter` composes handlers into a chain whose type is built at compile time, so
//! there can be no per-endpoint handler for a set of endpoints that comes from config.
//! Instead each handler matches its cluster on *any* application endpoint and dispatches
//! on `ctx.endpt()`. The metadata is the part that varies per endpoint, and that is
//! ordinary runtime data.

use std::sync::Arc;

use rs_matter::devices;
use rs_matter::dm::clusters::decl::{
    application_basic, content_launcher, media_playback, target_navigator,
};
use rs_matter::dm::clusters::desc::{self, DescHandler};
use rs_matter::dm::devices::DEV_TYPE_CASTING_VIDEO_PLAYER;
use rs_matter::dm::endpoints::{EthSysHandlerBuilder, ROOT_ENDPOINT_ID};
use rs_matter::dm::{
    ArrayAttributeRead, Async, Cluster, Dataver, DeviceType, Endpoint, InvokeContext, MatchContext,
    Matcher, Node, ReadContext,
};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::{TLVBuilderParent, Utf8StrBuilder};

use rand_core::RngCore;
use tokio::sync::mpsc;

use crate::player::{
    CastCommand, Catalogue, EndpointId, LaunchRefusal, PlaybackState, PlayerState, Transport,
    PLAYER_ENDPOINT,
};

/// The Content App device type (0x0024). Not among `rs-matter`'s constants, because
/// nothing but a casting receiver has one.
pub const DEV_TYPE_CONTENT_APP: DeviceType = DeviceType {
    dtype: 0x0024,
    drev: 1,
};

/// `ContentLauncher` feature bits: URL playback and content search.
const CONTENT_LAUNCHER_FEATURES: u32 = 0b11;

/// `MediaPlayback` feature bits: advanced seek. Not variable speed — the panel's players
/// do not all have a rate control, and advertising one that silently does nothing is
/// worse than not having it.
const MEDIA_PLAYBACK_FEATURES: u32 = 0b1;

/// Matches a cluster on any endpoint *except* the root.
///
/// `EpClMatcher::new(None, Some(id))` would match endpoint 0 too, and since the most
/// recently chained handler is tried first, that would shadow the root node's own
/// Descriptor with ours.
#[derive(Debug, Clone, Copy)]
struct AppCluster(u32);

impl Matcher for AppCluster {
    fn matches(&self, ctx: impl MatchContext) -> bool {
        ctx.endpt().is_some_and(|ep| ep != ROOT_ENDPOINT_ID) && ctx.cluster() == Some(self.0)
    }
}

/// Everything the cluster handlers share: what the panel can open, what it is doing, and
/// where to send what a client asks for.
#[derive(Debug)]
pub struct CastingContext {
    /// The apps this panel hosts.
    pub catalogue: Catalogue,
    /// What the panel is playing, as `MediaPlayback` reports it.
    pub state: Arc<PlayerState>,
    /// Where a decoded invoke goes.
    pub commands: mpsc::UnboundedSender<CastCommand>,
}

impl CastingContext {
    /// Push a command at the adapter.
    ///
    /// A closed channel means the adapter is gone, which for a cluster invoke is a
    /// `Busy` — the session is being torn down and the client should retry, not conclude
    /// that the command was rejected.
    fn send(&self, command: CastCommand) -> Result<(), Error> {
        self.commands
            .send(command)
            .map_err(|_| Error::from(ErrorCode::Busy))
    }

    /// The endpoint an invoke arrived on.
    fn endpoint(ctx: &impl InvokeContext) -> Result<EndpointId, Error> {
        ctx.endpt()
            .ok_or_else(|| ErrorCode::EndpointNotFound.into())
    }
}

/// The `ContentLauncher` server: "open this", in the two forms the cluster has.
#[derive(Debug)]
pub struct ContentLauncherHandler {
    ctx: Arc<CastingContext>,
    dataver: Dataver,
}

impl ContentLauncherHandler {
    /// Build the handler.
    pub fn new(ctx: Arc<CastingContext>, dataver: Dataver) -> Self {
        Self { ctx, dataver }
    }

    /// Map a refusal onto the status the cluster has for it, so the phone can say
    /// something true to the person holding it rather than "something went wrong" (D32).
    const fn status(refusal: LaunchRefusal) -> content_launcher::StatusEnum {
        match refusal {
            LaunchRefusal::UrlNotAvailable => content_launcher::StatusEnum::URLNotAvailable,
            LaunchRefusal::NotAllowed | LaunchRefusal::NoAppFound => {
                content_launcher::StatusEnum::AuthFailed
            }
        }
    }
}

impl content_launcher::ClusterHandler for ContentLauncherHandler {
    const CLUSTER: Cluster<'static> =
        content_launcher::FULL_CLUSTER.with_features(CONTENT_LAUNCHER_FEATURES);

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn supported_streaming_protocols(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<content_launcher::SupportedProtocolsBitmap, Error> {
        // DASH and HLS, which is what the panel's decoder opens over HTTP. Not a promise
        // about codecs — a client asks this to choose a manifest, not a profile.
        Ok(content_launcher::SupportedProtocolsBitmap::DASH
            | content_launcher::SupportedProtocolsBitmap::HLS)
    }

    fn handle_launch_url<P: TLVBuilderParent>(
        &self,
        ctx: impl InvokeContext,
        request: content_launcher::LaunchURLRequest<'_>,
        response: content_launcher::LauncherResponseBuilder<P>,
    ) -> Result<P, Error> {
        let endpoint = CastingContext::endpoint(&ctx)?;
        let url = request.content_url()?;
        let title = request.display_string()?;

        match self.ctx.catalogue.launch_url(endpoint, url, title) {
            Ok(command) => {
                self.ctx.send(command)?;
                response
                    .status(content_launcher::StatusEnum::Success)?
                    .data(None)?
                    .end()
            }
            Err(refusal) => {
                tracing::info!(endpoint, %url, ?refusal, "matter: declining a LaunchURL");
                response.status(Self::status(refusal))?.data(None)?.end()
            }
        }
    }

    fn handle_launch_content<P: TLVBuilderParent>(
        &self,
        ctx: impl InvokeContext,
        request: content_launcher::LaunchContentRequest<'_>,
        response: content_launcher::LauncherResponseBuilder<P>,
    ) -> Result<P, Error> {
        let endpoint = CastingContext::endpoint(&ctx)?;
        let autoplay = request.auto_play()?;

        // A search is a list of parameters — actor, channel, title, and so on. The panel
        // has one thing to do with any of them, which is put the string in a search box,
        // so they are joined rather than interpreted.
        let mut query = String::new();
        for parameter in request.search()?.parameter_list()?.iter() {
            let parameter = parameter?;
            if !query.is_empty() {
                query.push(' ');
            }
            query.push_str(parameter.value()?);
        }

        match self.ctx.catalogue.launch_search(endpoint, &query, autoplay) {
            Ok(command) => {
                self.ctx.send(command)?;
                response
                    .status(content_launcher::StatusEnum::Success)?
                    .data(None)?
                    .end()
            }
            Err(refusal) => {
                tracing::info!(endpoint, %query, ?refusal, "matter: declining a LaunchContent");
                response.status(Self::status(refusal))?.data(None)?.end()
            }
        }
    }
}

/// The `MediaPlayback` server: the transport, and where playback has got to.
#[derive(Debug)]
pub struct MediaPlaybackHandler {
    ctx: Arc<CastingContext>,
    dataver: Dataver,
}

impl MediaPlaybackHandler {
    /// Build the handler.
    pub fn new(ctx: Arc<CastingContext>, dataver: Dataver) -> Self {
        Self { ctx, dataver }
    }

    /// Send a transport verb and answer with the cluster's status.
    ///
    /// `NotActive` when nothing is loaded, which is the cluster's own way of saying "there
    /// is nothing to pause" — the alternative is `Success` for a command that did nothing.
    fn drive<P: TLVBuilderParent>(
        &self,
        transport: Transport,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        if matches!(self.ctx.state.get().state, PlaybackState::NotPlaying) {
            return response
                .status(media_playback::StatusEnum::NotActive)?
                .data(None)?
                .end();
        }

        self.ctx.send(CastCommand::Transport(transport))?;
        response
            .status(media_playback::StatusEnum::Success)?
            .data(None)?
            .end()
    }
}

impl media_playback::ClusterHandler for MediaPlaybackHandler {
    const CLUSTER: Cluster<'static> =
        media_playback::FULL_CLUSTER.with_features(MEDIA_PLAYBACK_FEATURES);

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn current_state(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<media_playback::PlaybackStateEnum, Error> {
        Ok(match self.ctx.state.get().state {
            PlaybackState::Playing => media_playback::PlaybackStateEnum::Playing,
            PlaybackState::Paused => media_playback::PlaybackStateEnum::Paused,
            PlaybackState::Buffering => media_playback::PlaybackStateEnum::Buffering,
            PlaybackState::NotPlaying => media_playback::PlaybackStateEnum::NotPlaying,
        })
    }

    fn duration(&self, _ctx: impl ReadContext) -> Result<rs_matter::tlv::Nullable<u64>, Error> {
        // Milliseconds on this cluster; `Null` for a live stream, which is a different
        // statement from a duration of zero.
        Ok(self
            .ctx
            .state
            .get()
            .duration
            .map_or_else(rs_matter::tlv::Nullable::none, |d| {
                rs_matter::tlv::Nullable::some(millis(d))
            }))
    }

    fn handle_play<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        self.drive(Transport::Play, response)
    }

    fn handle_pause<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        self.drive(Transport::Pause, response)
    }

    fn handle_stop<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        self.drive(Transport::Stop, response)
    }

    fn handle_start_over<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        self.drive(Transport::StartOver, response)
    }

    fn handle_previous<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        self.drive(Transport::Previous, response)
    }

    fn handle_next<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        self.drive(Transport::Next, response)
    }

    fn handle_rewind<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        _request: media_playback::RewindRequest<'_>,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        // Rewind means "play backwards at increasing speed", which needs the variable-speed
        // feature we do not advertise. Saying so beats seeking and calling it rewind.
        response
            .status(media_playback::StatusEnum::SpeedOutOfRange)?
            .data(None)?
            .end()
    }

    fn handle_fast_forward<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        _request: media_playback::FastForwardRequest<'_>,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        response
            .status(media_playback::StatusEnum::SpeedOutOfRange)?
            .data(None)?
            .end()
    }

    fn handle_skip_forward<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: media_playback::SkipForwardRequest<'_>,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        let by = std::time::Duration::from_millis(request.delta_position_milliseconds()?);
        self.drive(Transport::SkipForward(by), response)
    }

    fn handle_skip_backward<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: media_playback::SkipBackwardRequest<'_>,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        let by = std::time::Duration::from_millis(request.delta_position_milliseconds()?);
        self.drive(Transport::SkipBackward(by), response)
    }

    fn handle_activate_audio_track(
        &self,
        _ctx: impl InvokeContext,
        _request: media_playback::ActivateAudioTrackRequest<'_>,
    ) -> Result<(), Error> {
        // The audio/text track features are not advertised, so these commands are not on
        // this cluster instance at all. Reached only by a client that ignored the feature
        // map, and answered the way the interaction model answers that.
        Err(ErrorCode::CommandNotFound.into())
    }

    fn handle_activate_text_track(
        &self,
        _ctx: impl InvokeContext,
        _request: media_playback::ActivateTextTrackRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    fn handle_deactivate_text_track(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    fn handle_seek<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: media_playback::SeekRequest<'_>,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        let to = std::time::Duration::from_millis(request.position()?);

        // A seek past the end is the one transport command with a wrong answer rather than
        // a refused one, and the cluster has a status for it.
        if let Some(duration) = self.ctx.state.get().duration {
            if to > duration {
                return response
                    .status(media_playback::StatusEnum::SeekOutOfRange)?
                    .data(None)?
                    .end();
            }
        }

        self.drive(Transport::Seek(to), response)
    }
}

/// The `ApplicationBasic` server: who each content-app endpoint claims to be.
#[derive(Debug)]
pub struct ApplicationBasicHandler {
    ctx: Arc<CastingContext>,
    dataver: Dataver,
}

impl ApplicationBasicHandler {
    /// Build the handler.
    pub fn new(ctx: Arc<CastingContext>, dataver: Dataver) -> Self {
        Self { ctx, dataver }
    }

    fn app(&self, ctx: &impl ReadContext) -> Result<&crate::player::ContentApp, Error> {
        let endpoint = ctx.endpt().ok_or(ErrorCode::EndpointNotFound)?;
        self.ctx
            .catalogue
            .at(endpoint)
            .ok_or_else(|| ErrorCode::EndpointNotFound.into())
    }
}

impl application_basic::ClusterHandler for ApplicationBasicHandler {
    const CLUSTER: Cluster<'static> = application_basic::FULL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn vendor_name<P: TLVBuilderParent>(
        &self,
        ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(&self.app(&ctx)?.vendor_name)
    }

    fn vendor_id(&self, ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.app(&ctx)?.vendor_id)
    }

    fn product_id(&self, ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.app(&ctx)?.product_id)
    }

    fn application_name<P: TLVBuilderParent>(
        &self,
        ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(&self.app(&ctx)?.name)
    }

    fn application_version<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(env!("CARGO_PKG_VERSION"))
    }

    fn application<P: TLVBuilderParent>(
        &self,
        ctx: impl ReadContext,
        builder: application_basic::ApplicationStructBuilder<P>,
    ) -> Result<P, Error> {
        let app = self.app(&ctx)?;
        builder
            .catalog_vendor_id(app.catalog_vendor_id)?
            .application_id(&app.application_id)?
            .end()
    }

    fn status(
        &self,
        ctx: impl ReadContext,
    ) -> Result<application_basic::ApplicationStatusEnum, Error> {
        // `ActiveVisibleFocus` when this app's media is the one on the glass, and
        // `Stopped` otherwise. Never `ActiveHidden`: the panel shows one thing at a time,
        // so an app that is not on screen is not running.
        let snapshot = self.ctx.state.get();
        let endpoint = ctx.endpt().ok_or(ErrorCode::EndpointNotFound)?;
        Ok(if snapshot.app == Some(endpoint) {
            application_basic::ApplicationStatusEnum::ActiveVisibleFocus
        } else {
            application_basic::ApplicationStatusEnum::Stopped
        })
    }

    fn allowed_vendor_list<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<
            rs_matter::tlv::ToTLVArrayBuilder<P, u16>,
            rs_matter::tlv::ToTLVBuilder<P, u16>,
        >,
    ) -> Result<P, Error> {
        // Empty, and deliberately so. On a certified receiver this list is what lets a
        // content app refuse a casting client whose vendor is not on it — an access
        // control decision made on the strength of the client's attestation certificate.
        // This panel does not verify attestation (#N), so a list here would be a claim we
        // cannot back. Empty means "no vendor is specially privileged", which is true.
        match builder {
            ArrayAttributeRead::ReadAll(builder) => builder.end(),
            ArrayAttributeRead::ReadOne(_, _) => Err(ErrorCode::ConstraintError.into()),
            ArrayAttributeRead::ReadNone(builder) => builder.end(),
        }
    }
}

/// The `TargetNavigator` server: the content apps, as a list a client can pick from.
///
/// The same apps the descriptor already describes, in the form a client that has not read
/// the descriptor looks for. Targets are numbered from 1 — target 0 is reserved.
#[derive(Debug)]
pub struct TargetNavigatorHandler {
    ctx: Arc<CastingContext>,
    dataver: Dataver,
}

impl TargetNavigatorHandler {
    /// Build the handler.
    pub fn new(ctx: Arc<CastingContext>, dataver: Dataver) -> Self {
        Self { ctx, dataver }
    }

    /// Target identifiers are one-based and dense; endpoints are neither.
    fn endpoint_for_target(&self, target: u8) -> Option<EndpointId> {
        let index = usize::from(target.checked_sub(1)?);
        self.ctx.catalogue.apps().get(index).map(|a| a.endpoint)
    }
}

impl target_navigator::ClusterHandler for TargetNavigatorHandler {
    const CLUSTER: Cluster<'static> = target_navigator::FULL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn target_list<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<
            target_navigator::TargetInfoStructArrayBuilder<P>,
            target_navigator::TargetInfoStructBuilder<P>,
        >,
    ) -> Result<P, Error> {
        let apps = self.ctx.catalogue.apps();

        match builder {
            ArrayAttributeRead::ReadAll(mut builder) => {
                for (index, app) in apps.iter().enumerate() {
                    // Bounded by MAX_CONTENT_APPS, so the cast cannot truncate.
                    #[allow(clippy::cast_possible_truncation)]
                    let identifier = (index + 1) as u8;
                    builder = builder
                        .push()?
                        .identifier(identifier)?
                        .name(&app.name)?
                        .end()?;
                }
                builder.end()
            }
            ArrayAttributeRead::ReadOne(index, builder) => {
                let Some(app) = apps.get(index as usize) else {
                    return Err(ErrorCode::ConstraintError.into());
                };
                #[allow(clippy::cast_possible_truncation)]
                let identifier = (index + 1) as u8;
                builder.identifier(identifier)?.name(&app.name)?.end()
            }
            ArrayAttributeRead::ReadNone(builder) => builder.end(),
        }
    }

    fn current_target(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        let current = self.ctx.state.get().app;
        Ok(current
            .and_then(|endpoint| {
                self.ctx
                    .catalogue
                    .apps()
                    .iter()
                    .position(|a| a.endpoint == endpoint)
            })
            .and_then(|index| u8::try_from(index + 1).ok())
            // 0 is the reserved "no target", which is the honest answer when the panel is
            // playing something no content app launched.
            .unwrap_or(0))
    }

    fn handle_navigate_target<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: target_navigator::NavigateTargetRequest<'_>,
        response: target_navigator::NavigateTargetResponseBuilder<P>,
    ) -> Result<P, Error> {
        let target = request.target()?;

        let Some(endpoint) = self.endpoint_for_target(target) else {
            return response
                .status(target_navigator::StatusEnum::TargetNotFound)?
                .data(None)?
                .end();
        };

        self.ctx.send(CastCommand::SelectTarget(endpoint))?;
        response
            .status(target_navigator::StatusEnum::Success)?
            .data(None)?
            .end()
    }
}

/// Milliseconds, saturating. The cluster's unit; a duration past `u64::MAX` ms is not a
/// number any media has.
fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The node metadata: endpoints, device types, clusters.
///
/// Owns leaked slices. `Endpoint<'a>` borrows its device-type and cluster lists, and the
/// tree is built from config rather than being a `const`, so something has to own those
/// for the process's lifetime. This is built once per run and the panel does not reload
/// config without restarting, so a leak here is a fixed cost and not a growing one.
pub struct NodeTree {
    endpoints: &'static [Endpoint<'static>],
}

impl NodeTree {
    /// Build the tree for a catalogue.
    #[must_use]
    pub fn new(catalogue: &Catalogue) -> Self {
        let mut endpoints: Vec<Endpoint<'static>> = Vec::with_capacity(catalogue.apps().len() + 2);

        // A `const` binding, not a temporary: the macro expands to an `Endpoint` whose
        // slices borrow from the expression, and pushing it inline drops them at the
        // semicolon.
        const ROOT: Endpoint<'static> = rs_matter::root_endpoint!(eth);
        endpoints.push(ROOT);

        endpoints.push(Endpoint {
            id: PLAYER_ENDPOINT,
            device_types: devices!(DEV_TYPE_CASTING_VIDEO_PLAYER),
            clusters: Box::leak(Box::new([
                <DescHandler as desc::ClusterHandler>::CLUSTER,
                <ContentLauncherHandler as content_launcher::ClusterHandler>::CLUSTER,
                <MediaPlaybackHandler as media_playback::ClusterHandler>::CLUSTER,
                <TargetNavigatorHandler as target_navigator::ClusterHandler>::CLUSTER,
            ])),
            client_clusters: &[],
        });

        for app in catalogue.apps() {
            endpoints.push(Endpoint {
                id: app.endpoint,
                device_types: devices!(DEV_TYPE_CONTENT_APP),
                clusters: Box::leak(Box::new([
                    <DescHandler as desc::ClusterHandler>::CLUSTER,
                    <ApplicationBasicHandler as application_basic::ClusterHandler>::CLUSTER,
                    <ContentLauncherHandler as content_launcher::ClusterHandler>::CLUSTER,
                ])),
                client_clusters: &[],
            });
        }

        Self {
            endpoints: Box::leak(endpoints.into_boxed_slice()),
        }
    }

    /// The node a client reads.
    #[must_use]
    pub fn node(&self) -> Node<'static> {
        Node::new(self.endpoints)
    }
}

/// Build the whole handler chain: `rs-matter`'s root-endpoint system clusters, then ours.
///
/// Chained after the system handler, so the root endpoint's own Descriptor still answers
/// for endpoint 0 — [`AppCluster`] is what keeps ours off it.
pub fn handlers<'a>(
    ctx: &'a Arc<CastingContext>,
    mut rand: impl RngCore + Copy,
) -> impl rs_matter::dm::AsyncHandler + 'a {
    let desc = Async(DescHandler::new(Dataver::new_rand(&mut rand)).adapt());
    let content_launcher = Async(content_launcher::HandlerAdaptor(
        ContentLauncherHandler::new(Arc::clone(ctx), Dataver::new_rand(&mut rand)),
    ));
    let media_playback = Async(media_playback::HandlerAdaptor(MediaPlaybackHandler::new(
        Arc::clone(ctx),
        Dataver::new_rand(&mut rand),
    )));
    let application_basic = Async(application_basic::HandlerAdaptor(
        ApplicationBasicHandler::new(Arc::clone(ctx), Dataver::new_rand(&mut rand)),
    ));
    let target_navigator = Async(target_navigator::HandlerAdaptor(
        TargetNavigatorHandler::new(Arc::clone(ctx), Dataver::new_rand(&mut rand)),
    ));

    EthSysHandlerBuilder::new()
        .build(rand)
        .chain(
            AppCluster(<DescHandler as desc::ClusterHandler>::CLUSTER.id),
            desc,
        )
        .chain(
            AppCluster(<ContentLauncherHandler as content_launcher::ClusterHandler>::CLUSTER.id),
            content_launcher,
        )
        .chain(
            AppCluster(<MediaPlaybackHandler as media_playback::ClusterHandler>::CLUSTER.id),
            media_playback,
        )
        .chain(
            AppCluster(<ApplicationBasicHandler as application_basic::ClusterHandler>::CLUSTER.id),
            application_basic,
        )
        .chain(
            AppCluster(<TargetNavigatorHandler as target_navigator::ClusterHandler>::CLUSTER.id),
            target_navigator,
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::player::{ContentApp, LaunchTarget, FIRST_CONTENT_APP_ENDPOINT};

    fn app(name: &str) -> ContentApp {
        ContentApp {
            // Overwritten by `Catalogue::new`, which is the point of the assertion below.
            endpoint: 0,
            vendor_id: 0xFFF1,
            product_id: 0x8001,
            vendor_name: "castaway".into(),
            name: name.into(),
            application_id: format!("com.example.{name}"),
            catalog_vendor_id: 0,
            launch: LaunchTarget::Browser { search: None },
        }
    }

    fn cluster_ids(endpoint: &Endpoint<'static>) -> Vec<u32> {
        endpoint.clusters.iter().map(|c| c.id).collect()
    }

    /// The endpoint tree a client reads, which nothing had ever constructed in a test.
    ///
    /// `NodeTree` is the whole of what a commissioned phone discovers about this panel:
    /// the root, the Casting Video Player, and one Content App per thing the panel can
    /// open. A client picks an endpoint by walking the Descriptor, so a tree with the
    /// wrong device type or a missing cluster is a client that finds nothing to cast to
    /// and says nothing about why (#196).
    #[test]
    fn the_tree_is_a_root_a_player_and_one_endpoint_per_content_app() {
        let catalogue = Catalogue::new([app("alpha"), app("beta")]);
        let tree = NodeTree::new(&catalogue);
        let node = tree.node();
        let endpoints: Vec<&Endpoint<'static>> = node.endpoints.iter().collect();

        assert_eq!(endpoints.len(), 4, "root, player, and one per app");

        // Endpoint 0 is `rs-matter`'s root, and ours must stay off it: the system handler
        // answers for it, and a Descriptor there listing our clusters is a client told the
        // root can launch content.
        assert_eq!(endpoints[0].id, 0);
        let root_clusters = cluster_ids(endpoints[0]);
        for ours in [
            <ContentLauncherHandler as content_launcher::ClusterHandler>::CLUSTER.id,
            <MediaPlaybackHandler as media_playback::ClusterHandler>::CLUSTER.id,
            <TargetNavigatorHandler as target_navigator::ClusterHandler>::CLUSTER.id,
            <ApplicationBasicHandler as application_basic::ClusterHandler>::CLUSTER.id,
        ] {
            assert!(
                !root_clusters.contains(&ours),
                "cluster {ours:#x} is on the root endpoint"
            );
        }

        // The player: the endpoint a client casts to when it has no app in mind. All
        // three of its clusters are load-bearing — no ContentLauncher and there is
        // nothing to launch with, no MediaPlayback and the transport buttons do nothing,
        // no TargetNavigator and the content apps are invisible.
        assert_eq!(endpoints[1].id, PLAYER_ENDPOINT);
        let player = cluster_ids(endpoints[1]);
        for required in [
            <DescHandler as desc::ClusterHandler>::CLUSTER.id,
            <ContentLauncherHandler as content_launcher::ClusterHandler>::CLUSTER.id,
            <MediaPlaybackHandler as media_playback::ClusterHandler>::CLUSTER.id,
            <TargetNavigatorHandler as target_navigator::ClusterHandler>::CLUSTER.id,
        ] {
            assert!(
                player.contains(&required),
                "player is missing {required:#x}"
            );
        }
        assert!(
            endpoints[1]
                .device_types
                .iter()
                .any(|d| d.dtype == DEV_TYPE_CASTING_VIDEO_PLAYER.dtype),
            "the player must say it is a Casting Video Player, which is what a client \
             matches on before it will cast at all"
        );

        // One endpoint per app, numbered from `FIRST_CONTENT_APP_ENDPOINT` upward and in
        // catalogue order — the identity `TargetNavigator`'s one-based identifiers assume.
        for (i, app) in catalogue.apps().iter().enumerate() {
            let ep = endpoints[2 + i];
            assert_eq!(ep.id, app.endpoint);
            #[allow(clippy::cast_possible_truncation)]
            let expected = FIRST_CONTENT_APP_ENDPOINT + i as u16;
            assert_eq!(ep.id, expected, "content apps are dense from the first");
            assert!(
                ep.device_types
                    .iter()
                    .any(|d| d.dtype == DEV_TYPE_CONTENT_APP.dtype),
                "a content app that does not say so is one a TargetApp match skips"
            );
            let clusters = cluster_ids(ep);
            assert!(
                clusters.contains(
                    &<ApplicationBasicHandler as application_basic::ClusterHandler>::CLUSTER.id
                ),
                "ApplicationBasic is how a client learns who this endpoint claims to be"
            );
            assert!(clusters.contains(
                &<ContentLauncherHandler as content_launcher::ClusterHandler>::CLUSTER.id
            ));
            // …and *not* MediaPlayback: the transport is the player's, and a second one
            // per app would give a client two places to send Play and no rule for which.
            assert!(
                !clusters.contains(
                    &<MediaPlaybackHandler as media_playback::ClusterHandler>::CLUSTER.id
                ),
                "the transport belongs to the player endpoint, not to each app"
            );
        }
    }

    /// An empty catalogue is still a node a client can talk to.
    ///
    /// The honest default for this panel is no content apps at all — Matter carries no
    /// media, and an app that accepted a cast it cannot play would be lying — so this is
    /// the shipped shape rather than a degenerate one.
    #[test]
    fn a_panel_with_no_content_apps_still_offers_a_player() {
        let tree = NodeTree::new(&Catalogue::new([]));
        let node = tree.node();
        let endpoints: Vec<&Endpoint<'static>> = node.endpoints.iter().collect();
        assert_eq!(endpoints.len(), 2, "root and player");
        assert_eq!(endpoints[1].id, PLAYER_ENDPOINT);
    }

    /// The one-based target identifier a client sends maps onto an endpoint, and the
    /// reserved zero maps onto nothing.
    ///
    /// `TargetNavigator`'s identifiers are one-based and dense; endpoints are neither.
    /// The mapping is four lines and was untested, and both ends of it are a way for a
    /// `NavigateTarget` to select the wrong app silently — an off-by-one here launches
    /// the neighbour of the thing the user picked (#196).
    #[test]
    fn target_identifiers_are_one_based_and_zero_is_reserved() {
        let catalogue = Catalogue::new([app("alpha"), app("beta"), app("gamma")]);
        let (tx, _rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(CastingContext {
            catalogue: catalogue.clone(),
            state: Arc::new(PlayerState::new()),
            commands: tx,
        });
        let nav = TargetNavigatorHandler::new(Arc::clone(&ctx), Dataver::new(1));

        // 1 is the first app, not the second and not the player.
        assert_eq!(
            nav.endpoint_for_target(1),
            Some(FIRST_CONTENT_APP_ENDPOINT),
            "target 1 must be the first content app"
        );
        assert_eq!(
            nav.endpoint_for_target(3),
            Some(FIRST_CONTENT_APP_ENDPOINT + 2)
        );

        // 0 is the spec's "no target" and must not index anything — a `checked_sub` that
        // became a `- 1` would wrap to 255 and miss, but a `saturating_sub` would select
        // the *first* app, which is the dangerous one.
        assert_eq!(
            nav.endpoint_for_target(0),
            None,
            "0 is reserved; it must not select the first app"
        );

        // Past the end is `TargetNotFound`, not the last app.
        assert_eq!(nav.endpoint_for_target(4), None);
        assert_eq!(nav.endpoint_for_target(u8::MAX), None);
    }

    #[test]
    fn a_duration_longer_than_any_media_saturates_rather_than_wrapping() {
        assert_eq!(millis(std::time::Duration::from_millis(1500)), 1500);
        assert_eq!(millis(std::time::Duration::MAX), u64::MAX);
    }
}
