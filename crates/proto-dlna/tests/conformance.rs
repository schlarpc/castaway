//! The executable half of `docs/dlna-conformance.md`.
//!
//! That document exists to stop the next person "fixing" things that are already right,
//! and it says twice that the obvious-looking change was the wrong one. Prose cannot say
//! so at the moment somebody makes the change; these tests can.
//!
//! Every test here names the row of the doc's "Confirmed correct — do not fix" table it
//! guards and quotes the citation the row rests on, so a failure reads as *why* rather
//! than as a bare inequality. If one of these fails, the fix is almost never to change
//! the assertion — read the citation first, and if it really is wrong, change the document
//! in the same commit.
//!
//! Deliberately exact rather than `contains`-shaped where the doc's claim is exactness:
//! a row that says "1.50, not 1.51" is not guarded by a test that would also pass on 1.51.

#![allow(clippy::unwrap_used)]

use proto_dlna::descriptions::{self, paths, service_types};
use proto_dlna::soap::SoapAction;
use proto_dlna::state::Renderer;

fn action(name: &str, args: &[(&str, &str)]) -> SoapAction {
    SoapAction {
        name: name.into(),
        args: args
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    }
}

/// Row 1 — `GetCurrentConnectionInfo`'s default-connection out-args.
///
/// ConnectionManager:1 §2.2.3: *"If optional action PrepareForConnection is not
/// implemented then this state variable should be set to '0'."* This is the prescribed
/// shape for a renderer that has no connection model, not a stub somebody left behind,
/// and `PeerConnectionID = -1` in particular reads as "no peer" rather than as "peer 0".
///
/// `state.rs::every_advertised_action_is_answered_and_every_answered_action_is_advertised`
/// reaches this action but only asserts it does not 401/602 — it never looks at what comes
/// back, so a refactor to `PeerConnectionID = 0` breaks no test. This one.
#[test]
fn the_default_connection_reports_the_shape_connectionmanager_prescribes() {
    let r = Renderer::default();
    let out = r
        .connection_manager(&action(
            "GetCurrentConnectionInfo",
            &[("ConnectionID", "0")],
        ))
        .unwrap()
        .out_args;
    let by_name = |n: &str| -> String {
        out.iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("GetCurrentConnectionInfo omitted the {n} out-arg"))
    };

    assert_eq!(
        by_name("RcsID"),
        "0",
        "CM:1 §2.2.3 — the default RCS instance"
    );
    assert_eq!(
        by_name("AVTransportID"),
        "0",
        "CM:1 §2.2.3 — the default AVTransport instance"
    );
    assert_eq!(
        by_name("PeerConnectionID"),
        "-1",
        "CM:1 §2.2.3 — -1 is 'no peer'. 0 names connection zero, which is a different \
         and untrue statement"
    );
    assert_eq!(
        by_name("PeerConnectionManager"),
        "",
        "there is no peer connection manager to name"
    );
    assert_eq!(
        by_name("Direction"),
        "Input",
        "a renderer sinks, it does not source"
    );
    assert_eq!(by_name("Status"), "OK");
    assert!(
        !by_name("ProtocolInfo").is_empty(),
        "the connection has to say what it accepts, or a control point cannot pick a format"
    );

    // …and the companion action, whose single out-arg names that same connection.
    let ids = r
        .connection_manager(&action("GetCurrentConnectionIDs", &[]))
        .unwrap()
        .out_args;
    assert_eq!(ids, vec![("ConnectionIDs".to_string(), "0".to_string())]);
}

