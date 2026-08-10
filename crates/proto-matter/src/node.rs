//! The endpoint tree a Casting Client sees, and the cluster handlers behind it.
//!
//! Three shapes of endpoint:
//!
//! - **0** — the root node. `rs-matter`'s own system clusters: Basic Information,
//!   Operational Credentials, Access Control, General Commissioning. Untouched by us.
//! - **1** — the Casting Video Player. `ContentLauncher` (a URL the panel plays itself),
//!   `MediaPlayback` (the transport), `TargetNavigator` (the list of content apps,
//!   which is how a client that has not read the descriptor finds them), `KeypadInput`
//!   (a remote's transport keys), and `ApplicationLauncher` (the same apps addressed by
//!   catalog entry, #274).
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
    application_basic, application_launcher, content_launcher, keypad_input, media_playback,
    target_navigator,
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

use castaway_core::PlaybackReport;

use crate::player::{
    CastCommand, Catalogue, EndpointId, LaunchRefusal, PlaybackState, PlayerSnapshot, PlayerState,
    Transport, PLAYER_ENDPOINT,
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

/// `ApplicationLauncher` feature bits: ApplicationPlatform (#274). The panel *is* the
/// platform — it hosts a catalogue of content apps on endpoints of its own — which is
/// the shape where this cluster sits on the video player endpoint and `CatalogList`
/// names the catalogs those apps come from.
const APPLICATION_LAUNCHER_FEATURES: u32 = 0b1;

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
pub struct CastingContext {
    /// The apps this panel hosts.
    pub catalogue: Catalogue,
    /// What the panel is playing, as `MediaPlayback` reports it.
    pub state: Arc<PlayerState>,
    /// Where a decoded invoke goes.
    pub commands: mpsc::UnboundedSender<CastCommand>,
    /// Where the pipeline's position and duration come from (#283). [`None`] on a build
    /// with nothing that reports — the null pipeline — where the projection then keeps
    /// whatever it was last told: position frozen where the last accepted seek put it,
    /// and no duration, which reads to a client as a stream with no known end.
    pub playback: Option<Arc<dyn PlaybackReport>>,
}

impl std::fmt::Debug for CastingContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CastingContext")
            .field("catalogue", &self.catalogue)
            .field("state", &self.state)
            .field("playback", &self.playback.is_some())
            .finish_non_exhaustive()
    }
}

impl CastingContext {
    /// The projection, refreshed from the pipeline (#283).
    ///
    /// Sampled here — at the handler boundary, once per read or invoke — and folded into
    /// the shared projection, so everything downstream of the transaction the client is
    /// waiting on (the bound check on a `Seek`, the resolution of a skip, the next
    /// attribute read) agrees on one position and one duration.
    fn refreshed(&self) -> PlayerSnapshot {
        let progress = self.playback.as_ref().and_then(|report| report.progress());
        self.state.refresh(progress)
    }

