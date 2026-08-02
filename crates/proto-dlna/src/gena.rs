//! GENA eventing: the subscriber table, the `LastChange` documents, and the wire shapes.
//!
//! Pure and socket-free (ground rule 3) — [`crate::notify`] does the sending and
//! [`crate::service`] does the HTTP. What is here is decisions: who is subscribed, what
//! they should be told, and what the bytes look like.
//!
//! ## Why this exists at all
//!
//! Answering `SUBSCRIBE` with 200 and never sending an event is worse than refusing, and
//! the mechanism is specific: `async_upnp_client` — which Home Assistant's `dlna_dmr` runs
//! on — guards its whole polling fallback on `is_subscribed`, and documents the
//! alternative itself (*"Device rejected subscription request. State variables will need to
//! be polled."*). Accepting therefore **disabled** their polling, and transport state,
//! volume and mute froze at connect values forever on a device that went on looking
//! healthy. So this service answered 501 instead, which put that one control point back on
//! a path that works — correct, and a placeholder for the real thing.
//!
//! This is the real thing. It matters beyond Home Assistant because Windows "Cast to
//! device" is documented as depending on it: WHCK EVENT-01 requires `LastChange` and
//! justifies it as *"The controller implemented in Windows 8 relies on AVT and RCS
//! LastChange events to make decisions about devices"*.
//!
//! ## The two eventing shapes, which are not the same
//!
//! AVTransport and RenderingControl **never** event their state variables directly
//! (AVT §2.3.1, RCS §2.3): every change is wrapped in one `LastChange` variable whose value
//! is an XML document — an XML document travelling as *text* inside another XML document,
//! so it is escaped exactly once, the same trap `CurrentURIMetaData` sets on the way in.
//! ConnectionManager is the opposite: its variables are evented directly, with no wrapper.
//!
//! Position is in neither. AVT §2.3.1 excludes `RelativeTimePosition` and
//! `AbsoluteTimePosition` from `LastChange`, which is why `GetPositionInfo` is polled once
//! a second and why it is the entire position channel.

use std::time::{Duration, Instant};

use crate::state::Renderer;

/// The longest subscription this service will grant, and what it offers by default.
///
/// UDA 1.1 §4.1.2 makes the duration the *publisher's* choice — a subscriber's `TIMEOUT`
/// is a request, not an instruction — and requires it to be at least 1800 seconds when a
/// number is given at all. Half an hour is also the practical figure: long enough that
/// renewals are rare, short enough that a control point which vanished without
/// unsubscribing stops being sent events within one.
pub const SUBSCRIPTION_SECS: u64 = 1800;

/// How many consecutive failed deliveries retire a subscription.
///
/// The spec does not say, and the two ends of the choice are both bad: drop too eagerly
/// and a control point loses eventing over one dropped packet; never drop and a phone that
/// left the building is notified for the life of the process, holding up delivery to
/// everyone behind it. Three is what the reference implementations settle on.
pub const MAX_DELIVERY_FAILURES: u32 = 3;

/// Which service a subscription is against.
///
/// An enum rather than the URN string because the three genuinely differ in what they
/// event and how — two wrap everything in `LastChange` and one does not — so a `match`
/// here is the thing that stops a ConnectionManager subscriber being sent an AVTransport
/// document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventedService {
    /// AVTransport: transport state, the current URI, what the transport can do now.
    AvTransport,
    /// RenderingControl: volume and mute.
    RenderingControl,
    /// ConnectionManager: what this renderer will accept. Fixed for the life of the
    /// process, so its subscribers hear once and never again.
    ConnectionManager,
}