/// Row 2 — `X_DLNADOC` says `DMR-1.50`, and there is no `M-DMR`.
///
/// Rygel ships a `Dlna150Hacks` class that string-replaces `-1.51` → `-1.50` per
/// User-Agent, because control points choke on 1.51. `M-DMR` is for devices advertising a
/// *reduced* mandatory media-format set, which a fixed panel with a full ffmpeg behind it
/// is not — claiming it invites a sender to withhold formats we can play.
#[test]
fn the_dlna_device_profile_is_dmr_150_and_nothing_else() {
    let xml = descriptions::device_description("Panel", "abc-123");
    assert!(
        xml.contains(">DMR-1.50<"),
        "X_DLNADOC must say DMR-1.50; do not 'upgrade' to 1.51"
    );
    assert!(
        !xml.contains("1.51"),
        "1.51 is the version control points choke on"
    );
    assert!(
        !xml.contains("M-DMR"),
        "M-DMR advertises a reduced mandatory format set, which this renderer does not have"
    );
}

/// Row 3 — `urn:schemas-dlna-org:device-1-0` has **no** trailing slash.
///
/// It is distinct from the *metadata* namespace `urn:schemas-dlna-org:metadata-1-0/`,
/// which does have one. They look like a typo of each other and are not, which is exactly
/// why one gets "corrected" into the other.
#[test]
fn the_dlna_device_namespace_has_no_trailing_slash() {
    let xml = descriptions::device_description("Panel", "abc-123");
    assert!(
        xml.contains(r#"xmlns:dlna="urn:schemas-dlna-org:device-1-0""#),
        "the device namespace takes no trailing slash"
    );
    assert!(
        !xml.contains("urn:schemas-dlna-org:device-1-0/"),
        "a trailing slash here is the metadata namespace's, not this one's"
    );
}

/// Row 4 — `<iconList>` is omitted.
///
/// UDA 1.1 §2.3 makes it REQUIRED if and only if the device *has* icons. We serve none,
/// so an empty or placeholder `<iconList>` would be a description promising an icon URL
/// that 404s — worse than the omission it was added to fix.
#[test]
fn no_iconlist_is_advertised_because_there_are_no_icons() {
    let xml = descriptions::device_description("Panel", "abc-123");
    assert!(!xml.contains("iconList"));
    assert!(!xml.contains("<icon>"));
}

/// The description is well-formed XML, **including with a hostile friendly name**.
///
/// This is the bug that shipped, recorded at `descriptions.rs:88`: a panel named
/// `Bar & Grill` produced XML no control point's parser accepted. The device answered
/// M-SEARCH, served its LOCATION with a 200, logged nothing, and appeared in no picker
/// anywhere. The fix was an escape call; what was missing was anything that would fail if
/// the call were removed — `device_description_embeds_name_and_udn` passes a benign name,
/// and the VM test asserts the LOCATION returns 200 and never that the body parses.
///
/// DIAL got this right (`Test & Screen`); DLNA did not.
#[test]
fn a_hostile_friendly_name_still_produces_a_document_a_parser_accepts() {
    // Every metacharacter that has ever come off a name field: the ampersand that shipped
    // the bug, angle brackets that would forge an element, quotes that would escape an
    // attribute, and a stray `]]>` that closes a CDATA section we do not open but a lenient
    // sender's parser might.
    let hostile = r#"Bar & Grill <TV> "quoted" ]]> '"#;
    let xml = descriptions::device_description(hostile, "abc & 123");

    // Strict: no `allow_dangling_amp`. Inbound leniency is the right posture for DIDL a
    // control point sent us (the conformance doc says so); *outbound* we are the one
    // required to be well-formed, and this is the parser a control point brings.
    let name = read_element_text(&xml, "friendlyName").unwrap_or_else(|e| {
        panic!(
            "a friendly name made the device description unparseable — this is the failure \
             that shipped, and it is silent: {e}\n{xml}"
        )
    });

    // Not merely parseable — parseable back to the *same string*. Escaping that mangles
    // the name still produces well-formed XML, and puts the wrong thing in the picker.
    assert_eq!(
        name.as_deref(),
        Some(hostile),
        "the name round-trips through the escape or the panel is listed under a different one"
    );

    // The UDN takes the same treatment. It is normally a UUID and normally safe, and
    // `DlnaService::new` takes it from configuration — so "it is always a UUID" is a
    // property of the caller, not of this function.
    let udn = read_element_text(&xml, "UDN").unwrap();
    assert_eq!(udn.as_deref(), Some("uuid:abc & 123"));
}

/// The text of the first `name` element, with entity references resolved — i.e. what a
/// control point's parser would show. `Err` if the document is not well-formed at all.
fn read_element_text(xml: &str, name: &str) -> Result<Option<String>, quick_xml::Error> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    let (mut inside, mut text) = (false, String::new());
    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                inside = e.local_name().as_ref() == name.as_bytes();
            }
            // Character data arrives in fragments split around entity references, so the
            // value is only complete at the closing tag — the same shape `didl::parse`
            // deals with, and the reason a naive "first Text event" read truncates
            // `Bar & Grill` to `Bar `.
            Event::Text(t) if inside => text.push_str(&t.xml10_content()?),
            Event::GeneralRef(r) if inside => {
                // The five predefined entities are all `xml_escape` emits.
                let resolved = match r.as_ref() {
                    b"amp" => "&",
                    b"lt" => "<",
                    b"gt" => ">",
                    b"quot" => "\"",
                    b"apos" => "'",
                    other => panic!(
                        "the description used an entity no plain XML parser resolves: \
                         &{};",
                        String::from_utf8_lossy(other)
                    ),
                };
                text.push_str(resolved);
            }
            Event::End(e) if e.local_name().as_ref() == name.as_bytes() => {
                return Ok(Some(text));
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

/// The three SCPDs are well-formed too. They are static, so this can only fail on an edit
/// — which is when it is worth knowing, because a malformed SCPD makes every action in
/// that service uncallable while discovery still succeeds.
#[test]
fn every_served_document_is_well_formed() {
    let docs: [(&str, &str); 4] = [
        (
            paths::DESCRIPTION,
            &descriptions::device_description("Panel", "abc-123"),
        ),
        (paths::AVT_SCPD, descriptions::AVTRANSPORT_SCPD),
        (paths::RC_SCPD, descriptions::RENDERING_CONTROL_SCPD),
        (paths::CM_SCPD, descriptions::CONNECTION_MANAGER_SCPD),
    ];
    for (path, body) in docs {
        if let Err(e) = read_element_text(body, "nothing-in-particular") {
            panic!("{path} is not well-formed XML: {e}");
        }
    }
}

/// Row 5 — `state::parse_upnp_time` and `didl::parse_duration` are **separate parsers**.
///
/// The grammars genuinely differ, and merging them is the tempting wrong move.
/// `TimeSeekRange` will need a *third* (RFC 2326 NPT, where a bare-seconds form like
/// `npt=123.45-125` is legal).
///
/// Neither function is public, so this asserts the divergence through the two public
/// entry points that reach them, on the one input that discriminates: `0:3:45`.
/// AVTransport's `Seek` target is `H+:MM:SS` with a lenient hour field and takes it; the
/// DIDL `res@duration` pattern requires exactly two digits per field and refuses it,
/// because accepting `0:3:45` means accepting `0:345:00` as well.
///
/// A merged parser flips one of these two, whichever direction it merges in.
#[test]
fn the_two_duration_grammars_stay_separate_because_they_are_not_the_same_grammar() {
    let mut r = Renderer::default();
    r.av_transport(&action(
        "SetAVTransportURI",
        &[("CurrentURI", "http://h/a.mp4")],
    ))
    .unwrap();
    r.av_transport(&action("Play", &[])).unwrap();

    // AVTransport takes the single-digit minute field.
    assert!(
        r.av_transport(&action(
            "Seek",
            &[("Unit", "REL_TIME"), ("Target", "0:3:45")]
        ))
        .is_ok(),
        "UPnP's seek target grammar is lenient about field width; a Seek that refuses \
         this is a control point that cannot scrub"
    );

    // DIDL does not — and so reports no duration rather than a wrong one.
    let didl = proto_dlna::didl::parse(
        r#"<DIDL-Lite><item><dc:title>t</dc:title>
           <res duration="0:3:45">http://h/a.mp3</res></item></DIDL-Lite>"#,
    );
    assert_eq!(
        didl.duration, None,
        "res@duration is `H+:MM:SS`; accepting a one-digit minute means accepting \
         `0:345:00`, which parses to a plausible and wrong number"
    );

    // Both take the conformant spelling, so the test above is about the grammar and not
    // about one of the two parsers being broken outright.
    assert!(r
        .av_transport(&action(
            "Seek",
            &[("Unit", "REL_TIME"), ("Target", "0:03:45")]
        ))
        .is_ok());
    let didl = proto_dlna::didl::parse(
        r#"<DIDL-Lite><item><dc:title>t</dc:title>
           <res duration="0:03:45">http://h/a.mp3</res></item></DIDL-Lite>"#,
    );
    assert_eq!(didl.duration, Some(std::time::Duration::from_secs(225)));
}

/// Row 6 — `upnp:class` is matched by **substring**, not equality.
///
/// Vendors legally extend the class taxonomy. `didl.rs`'s own tests cover class *parsing*
/// for the canonical spellings; none of them presents a vendor-extended class, which is
/// the case the substring rule exists for. An equality match reads every one of these as
/// `Unknown`, and the card falls back to a generic label for the sender that bothered to
/// say what it was sending.
#[test]
fn a_vendor_extended_upnp_class_still_reads_as_its_base_kind() {
    use proto_dlna::didl::ItemKind;

    let cases: [(&str, ItemKind); 6] = [
        (
            "object.item.audioItem.musicTrack.someVendorThing",
            ItemKind::Audio,
        ),
        (
            "object.item.videoItem.movie.x-vendorExtension",
            ItemKind::Video,
        ),
        ("object.item.imageItem.photo.vendorPhoto", ItemKind::Image),
        // Case-insensitively, too: the taxonomy is camelCase and senders are not.
        ("OBJECT.ITEM.AUDIOITEM.MUSICTRACK", ItemKind::Audio),
        // The base classes themselves, unextended.
        ("object.item.audioItem", ItemKind::Audio),
        // And something genuinely outside the taxonomy stays Unknown rather than
        // guessing — a substring rule that matched this would be too loose.
        ("object.container.storageFolder", ItemKind::Unknown),
    ];

    for (class, expected) in cases {
        let didl = proto_dlna::didl::parse(&format!(
            r#"<DIDL-Lite><item><dc:title>t</dc:title>
               <upnp:class>{class}</upnp:class></item></DIDL-Lite>"#
        ));
        assert_eq!(
            didl.kind, expected,
            "upnp:class `{class}` read as {:?}",
            didl.kind
        );
    }
}

/// The service list the description advertises is the one the SCPDs are served for.
///
/// Not a row of the table, but the join every other row assumes: a description that names
/// a `SCPDURL` nothing serves is a device that discovers and then fails to describe, which
/// looks from the sender's side exactly like the unparseable-XML failure above.
#[test]
fn every_advertised_service_url_is_one_the_router_serves() {
    let xml = descriptions::device_description("Panel", "abc-123");
    for path in [
        paths::AVT_SCPD,
        paths::RC_SCPD,
        paths::CM_SCPD,
        paths::AVT_CONTROL,
        paths::RC_CONTROL,
        paths::CM_CONTROL,
        paths::AVT_EVENT,
        paths::RC_EVENT,
        paths::CM_EVENT,
    ] {
        assert!(xml.contains(path), "the description does not name {path}");
    }
    for urn in [
        service_types::AVTRANSPORT,
        service_types::RENDERING_CONTROL,
        service_types::CONNECTION_MANAGER,
        service_types::MEDIA_RENDERER,
    ] {
        assert!(xml.contains(urn), "the description does not name {urn}");
    }
}