    /// Send a transport verb at the adapter — or report that there is nothing to drive.
    ///
    /// Shared by every cluster that can move the transport (`MediaPlayback`'s verbs and
    /// `KeypadInput`'s transport keys), so the guard and the projection move are decided
    /// once; each caller maps the outcome onto its own cluster's status vocabulary.
    fn drive_transport(&self, transport: Transport) -> Result<TransportOutcome, Error> {
        if matches!(self.state.get().state, PlaybackState::NotPlaying) {
            return Ok(TransportOutcome::NothingPlaying);
        }

        self.send(CastCommand::Transport(transport))?;
        // And move the projection this cluster reads *here*, in the transaction the client
        // is waiting on, rather than where the command is eventually consumed. Two reasons,
        // and the second is the one that made this a defect rather than a race:
        //
        // - `CastCommand` goes out on a channel the adapter drains on another task, so a
        //   phone that presses pause and reads `CurrentState` back can otherwise see the
        //   two out of order.
        // - Nothing moved this projection at all except a launch and the end of a session.
        //   So a paused phone was told it was still playing, and `NotActive` was
        //   unreachable for the whole life of a session: nothing could put the state back
        //   to `NotPlaying` (#196).
        //
        // The pipeline is the real authority; a command the panel has just accepted is the
        // best evidence this projection has. The position moves on the absolute verbs for
        // the same read-back reason (#283) — but the *state* deliberately stays: seeking
        // while paused does not start playback.
        match transport {
            Transport::Play => self.state.update(|s| s.state = PlaybackState::Playing),
            Transport::Pause => self.state.update(|s| s.state = PlaybackState::Paused),
            Transport::Stop => self.state.update(|s| {
                // Not a whole `PlayerSnapshot::default()`: stopping releases the media and
                // not the client's idea of which app it was in, and a `CurrentTarget` that
                // reset itself on stop reads to a phone as the app having closed.
                s.state = PlaybackState::NotPlaying;
                s.position = std::time::Duration::ZERO;
                s.duration = None;
            }),
            Transport::StartOver => self
                .state
                .update(|s| s.position = std::time::Duration::ZERO),
            Transport::Seek(to) => self.state.update(|s| s.position = to),
            Transport::Previous | Transport::Next => {}
        }
        Ok(TransportOutcome::Driven)
    }

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

/// What became of a transport verb, in terms the clusters translate into their own words.
///
/// `MediaPlayback` renders [`Self::NothingPlaying`] as `NotActive`; `KeypadInput` renders
/// it as `InvalidKeyInCurrentState`. Same fact, two vocabularies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportOutcome {
    /// The command went to the adapter and the projection moved with it.
    Driven,
    /// Nothing is loaded, so there was nothing to drive.
    NothingPlaying,
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
        let status = match self.ctx.drive_transport(transport)? {
            TransportOutcome::Driven => media_playback::StatusEnum::Success,
            TransportOutcome::NothingPlaying => media_playback::StatusEnum::NotActive,
        };
        response.status(status)?.data(None)?.end()
    }
}