impl EventedService {
    /// The service type URN, for the `NT`/`SVCID` a subscriber correlates on.
    #[must_use]
    pub const fn service_type(self) -> &'static str {
        match self {
            EventedService::AvTransport => crate::descriptions::service_types::AVTRANSPORT,
            EventedService::RenderingControl => {
                crate::descriptions::service_types::RENDERING_CONTROL
            }
            EventedService::ConnectionManager => {
                crate::descriptions::service_types::CONNECTION_MANAGER
            }
        }
    }

    /// The `(name, value)` properties a subscriber to this service should be sent, given
    /// the renderer's current state.
    ///
    /// One function for both shapes, because the difference is data rather than control
    /// flow: two services produce a single `LastChange` property and one produces three
    /// ordinary ones.
    #[must_use]
    pub fn properties(self, renderer: &Renderer) -> Vec<(&'static str, String)> {
        match self {
            EventedService::AvTransport => {
                vec![("LastChange", avt_last_change(renderer))]
            }
            EventedService::RenderingControl => {
                vec![("LastChange", rcs_last_change(renderer))]
            }
            EventedService::ConnectionManager => vec![
                ("SourceProtocolInfo", String::new()),
                (
                    "SinkProtocolInfo",
                    crate::state::sink_protocol_info().into(),
                ),
                ("CurrentConnectionIDs", "0".into()),
            ],
        }
    }
}

/// One live subscription.
#[derive(Debug, Clone)]
pub struct Subscription {
    /// The `SID` this subscriber quotes, including the `uuid:` prefix.
    pub sid: String,
    /// Which service it is against.
    pub service: EventedService,
    /// Where to deliver, in the order the subscriber listed them.
    ///
    /// A list because `CALLBACK` is one: UDA 1.1 §4.1.2 has the publisher try them in turn
    /// and stop at the first that answers, which is how a subscriber behind more than one
    /// interface tells us which of its addresses we can actually reach.
    pub callbacks: Vec<String>,
    /// The next `SEQ` to send. 0 is the initial event and only the initial event.
    pub seq: u32,
    /// When this subscription lapses unless renewed.
    pub expires: Instant,
    /// Consecutive failed deliveries.
    pub failures: u32,
}

impl Subscription {
    /// Take the next sequence number, wrapping as §4.3 requires.
    ///
    /// The wrap skips 0, and that is not an off-by-one to tidy away: 0 means *initial
    /// event*, carrying the complete state, and a subscriber that saw it again mid-session
    /// would be entitled to conclude it had missed everything in between.
    pub fn take_seq(&mut self) -> u32 {
        let seq = self.seq;
        self.seq = self.seq.checked_add(1).unwrap_or(1);
        seq
    }
}

/// What a `SUBSCRIBE` request turned out to be asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscribeRequest {
    /// A new subscription, with the callbacks to deliver to and the duration asked for.
    New {
        /// Callback URLs, in the order given.
        callbacks: Vec<String>,
        /// The duration requested, if a number was given. `None` means "infinite", which
        /// this service does not grant — it answers with its own.
        requested: Option<Duration>,
    },
    /// A renewal of an existing subscription.
    Renew {
        /// The `SID` being renewed.
        sid: String,
        /// The duration requested, if any.
        requested: Option<Duration>,
    },
}

/// Why a `SUBSCRIBE`/`UNSUBSCRIBE` could not be honoured.
///
/// Two codes and the distinction is UDA 1.1 §4.1.2's: 400 means the request was
/// incoherent, 412 means it was well-formed and referred to something that is not true —
/// most importantly a `SID` this publisher has never heard of, or has expired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeError {
    /// Missing or contradictory headers.
    BadRequest(&'static str),
    /// A precondition failed — an unknown `SID`, or an `NT` that is not `upnp:event`.
    PreconditionFailed(&'static str),
}

impl SubscribeError {
    /// The HTTP status a control point expects.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            SubscribeError::BadRequest(_) => 400,
            SubscribeError::PreconditionFailed(_) => 412,
        }
    }

    /// What went wrong, for the response body and the log.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            SubscribeError::BadRequest(r) | SubscribeError::PreconditionFailed(r) => r,
        }
    }
}

/// Read a `SUBSCRIBE` request's headers.
///
/// `callback`, `nt` and `sid` are the raw header values, absent when the header was not
/// sent. Header *names* are matched case-insensitively by the caller, because they are —
/// Microsoft sends `TransferMode.DLNA.ORG` where everyone else sends the lower-cased form,
/// and the same casual attitude applies here.
///
/// # Errors
/// [`SubscribeError`] when the combination is not one UDA 1.1 §4.1.2 defines.
pub fn parse_subscribe(
    callback: Option<&str>,
    nt: Option<&str>,
    sid: Option<&str>,
    timeout: Option<&str>,
) -> Result<SubscribeRequest, SubscribeError> {
    let requested = timeout.and_then(parse_timeout);
    match (callback, nt, sid) {
        // A renewal is a SID and nothing else. A request carrying both is ambiguous about
        // whether it wants a new subscription or an extended one, and §4.1.2 says to
        // refuse rather than guess.
        (None, None, Some(sid)) => Ok(SubscribeRequest::Renew {
            sid: sid.trim().to_string(),
            requested,
        }),
        (Some(_) | None, Some(_) | None, Some(_)) => Err(SubscribeError::BadRequest(
            "SID must not be combined with CALLBACK or NT",
        )),
        (Some(callback), Some(nt), None) => {
            if !nt.trim().eq_ignore_ascii_case("upnp:event") {
                return Err(SubscribeError::PreconditionFailed("NT must be upnp:event"));
            }
            let callbacks = parse_callbacks(callback);
            if callbacks.is_empty() {
                return Err(SubscribeError::PreconditionFailed(
                    "CALLBACK carried no usable http URL",
                ));
            }
            Ok(SubscribeRequest::New {
                callbacks,
                requested,
            })
        }
        _ => Err(SubscribeError::BadRequest(
            "a SUBSCRIBE needs either CALLBACK and NT, or SID",
        )),
    }
}

/// The URLs in a `CALLBACK` header: `<url><url>…`, angle-bracketed and unseparated.
///
/// Anything that is not an `http` URL is dropped rather than refused. A subscriber may
/// legitimately list an address family we cannot reach, and the header as a whole is still
/// usable as long as one entry is.
fn parse_callbacks(header: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else { break };
        let url = after[..close].trim();
        if url.to_ascii_lowercase().starts_with("http://") {
            out.push(url.to_string());
        }
        rest = &after[close + 1..];
    }
    out
}

/// `Second-1800` → 1800s; `Second-infinite` and anything unparseable → [`None`].
fn parse_timeout(header: &str) -> Option<Duration> {
    let value = header.trim();
    let rest = value.strip_prefix("Second-").or_else(|| {
        value
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("second-"))
            .map(|_| &value[7..])
    })?;
    rest.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// The `TIMEOUT` header value to answer with.
#[must_use]
pub fn timeout_header(granted: Duration) -> String {
    format!("Second-{}", granted.as_secs())
}

/// The subscriber table for all three services.
///
/// One table rather than one per service because a `SID` is unique across the device and
/// an `UNSUBSCRIBE` arrives at a URL the subscriber chose, not necessarily the one we would
/// have looked in.
#[derive(Debug, Default)]
pub struct Subscribers {
    live: Vec<Subscription>,
}

impl Subscribers {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many subscriptions are live. For tests and diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// Whether nothing is subscribed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Accept a new subscription, returning it so the caller can send the initial event.
    ///
    /// `sid` is supplied rather than generated so this module stays deterministic under
    /// test; the caller mints a v4 UUID.
    pub fn add(
        &mut self,
        sid: String,
        service: EventedService,
        callbacks: Vec<String>,
        granted: Duration,
        now: Instant,
    ) -> Subscription {
        let sub = Subscription {
            sid,
            service,
            callbacks,
            seq: 0,
            expires: now + granted,
            failures: 0,
        };
        self.live.push(sub.clone());
        sub
    }

    /// Extend an existing subscription. [`None`] if the `SID` is unknown or has lapsed.
    pub fn renew(&mut self, sid: &str, granted: Duration, now: Instant) -> Option<&Subscription> {
        let sub = self
            .live
            .iter_mut()
            .find(|s| s.sid == sid && s.expires > now)?;
        sub.expires = now + granted;
        Some(sub)
    }

    /// Drop a subscription. `false` if the `SID` was not one of ours.
    pub fn remove(&mut self, sid: &str) -> bool {
        let before = self.live.len();
        self.live.retain(|s| s.sid != sid);
        self.live.len() != before
    }