impl media_playback::ClusterHandler for MediaPlaybackHandler {
    // Beyond the feature map, the attribute and command lists are trimmed to what the
    // panel actually serves: the mandatory `CurrentState`, plus `Duration` and the seek
    // range — the shape of the media's end, which is what #283 put in the projection.
    // `SampledPosition` and `PlaybackSpeed` are *not* advertised: nothing here implements
    // them yet, and a list is a promise a client plans reads against. Likewise the track
    // commands stay off the accepted-command list — the track features are not advertised
    // and a command the metadata does not carry is refused by the interaction model
    // itself, which is a truer answer than a handler's `CommandNotFound`.
    const CLUSTER: Cluster<'static> = media_playback::FULL_CLUSTER
        .with_features(MEDIA_PLAYBACK_FEATURES)
        .with_attrs(rs_matter::with!(
            required;
            media_playback::AttributeId::Duration
                | media_playback::AttributeId::SeekRangeStart
                | media_playback::AttributeId::SeekRangeEnd
        ))
        .with_cmds(rs_matter::with!(
            media_playback::CommandId::Play
                | media_playback::CommandId::Pause
                | media_playback::CommandId::Stop
                | media_playback::CommandId::StartOver
                | media_playback::CommandId::Previous
                | media_playback::CommandId::Next
                | media_playback::CommandId::Rewind
                | media_playback::CommandId::FastForward
                | media_playback::CommandId::SkipForward
                | media_playback::CommandId::SkipBackward
                | media_playback::CommandId::Seek
        ));

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
        // statement from a duration of zero. Refreshed from the pipeline, which is the
        // only party that has read the container (#283).
        Ok(self
            .ctx
            .refreshed()
            .duration
            .map_or_else(rs_matter::tlv::Nullable::none, |d| {
                rs_matter::tlv::Nullable::some(millis(d))
            }))
    }

    fn seek_range_start(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<rs_matter::tlv::Nullable<u64>, Error> {
        // The panel plays plain items, not shifting live windows: seekable media is
        // seekable from the top, and unseekable media has no range at all — `Null` on
        // both ends, matching the `SeekOutOfRange` every seek into it gets.
        Ok(match self.ctx.refreshed().duration {
            Some(_) => rs_matter::tlv::Nullable::some(0),
            None => rs_matter::tlv::Nullable::none(),
        })
    }

    fn seek_range_end(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<rs_matter::tlv::Nullable<u64>, Error> {
        // For a plain item the furthest seekable point is the end, so this is `Duration`
        // again — stated separately because the cluster asks separately.
        Ok(self
            .ctx
            .refreshed()
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
        // Resolved here, against the projection just refreshed from the pipeline, so the
        // seek the adapter forwards and the position a read-back reports are the same
        // number (#283). A skip past a known end is a seek *to* the end, per the spec —
        // the one relative verb with no out-of-range refusal.
        let target = self.ctx.refreshed().skip_forward_target(by);
        self.drive(Transport::Seek(target), response)
    }

    fn handle_skip_backward<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: media_playback::SkipBackwardRequest<'_>,
        response: media_playback::PlaybackResponseBuilder<P>,
    ) -> Result<P, Error> {
        let by = std::time::Duration::from_millis(request.delta_position_milliseconds()?);
        let target = self.ctx.refreshed().skip_backward_target(by);
        self.drive(Transport::Seek(target), response)
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
        let snapshot = self.ctx.refreshed();

        // Nothing loaded outranks out-of-range: with no media there is no range to be
        // outside of, and `NotActive` is the status that says so.
        if matches!(snapshot.state, PlaybackState::NotPlaying) {
            return self.drive(Transport::Seek(to), response);
        }

        // A seek past the end is the one transport command with a wrong answer rather
        // than a refused one, and the cluster has a status for it. Media with no known
        // end gets the same status for *every* target (#283): a live stream has no range
        // to be inside, and honouring the seek instead would hand the pipeline a target
        // it cannot bound. The refusal is lossy on the wire — `SeekOutOfRange` covers
        // both — so the reason goes to the panel's own log.
        match snapshot.seek_target(to) {
            Ok(target) => self.drive(Transport::Seek(target), response),
            Err(refusal) => {
                tracing::info!(to_ms = millis(to), ?refusal, "matter: declining a Seek");
                response
                    .status(media_playback::StatusEnum::SeekOutOfRange)?
                    .data(None)?
                    .end()
            }
        }
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
        // Synchronously, for the reason `MediaPlaybackHandler::drive` gives: the command
        // crosses a channel another task drains, and `CurrentTarget` is the *only* place
        // a selection is observable — so a client that navigates and then reads back
        // could otherwise be told it is still on the target it just left.
        self.ctx.state.update(|s| s.app = Some(endpoint));
        response
            .status(target_navigator::StatusEnum::Success)?
            .data(None)?
            .end()
    }
}

/// The `KeypadInput` server: a remote control's buttons, on the player endpoint (#274).
///
/// The feature map is empty on purpose: the navigation, location and number key features
/// are for devices with an on-screen menu to move a cursor around, and the panel has
/// none. What a casting remote's keys can honestly do here is drive the transport, so the
/// transport keys are the supported set and everything else answers `UnsupportedKey` —
/// the status that tells a sender to hide or disable the button, rather than a `Success`
/// for a key that did nothing.
#[derive(Debug)]
pub struct KeypadInputHandler {
    ctx: Arc<CastingContext>,
    dataver: Dataver,
}

impl KeypadInputHandler {
    /// Build the handler.
    pub fn new(ctx: Arc<CastingContext>, dataver: Dataver) -> Self {
        Self { ctx, dataver }
    }

    /// The CEC keys the panel can honour, mapped onto the transport verbs it already has.
    ///
    /// [`None`] is "not a key this panel has", answered `UnsupportedKey`. The play/pause
    /// *functions* (96/97) are the deterministic Feature-C variants of the plain keys and
    /// map the same way; `PausePlayFunction` is the toggle every one-button remote sends,
    /// resolved against the projection because a toggle is meaningless without knowing
    /// which way it toggles.
    fn key_to_transport(
        key: keypad_input::CECKeyCodeEnum,
        state: PlaybackState,
    ) -> Option<Transport> {
        use keypad_input::CECKeyCodeEnum as Key;
        match key {
            Key::Play | Key::PlayFunction => Some(Transport::Play),
            Key::Pause => Some(Transport::Pause),
            Key::PausePlayFunction => Some(match state {
                PlaybackState::Playing | PlaybackState::Buffering => Transport::Pause,
                PlaybackState::Paused | PlaybackState::NotPlaying => Transport::Play,
            }),
            Key::Stop | Key::StopFunction => Some(Transport::Stop),
            // CEC's names for the track keys: 75/76 are the |<< / >>| pair, not seeks.
            Key::Forward => Some(Transport::Next),
            Key::Backward => Some(Transport::Previous),
            // Rewind and FastForward deliberately not mapped: the panel refuses the same
            // verbs on MediaPlayback with SpeedOutOfRange, and a key that pretended by
            // seeking would make the keypad disagree with the transport cluster.
            _ => None,
        }
    }
}

impl keypad_input::ClusterHandler for KeypadInputHandler {
    const CLUSTER: Cluster<'static> = keypad_input::FULL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn handle_send_key<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: keypad_input::SendKeyRequest<'_>,
        response: keypad_input::SendKeyResponseBuilder<P>,
    ) -> Result<P, Error> {
        let key = request.key_code()?;
        let snapshot = self.ctx.state.get();

        let status = match Self::key_to_transport(key, snapshot.state) {
            Some(transport) => match self.ctx.drive_transport(transport)? {
                TransportOutcome::Driven => keypad_input::StatusEnum::Success,
                // The key exists here; there is just nothing loaded for it to act on.
                // `InvalidKeyInCurrentState`, not `UnsupportedKey`: the latter would tell
                // the sender to remove the button for good.
                TransportOutcome::NothingPlaying => {
                    keypad_input::StatusEnum::InvalidKeyInCurrentState
                }
            },
            None => {
                tracing::info!(?key, "matter: declining a key the panel does not have");
                keypad_input::StatusEnum::UnsupportedKey
            }
        };
        response.status(status)?.end()
    }
}

/// The `ApplicationLauncher` server: the content-app catalogue as a launchable platform
/// (#274), on the player endpoint with the ApplicationPlatform feature.
///
/// The same apps `TargetNavigator` indexes, addressed the third way a client can aim: by
/// catalog entry rather than by list position or by vendor/product. Launching an app on
/// this panel *selects* it — Matter carries no media, the panel's apps have no home
/// screen to open, and what a launch really establishes is which app a subsequent
/// `LaunchURL`/`LaunchContent` belongs to, exactly as `NavigateTarget` does.
#[derive(Debug)]
pub struct ApplicationLauncherHandler {
    ctx: Arc<CastingContext>,
    dataver: Dataver,
}

impl ApplicationLauncherHandler {
    /// Build the handler.
    pub fn new(ctx: Arc<CastingContext>, dataver: Dataver) -> Self {
        Self { ctx, dataver }
    }

    /// Resolve a request's application struct against the catalogue.
    ///
    /// # Errors
    /// `ConstraintError` when the field is absent: with the ApplicationPlatform feature
    /// the spec makes it mandatory, and there is no "current app" fallback to guess at.
    fn requested_app(
        &self,
        application: Option<application_launcher::ApplicationStruct<'_>>,
    ) -> Result<Option<&crate::player::ContentApp>, Error> {
        let application = application.ok_or(ErrorCode::ConstraintError)?;
        let catalog = application.catalog_vendor_id()?;
        let id = application.application_id()?;
        Ok(self.ctx.catalogue.by_application(catalog, id))
    }