    /// Drop everything that has lapsed, returning how many went.
    ///
    /// Subscriptions expire rather than persist because the usual way one ends is a phone
    /// walking out of the building, not an `UNSUBSCRIBE`.
    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.live.len();
        self.live.retain(|s| s.expires > now);
        before - self.live.len()
    }

    /// Take the next sequence number for every live subscriber to `service`, returning
    /// what to deliver.
    ///
    /// Sequence numbers are handed out here, under the table's own lock, because they are
    /// per-subscription and must not interleave: two threads publishing at once would
    /// otherwise give one subscriber the same `SEQ` twice, which a subscriber is entitled
    /// to read as a duplicate and drop.
    pub fn prepare(
        &mut self,
        service: EventedService,
        now: Instant,
    ) -> Vec<(String, u32, Vec<String>)> {
        self.live
            .iter_mut()
            .filter(|s| s.service == service && s.expires > now)
            .map(|s| (s.sid.clone(), s.take_seq(), s.callbacks.clone()))
            .collect()
    }

    /// Take the next sequence number for *one* live subscription, by `SID`.
    ///
    /// The initial event after a SUBSCRIBE goes to exactly one subscriber, and taking it
    /// through [`Self::prepare`] and filtering the result would advance every other
    /// subscriber to the same service by one with nothing sent — a permanent gap in their
    /// SEQ, which UDA 1.1 §4.2 entitles them to read as a lost event and resync on.
    pub fn prepare_one(&mut self, sid: &str, now: Instant) -> Option<(u32, Vec<String>)> {
        self.live
            .iter_mut()
            .find(|s| s.sid == sid && s.expires > now)
            .map(|s| (s.take_seq(), s.callbacks.clone()))
    }

    /// Record that a delivery worked, clearing the failure count.
    pub fn delivered(&mut self, sid: &str) {
        if let Some(sub) = self.live.iter_mut().find(|s| s.sid == sid) {
            sub.failures = 0;
        }
    }

    /// Record a failed delivery, dropping the subscription once it has failed enough.
    /// Returns whether it was dropped.
    pub fn delivery_failed(&mut self, sid: &str) -> bool {
        let Some(sub) = self.live.iter_mut().find(|s| s.sid == sid) else {
            return false;
        };
        sub.failures += 1;
        if sub.failures >= MAX_DELIVERY_FAILURES {
            self.live.retain(|s| s.sid != sid);
            return true;
        }
        false
    }
}

/// Wrap properties in the `propertyset` document a `NOTIFY` body is.
///
/// UDA 1.1 §4.3. Every value is escaped, which for `LastChange` means the whole inner
/// document is escaped exactly once — it is XML travelling as text, the same nesting that
/// `CurrentURIMetaData` arrives in, and one round too few or too many is a subscriber that
/// sees either markup it cannot parse or a document with our tags dissolved into its own.
#[must_use]
pub fn propertyset(properties: &[(&str, String)]) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="utf-8"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">"#,
    );
    for (name, value) in properties {
        out.push_str("<e:property><");
        out.push_str(name);
        out.push('>');
        out.push_str(&crate::soap::xml_escape(value));
        out.push_str("</");
        out.push_str(name);
        out.push_str("></e:property>");
    }
    out.push_str("</e:propertyset>");
    out
}

/// The AVTransport `LastChange` document for the renderer's current state.
///
/// Position and track duration are both deliberately absent. §2.3.1 excludes position, and
/// duration follows it for a reason of our own: both are read from the pipeline per
/// request rather than stored, so putting either here would make the change-diff differ on
/// every poll — one event per second, per subscriber, for a number nobody asked to be
/// pushed.
#[must_use]
pub fn avt_last_change(renderer: &Renderer) -> String {
    let mut out = String::from(
        r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0">"#,
    );
    let mut add = |name: &str, value: &str| {
        out.push('<');
        out.push_str(name);
        out.push_str(r#" val=""#);
        out.push_str(&crate::soap::xml_escape(value));
        out.push_str(r#""/>"#);
    };
    add("TransportState", renderer.state.as_upnp());
    add("TransportStatus", renderer.status.as_upnp());
    add("TransportPlaySpeed", "1");
    add("NumberOfTracks", "1");
    add("CurrentTrack", "1");
    add("CurrentPlayMode", "NORMAL");
    add(
        "AVTransportURI",
        renderer.current_uri.as_deref().unwrap_or_default(),
    );
    add("AVTransportURIMetaData", &renderer.current_uri_metadata);
    add(
        "CurrentTrackURI",
        renderer.current_uri.as_deref().unwrap_or_default(),
    );
    add("CurrentTrackMetaData", &renderer.current_uri_metadata);
    add(
        "NextAVTransportURI",
        renderer.next_uri.as_deref().unwrap_or_default(),
    );
    add("NextAVTransportURIMetaData", &renderer.next_uri_metadata);
    add("CurrentTransportActions", &renderer.available_actions());
    out.push_str("</InstanceID></Event>");
    out
}

/// The RenderingControl `LastChange` document.
///
/// Both variables carry `channel="Master"`, which is not decoration: RCS models every
/// level per channel, and a `Volume` element with no channel is one a subscriber cannot
/// place.
#[must_use]
pub fn rcs_last_change(renderer: &Renderer) -> String {
    format!(
        concat!(
            r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/"><InstanceID val="0">"#,
            r#"<Volume channel="Master" val="{volume}"/>"#,
            r#"<Mute channel="Master" val="{mute}"/>"#,
            r#"<PresetNameList val="FactoryDefaults"/>"#,
            "</InstanceID></Event>",
        ),
        volume = renderer.volume,
        mute = u8::from(renderer.muted),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn at() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_new_subscription_needs_callback_and_nt_and_a_renewal_needs_only_a_sid() {
        let new = parse_subscribe(
            Some("<http://10.0.0.5:1234/notify>"),
            Some("upnp:event"),
            None,
            Some("Second-300"),
        )
        .unwrap();
        assert_eq!(
            new,
            SubscribeRequest::New {
                callbacks: vec!["http://10.0.0.5:1234/notify".into()],
                requested: Some(Duration::from_secs(300)),
            }
        );

        let renew = parse_subscribe(None, None, Some("uuid:abc"), None).unwrap();
        assert_eq!(
            renew,
            SubscribeRequest::Renew {
                sid: "uuid:abc".into(),
                requested: None,
            }
        );
    }

    /// The refusals a control point acts on differently: 400 says the message was
    /// incoherent, 412 says it referred to something untrue — most importantly a `SID` we
    /// have never heard of, which is how a subscriber learns to start over.
    #[test]
    fn incoherent_and_untrue_subscribes_are_refused_differently() {
        // Both a SID and a CALLBACK: is this new or a renewal? §4.1.2 says do not guess.
        let both = parse_subscribe(
            Some("<http://h/cb>"),
            Some("upnp:event"),
            Some("uuid:abc"),
            None,
        )
        .unwrap_err();
        assert_eq!(both.status(), 400);

        // Neither.
        assert_eq!(
            parse_subscribe(None, None, None, None)
                .unwrap_err()
                .status(),
            400
        );

        // Well-formed, but asking to be notified of something that is not events.
        let wrong_nt = parse_subscribe(Some("<http://h/cb>"), Some("upnp:propchange"), None, None)
            .unwrap_err();
        assert_eq!(wrong_nt.status(), 412);

        // A callback we could never deliver to.
        let unusable =
            parse_subscribe(Some("<ftp://h/cb>"), Some("upnp:event"), None, None).unwrap_err();
        assert_eq!(unusable.status(), 412);
    }

    /// `CALLBACK` is a list with no separator, and a subscriber behind two interfaces uses
    /// it to say "reach me at whichever of these works".
    #[test]
    fn every_callback_url_is_kept_in_order() {
        let got = parse_callbacks("<http://10.0.0.5:9/a><http://[::1]:9/b>");
        assert_eq!(got, vec!["http://10.0.0.5:9/a", "http://[::1]:9/b"]);
        // A malformed tail does not lose the entries before it.
        assert_eq!(
            parse_callbacks("<http://h/a><http://h/b"),
            vec!["http://h/a"]
        );
    }

    #[test]
    fn timeouts_are_read_when_they_are_numbers_and_ignored_when_they_are_not() {
        assert_eq!(parse_timeout("Second-300"), Some(Duration::from_secs(300)));
        assert_eq!(parse_timeout("second-300"), Some(Duration::from_secs(300)));
        // "infinite" is a duration this service does not grant, so it falls back to ours.
        assert_eq!(parse_timeout("Second-infinite"), None);
        assert_eq!(parse_timeout("nonsense"), None);
        assert_eq!(timeout_header(Duration::from_secs(1800)), "Second-1800");
    }

    /// SEQ 0 means *initial event*, carrying the complete state. A subscriber that saw it
    /// again mid-session would be entitled to conclude it had missed everything between.
    #[test]
    fn sequence_numbers_start_at_zero_and_never_return_to_it() {
        let mut sub = Subscription {
            sid: "uuid:a".into(),
            service: EventedService::AvTransport,
            callbacks: vec![],
            seq: 0,
            expires: at(),
            failures: 0,
        };
        assert_eq!(sub.take_seq(), 0);
        assert_eq!(sub.take_seq(), 1);
        sub.seq = u32::MAX;
        assert_eq!(sub.take_seq(), u32::MAX);
        assert_eq!(sub.take_seq(), 1, "the wrap must skip the initial event");
    }

    #[test]
    fn subscriptions_are_added_renewed_expired_and_removed() {
        let now = at();
        let mut subs = Subscribers::new();
        assert!(subs.is_empty());

        subs.add(
            "uuid:a".into(),
            EventedService::AvTransport,
            vec!["http://h/cb".into()],
            Duration::from_secs(60),
            now,
        );
        assert_eq!(subs.len(), 1);

        // A renewal moves the deadline; an unknown SID is not silently created.
        assert!(subs
            .renew("uuid:a", Duration::from_secs(600), now)
            .is_some());
        assert!(subs
            .renew("uuid:nope", Duration::from_secs(600), now)
            .is_none());

        // Nothing has lapsed yet, and the one that has is the only one that goes.
        assert_eq!(subs.expire(now + Duration::from_secs(300)), 0);
        assert_eq!(subs.expire(now + Duration::from_secs(3_600)), 1);
        assert!(subs.is_empty());

        subs.add(
            "uuid:b".into(),
            EventedService::RenderingControl,
            vec![],
            Duration::from_secs(60),
            now,
        );
        assert!(subs.remove("uuid:b"));
        assert!(!subs.remove("uuid:b"));
    }

    /// Delivery only goes to subscribers of *that* service, and each gets its own sequence
    /// — the numbers are per-subscription, so two services publishing must not share one.
    #[test]
    fn preparing_a_publish_touches_only_that_services_subscribers() {
        let now = at();
        let mut subs = Subscribers::new();
        subs.add(
            "uuid:avt".into(),
            EventedService::AvTransport,
            vec!["http://h/a".into()],
            Duration::from_secs(60),
            now,
        );
        subs.add(
            "uuid:rcs".into(),
            EventedService::RenderingControl,
            vec!["http://h/r".into()],
            Duration::from_secs(60),
            now,
        );

        let batch = subs.prepare(EventedService::AvTransport, now);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0, "uuid:avt");
        assert_eq!(batch[0].1, 0, "the first publish is the initial event");

        let again = subs.prepare(EventedService::AvTransport, now);
        assert_eq!(again[0].1, 1);
        // …and the other service is untouched by either.
        let rcs = subs.prepare(EventedService::RenderingControl, now);
        assert_eq!(rcs[0].1, 0);
    }

    #[test]
    fn the_initial_event_does_not_spend_another_subscribers_sequence_number() {
        // A second SUBSCRIBE to a service must not move the first subscriber's counter.
        // It used to: the initial event was taken through prepare() and filtered down to
        // the new SID, so everyone else advanced with nothing sent and their next real
        // event arrived with a SEQ they had never been given the predecessor of — which
        // UDA 1.1 §4.2 lets them treat as a lost event and resync on. Home Assistant
        // stops polling once subscribed, so events being right is all it has.
        let now = at();
        let mut subs = Subscribers::new();
        for sid in ["uuid:first", "uuid:second"] {
            subs.add(
                sid.into(),
                EventedService::AvTransport,
                vec!["http://h/a".into()],
                Duration::from_secs(60),
                now,
            );
        }

        assert_eq!(
            subs.prepare_one("uuid:first", now).map(|(seq, _)| seq),
            Some(0)
        );
        assert_eq!(
            subs.prepare_one("uuid:second", now).map(|(seq, _)| seq),
            Some(0)
        );

        // The first subscriber's next event is 1 — consecutive with the 0 it was sent.
        let batch = subs.prepare(EventedService::AvTransport, now);
        let first = batch.iter().find(|(s, _, _)| s == "uuid:first").unwrap();
        assert_eq!(first.1, 1, "no gap left by the newcomer's initial event");

        assert_eq!(subs.prepare_one("uuid:nobody", now), None);
    }

    /// A phone that left the building must stop being notified, or it holds up delivery to
    /// everyone behind it for the life of the process.
    #[test]
    fn a_subscriber_that_stops_answering_is_dropped_but_not_on_the_first_miss() {
        let now = at();
        let mut subs = Subscribers::new();
        subs.add(
            "uuid:a".into(),
            EventedService::AvTransport,
            vec!["http://gone/cb".into()],
            Duration::from_secs(60),
            now,
        );
        for _ in 1..MAX_DELIVERY_FAILURES {
            assert!(!subs.delivery_failed("uuid:a"));
        }
        assert!(subs.delivery_failed("uuid:a"));
        assert!(subs.is_empty());

        // …and a delivery that works clears the count, so an occasional dropped packet
        // never accumulates into a dropped subscription.
        subs.add(
            "uuid:b".into(),
            EventedService::AvTransport,
            vec![],
            Duration::from_secs(60),
            now,
        );
        assert!(!subs.delivery_failed("uuid:b"));
        subs.delivered("uuid:b");
        for _ in 1..MAX_DELIVERY_FAILURES {
            assert!(!subs.delivery_failed("uuid:b"));
        }
    }

    /// The nesting that decides whether a subscriber sees anything: `LastChange` is an XML
    /// document travelling as *text*, so it is escaped exactly once. One round too few and
    /// our tags dissolve into the subscriber's document; one too many and it sees markup
    /// it cannot parse.
    #[test]
    fn the_last_change_document_is_escaped_exactly_once_inside_the_propertyset() {
        let mut r = Renderer::default();
        r.state = crate::state::TransportState::Playing;
        r.current_uri = Some("http://h/a.mp4?x=1&y=2".into());

        let inner = avt_last_change(&r);
        assert!(inner.starts_with("<Event "));
        assert!(inner.contains(r#"<TransportState val="PLAYING"/>"#));
        // The URI's own ampersand is escaped inside the inner document, once.
        assert!(inner.contains("x=1&amp;y=2"));

        let body = propertyset(&[("LastChange", inner)]);
        // In the outer document the whole inner one is text, so its markup is escaped…
        assert!(body.contains("&lt;Event "));
        assert!(!body.contains("<Event "));
        // …and the ampersand that was already an entity is escaped again, which is what
        // makes it survive both parsers and arrive as a single `&`.
        assert!(body.contains("x=1&amp;amp;y=2"));
        assert!(body.contains("<e:property><LastChange>"));
    }

    /// ConnectionManager events its variables directly — no `LastChange` wrapper — and
    /// getting that backwards means a subscriber parsing a document it never asked for.
    #[test]
    fn connection_manager_events_its_variables_directly() {
        let r = Renderer::default();
        let props = EventedService::ConnectionManager.properties(&r);
        let names: Vec<_> = props.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "SourceProtocolInfo",
                "SinkProtocolInfo",
                "CurrentConnectionIDs"
            ]
        );
        assert!(props[1].1.contains("http-get:*:video/mp4:*"));

        // …where the other two wrap everything in one.
        for service in [
            EventedService::AvTransport,
            EventedService::RenderingControl,
        ] {
            let props = service.properties(&r);
            assert_eq!(props.len(), 1);
            assert_eq!(props[0].0, "LastChange");
        }
    }

    #[test]
    fn rendering_control_events_carry_the_channel_they_belong_to() {
        let mut r = Renderer::default();
        r.volume = 73;
        r.muted = true;
        let doc = rcs_last_change(&r);
        assert!(doc.contains(r#"<Volume channel="Master" val="73"/>"#));
        assert!(doc.contains(r#"<Mute channel="Master" val="1"/>"#));
    }
}