    /// Take an app off the glass: end whatever it had playing and clear the current-app
    /// projection, synchronously with the invoke for the usual read-back reason (#196).
    fn retire(&self, endpoint: EndpointId) -> Result<(), Error> {
        let snapshot = self.ctx.state.get();
        if snapshot.app != Some(endpoint) {
            // Not the app on the glass. On this panel an app that is not on screen is
            // not running (`ApplicationBasic` says the same), so there is nothing to do
            // and Success is the honest answer — stopping a stopped app is idempotent.
            return Ok(());
        }
        if !matches!(snapshot.state, PlaybackState::NotPlaying) {
            // Its media is the session; ending the session is what stopping the app means.
            self.ctx.send(CastCommand::End)?;
        }
        self.ctx.state.set(crate::player::PlayerSnapshot::default());
        Ok(())
    }
}

impl application_launcher::ClusterHandler for ApplicationLauncherHandler {
    const CLUSTER: Cluster<'static> =
        application_launcher::FULL_CLUSTER.with_features(APPLICATION_LAUNCHER_FEATURES);

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn catalog_list<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<
            rs_matter::tlv::ToTLVArrayBuilder<P, u16>,
            rs_matter::tlv::ToTLVBuilder<P, u16>,
        >,
    ) -> Result<P, Error> {
        let catalogs = self.ctx.catalogue.catalog_vendor_ids();
        match builder {
            ArrayAttributeRead::ReadAll(mut builder) => {
                for id in &catalogs {
                    builder = builder.push(id)?;
                }
                builder.end()
            }
            ArrayAttributeRead::ReadOne(index, builder) => {
                let Some(id) = catalogs.get(index as usize) else {
                    return Err(ErrorCode::ConstraintError.into());
                };
                builder.set(id)
            }
            ArrayAttributeRead::ReadNone(builder) => builder.end(),
        }
    }

    fn current_app<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: rs_matter::tlv::NullableBuilder<
            P,
            application_launcher::ApplicationEPStructBuilder<P>,
        >,
    ) -> Result<P, Error> {
        // Null unless a *content app* is current: a launch on the bare player endpoint
        // belongs to no app, and the reserved answer is the honest one — the same shape
        // as `TargetNavigator`'s CurrentTarget of 0.
        let current = self
            .ctx
            .state
            .get()
            .app
            .and_then(|endpoint| self.ctx.catalogue.at(endpoint));
        match current {
            Some(app) => builder
                .non_null()?
                .application()?
                .catalog_vendor_id(app.catalog_vendor_id)?
                .application_id(&app.application_id)?
                .end()?
                .endpoint(Some(app.endpoint))?
                .end(),
            None => builder.null(),
        }
    }

    fn handle_launch_app<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: application_launcher::LaunchAppRequest<'_>,
        response: application_launcher::LauncherResponseBuilder<P>,
    ) -> Result<P, Error> {
        match self.requested_app(request.application()?)? {
            Some(app) => {
                let endpoint = app.endpoint;
                // The selection, exactly as `NavigateTarget` makes it: the command to the
                // adapter, and the projection moved synchronously with the invoke the
                // client is waiting on (#196). The request's opaque `data` is a message
                // for the app itself, and this panel's apps have nobody to hand it to.
                self.ctx.send(CastCommand::SelectTarget(endpoint))?;
                self.ctx.state.update(|s| s.app = Some(endpoint));
                response
                    .status(application_launcher::StatusEnum::Success)?
                    .data(None)?
                    .end()
            }
            None => {
                tracing::info!("matter: declining a LaunchApp for an app the panel does not host");
                response
                    .status(application_launcher::StatusEnum::AppNotAvailable)?
                    .data(None)?
                    .end()
            }
        }
    }

    fn handle_stop_app<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: application_launcher::StopAppRequest<'_>,
        response: application_launcher::LauncherResponseBuilder<P>,
    ) -> Result<P, Error> {
        match self.requested_app(request.application()?)? {
            Some(app) => {
                self.retire(app.endpoint)?;
                response
                    .status(application_launcher::StatusEnum::Success)?
                    .data(None)?
                    .end()
            }
            None => response
                .status(application_launcher::StatusEnum::AppNotAvailable)?
                .data(None)?
                .end(),
        }
    }

    fn handle_hide_app<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        request: application_launcher::HideAppRequest<'_>,
        response: application_launcher::LauncherResponseBuilder<P>,
    ) -> Result<P, Error> {
        // The spec's HideApp leaves the app running in the background. This panel has no
        // background — it shows one thing at a time, and an app that is not on the glass
        // is not running (`ApplicationBasic::status` says the same) — so hiding the
        // current app and stopping it are the same act here, and answering `Success`
        // while secretly keeping it "running" would be a state the panel cannot honour.
        match self.requested_app(request.application()?)? {
            Some(app) => {
                self.retire(app.endpoint)?;
                response
                    .status(application_launcher::StatusEnum::Success)?
                    .data(None)?
                    .end()
            }
            None => response
                .status(application_launcher::StatusEnum::AppNotAvailable)?
                .data(None)?
                .end(),
        }
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
                <KeypadInputHandler as keypad_input::ClusterHandler>::CLUSTER,
                <ApplicationLauncherHandler as application_launcher::ClusterHandler>::CLUSTER,
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
    let keypad_input = Async(keypad_input::HandlerAdaptor(KeypadInputHandler::new(
        Arc::clone(ctx),
        Dataver::new_rand(&mut rand),
    )));
    let application_launcher = Async(application_launcher::HandlerAdaptor(
        ApplicationLauncherHandler::new(Arc::clone(ctx), Dataver::new_rand(&mut rand)),
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
        .chain(
            AppCluster(<KeypadInputHandler as keypad_input::ClusterHandler>::CLUSTER.id),
            keypad_input,
        )
        .chain(
            AppCluster(
                <ApplicationLauncherHandler as application_launcher::ClusterHandler>::CLUSTER.id,
            ),
            application_launcher,
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
            <KeypadInputHandler as keypad_input::ClusterHandler>::CLUSTER.id,
            <ApplicationLauncherHandler as application_launcher::ClusterHandler>::CLUSTER.id,
        ] {
            assert!(
                !root_clusters.contains(&ours),
                "cluster {ours:#x} is on the root endpoint"
            );
        }

        // The player: the endpoint a client casts to when it has no app in mind. Every
        // one of its clusters is load-bearing — no ContentLauncher and there is
        // nothing to launch with, no MediaPlayback and the transport buttons do nothing,
        // no TargetNavigator and the content apps are invisible, no KeypadInput and a
        // remote's buttons land nowhere, no ApplicationLauncher and the catalogue cannot
        // be addressed by catalog entry (#274).
        assert_eq!(endpoints[1].id, PLAYER_ENDPOINT);
        let player = cluster_ids(endpoints[1]);
        for required in [
            <DescHandler as desc::ClusterHandler>::CLUSTER.id,
            <ContentLauncherHandler as content_launcher::ClusterHandler>::CLUSTER.id,
            <MediaPlaybackHandler as media_playback::ClusterHandler>::CLUSTER.id,
            <TargetNavigatorHandler as target_navigator::ClusterHandler>::CLUSTER.id,
            <KeypadInputHandler as keypad_input::ClusterHandler>::CLUSTER.id,
            <ApplicationLauncherHandler as application_launcher::ClusterHandler>::CLUSTER.id,
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
            // The platform clusters stay on the platform, for the same reason: one place
            // to press a key, one place to launch an app (#274).
            assert!(
                !clusters
                    .contains(&<KeypadInputHandler as keypad_input::ClusterHandler>::CLUSTER.id),
                "the keypad belongs to the player endpoint, not to each app"
            );
            assert!(
                !clusters.contains(
                    &<ApplicationLauncherHandler as application_launcher::ClusterHandler>::CLUSTER
                        .id
                ),
                "the launcher belongs to the player endpoint, not to each app"
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
            playback: None,
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

    /// A playback report modelling the render backend: `Some` with a position and
    /// (for a VOD item) a duration once the container is open, `None` before the first
    /// frame — which is the observed behaviour, not the trait's letter (ground rule 6).
    struct FakeReport(std::sync::Mutex<Option<castaway_core::PlaybackProgress>>);

    impl castaway_core::PlaybackReport for FakeReport {
        fn progress(&self) -> Option<castaway_core::PlaybackProgress> {
            self.0.lock().ok().and_then(|p| *p)
        }
    }

    fn context_with_report(
        progress: Option<castaway_core::PlaybackProgress>,
    ) -> (Arc<CastingContext>, mpsc::UnboundedReceiver<CastCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(CastingContext {
            catalogue: Catalogue::new([]),
            state: Arc::new(PlayerState::new()),
            commands: tx,
            playback: Some(Arc::new(FakeReport(std::sync::Mutex::new(progress)))),
        });
        (ctx, rx)
    }

    /// The pipeline's duration reaches the projection through `refreshed` (#283) —
    /// the join `handle_seek`'s bound check and the `Duration` attribute both read.
    #[test]
    fn refreshed_folds_the_pipeline_report_into_the_projection() {
        use std::time::Duration;

        let (ctx, _rx) = context_with_report(Some(
            castaway_core::PlaybackProgress::at(Duration::from_secs(30))
                .of(Duration::from_secs(300)),
        ));
        // A launch put the projection in flight; nothing has set a duration.
        ctx.state.update(|s| s.state = PlaybackState::Playing);

        let snapshot = ctx.refreshed();
        assert_eq!(snapshot.position, Duration::from_secs(30));
        assert_eq!(snapshot.duration, Some(Duration::from_secs(300)));
        // And the fold persisted, so the adapter and later reads agree.
        assert_eq!(ctx.state.get().duration, Some(Duration::from_secs(300)));
    }

    /// An accepted seek moves the projection in the same transaction (#283): the phone
    /// that seeks and reads back must not see the position it just left.
    #[test]
    fn an_accepted_seek_moves_the_projection_synchronously() {
        use std::time::Duration;

        let (ctx, mut rx) = context_with_report(None);
        ctx.state.update(|s| {
            s.state = PlaybackState::Playing;
            s.duration = Some(Duration::from_secs(300));
        });

        assert_eq!(
            ctx.drive_transport(Transport::Seek(Duration::from_secs(60)))
                .unwrap(),
            TransportOutcome::Driven
        );
        assert_eq!(ctx.state.get().position, Duration::from_secs(60));
        assert_eq!(
            rx.try_recv().unwrap(),
            CastCommand::Transport(Transport::Seek(Duration::from_secs(60)))
        );
    }

    /// With nothing loaded there is nothing to drive, and no command may leak out.
    #[test]
    fn transport_against_nothing_is_refused_and_sends_nothing() {
        let (ctx, mut rx) = context_with_report(None);
        assert_eq!(
            ctx.drive_transport(Transport::Play).unwrap(),
            TransportOutcome::NothingPlaying
        );
        assert!(rx.try_recv().is_err());
    }

    /// The CEC keys the panel honours map onto the transport it already has, and the
    /// rest map onto nothing — which the handler answers `UnsupportedKey` (#274).
    #[test]
    fn transport_keys_map_and_the_rest_are_unsupported() {
        use keypad_input::CECKeyCodeEnum as Key;

        let map = |key| KeypadInputHandler::key_to_transport(key, PlaybackState::Playing);
        assert_eq!(map(Key::Play), Some(Transport::Play));
        assert_eq!(map(Key::PlayFunction), Some(Transport::Play));
        assert_eq!(map(Key::Pause), Some(Transport::Pause));
        assert_eq!(map(Key::Stop), Some(Transport::Stop));
        assert_eq!(map(Key::StopFunction), Some(Transport::Stop));
        assert_eq!(map(Key::Forward), Some(Transport::Next));
        assert_eq!(map(Key::Backward), Some(Transport::Previous));

        // The variable-speed keys are refused for the same reason MediaPlayback refuses
        // the verbs: pretending with a seek would make the two clusters disagree.
        for unmapped in [
            Key::Rewind,
            Key::FastForward,
            Key::Select,
            Key::Up,
            Key::RootMenu,
            Key::Numbers5,
            Key::Power,
        ] {
            assert_eq!(map(unmapped), None, "{unmapped:?} must be unsupported");
        }
    }

    /// The one-button remote's toggle resolves against the projection: pause when moving,
    /// play otherwise. A toggle without state would send the key's name, not its meaning.
    #[test]
    fn pause_play_toggles_by_state() {
        use keypad_input::CECKeyCodeEnum as Key;

        let map = KeypadInputHandler::key_to_transport;
        assert_eq!(
            map(Key::PausePlayFunction, PlaybackState::Playing),
            Some(Transport::Pause)
        );
        assert_eq!(
            map(Key::PausePlayFunction, PlaybackState::Buffering),
            Some(Transport::Pause)
        );
        assert_eq!(
            map(Key::PausePlayFunction, PlaybackState::Paused),
            Some(Transport::Play)
        );
        assert_eq!(
            map(Key::PausePlayFunction, PlaybackState::NotPlaying),
            Some(Transport::Play)
        );
    }

    fn launcher_context() -> (
        Arc<CastingContext>,
        mpsc::UnboundedReceiver<CastCommand>,
        ApplicationLauncherHandler,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(CastingContext {
            catalogue: Catalogue::new([app("alpha"), app("beta")]),
            state: Arc::new(PlayerState::new()),
            commands: tx,
            playback: None,
        });
        let handler = ApplicationLauncherHandler::new(Arc::clone(&ctx), Dataver::new(1));
        (ctx, rx, handler)
    }

    /// Stopping the app on the glass ends its session and clears the projection — and
    /// stopping any other hosted app is an idempotent no-op, not an error (#274).
    #[test]
    fn retiring_an_app_ends_its_session_and_a_stopped_app_is_a_no_op() {
        let (ctx, mut rx, handler) = launcher_context();
        ctx.state.set(PlayerSnapshot {
            state: PlaybackState::Playing,
            app: Some(FIRST_CONTENT_APP_ENDPOINT),
            ..PlayerSnapshot::default()
        });

        // Not the current app: nothing happens, nothing is sent.
        handler.retire(FIRST_CONTENT_APP_ENDPOINT + 1).unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(ctx.state.get().app, Some(FIRST_CONTENT_APP_ENDPOINT));

        // The current app: the session ends and the projection clears, synchronously.
        handler.retire(FIRST_CONTENT_APP_ENDPOINT).unwrap();
        assert_eq!(rx.try_recv().unwrap(), CastCommand::End);
        assert_eq!(ctx.state.get(), PlayerSnapshot::default());

        // Stopping it again: already stopped, still Success-shaped — no command, no change.
        handler.retire(FIRST_CONTENT_APP_ENDPOINT).unwrap();
        assert!(rx.try_recv().is_err());
    }

    /// Retiring a current app that has nothing playing clears the selection without
    /// inventing a `SessionEvent::End` for a session that does not exist.
    #[test]
    fn retiring_an_idle_current_app_clears_the_selection_silently() {
        let (ctx, mut rx, handler) = launcher_context();
        ctx.state.set(PlayerSnapshot {
            state: PlaybackState::NotPlaying,
            app: Some(FIRST_CONTENT_APP_ENDPOINT),
            ..PlayerSnapshot::default()
        });

        handler.retire(FIRST_CONTENT_APP_ENDPOINT).unwrap();
        assert!(rx.try_recv().is_err(), "no session existed to end");
        assert_eq!(ctx.state.get().app, None);
    }
}
