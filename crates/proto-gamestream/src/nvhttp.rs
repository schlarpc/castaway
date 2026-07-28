//! NVHTTP — the GameStream control API, as request builders and response types.
//!
//! Pure: this module turns intent into a path+query string and turns a response body
//! into a rich type. It never opens a socket ([`crate::http`] does that), so every
//! query encoding below is asserted against a string in a test rather than against a
//! host.
//!
//! Two transports, one API (docs/gamestream-protocol-notes.md §4): `/serverinfo` and
//! `/pair` answer on plain HTTP 47989, everything else requires HTTPS 47984 with our
//! client certificate. Every response is XML whose `<root>` carries a `status_code`
//! attribute; `200` is success and anything else is an error *with a body*, so the
//! HTTP status alone is never the verdict — and neither is `status_code` for pairing,
//! which reports rejection as `200` plus `<paired>0</paired>`.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::GameStreamError;
use crate::pairing::{hex_encode, PhaseRequest};

/// Default NVHTTP plaintext port. The TLS port is read from `/serverinfo`, never
/// assumed — Sunshine derives it from a configurable base port.
pub const DEFAULT_HTTP_PORT: u16 = 47989;
/// Fallback TLS port when `/serverinfo` omits `HttpsPort`.
pub const DEFAULT_HTTPS_PORT: u16 = 47984;

/// Which listener a request must go to. The distinction is load-bearing: an HTTPS-only
/// endpoint asked over HTTP returns a 404 body, and `/serverinfo` reports `PairStatus`
/// truthfully only over TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Plain HTTP on [`DEFAULT_HTTP_PORT`] — `/serverinfo` and `/pair` only.
    Plain,
    /// HTTPS with our client certificate and the pinned host certificate.
    Tls,
}

/// A built NVHTTP request: everything but the host and the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Which listener to send it to.
    pub transport: Transport,
    /// Path and query, e.g. `/applist?uniqueid=…`.
    pub path_and_query: String,
}

/// The client's stable identity string in every request. Moonlight uses the lowercase
/// hex of a random u64; Sunshine keys its pairing sessions on it, so all four pairing
/// calls must carry the same value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueId(String);

impl UniqueId {
    /// Wrap a persisted id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Generate a fresh one, Moonlight's shape: lowercase hex of 8 random bytes.
    #[must_use]
    pub fn generate() -> Self {
        use rand::Rng;
        Self(format!("{:x}", rand::thread_rng().gen::<u64>()))
    }

    /// The wire form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Builds every NVHTTP request for one host. Holds the identity because Sunshine
/// requires `uniqueid` on all of them.
#[derive(Debug, Clone)]
pub struct RequestBuilder {
    unique_id: UniqueId,
}

impl RequestBuilder {
    /// Build for a given client identity string.
    #[must_use]
    pub fn new(unique_id: UniqueId) -> Self {
        Self { unique_id }
    }

    /// The prefix every request carries. `uuid` is per-request and ignored by
    /// Sunshine, but GFE wanted it, so it is sent.
    fn prefix(&self, uuid: &str) -> String {
        format!("uniqueid={}&uuid={uuid}", self.unique_id.as_str())
    }

    /// `/serverinfo`. Over [`Transport::Tls`] it also reports real pairing status.
    #[must_use]
    pub fn server_info(&self, transport: Transport, uuid: &str) -> Request {
        Request {
            transport,
            path_and_query: format!("/serverinfo?{}", self.prefix(uuid)),
        }
    }

    /// One `/pair` phase. Phases 1–4 go over plain HTTP; the final `pairchallenge`
    /// goes over TLS, which is the point of it.
    #[must_use]
    pub fn pair(&self, phase: &PhaseRequest, transport: Transport, uuid: &str) -> Request {
        let mut q = self.prefix(uuid);
        // devicename/updateState are ignored by Sunshine but sent for GFE parity.
        q.push_str("&devicename=roth&updateState=1");
        if let Some(phrase) = phase.phrase {
            q.push_str("&phrase=");
            q.push_str(phrase);
        }
        for (key, value) in &phase.extra {
            q.push('&');
            q.push_str(key);
            q.push('=');
            q.push_str(value);
        }
        q.push('&');
        q.push_str(phase.param.0);
        q.push('=');
        q.push_str(&phase.param.1);
        Request {
            transport,
            path_and_query: format!("/pair?{q}"),
        }
    }

    /// `/applist` (TLS only).
    #[must_use]
    pub fn app_list(&self, uuid: &str) -> Request {
        Request {
            transport: Transport::Tls,
            path_and_query: format!("/applist?{}", self.prefix(uuid)),
        }
    }

    /// `/launch` or `/resume` (TLS only) — which one is decided by whether the host
    /// already has an app running, so it is part of [`LaunchParams`].
    #[must_use]
    pub fn launch(&self, params: &LaunchParams, uuid: &str) -> Request {
        let verb = if params.resume { "resume" } else { "launch" };
        let mut q = self.prefix(uuid);
        // Order mirrors moonlight-qt. Sunshine parses by name, but a host that ever
        // cared would care about this order.
        q.push_str(&format!("&appid={}", params.app_id));
        q.push_str(&format!(
            "&mode={}x{}x{}",
            params.width, params.height, params.fps
        ));
        q.push_str("&additionalStates=1");
        q.push_str(&format!("&sops={}", u8::from(params.optimize_settings)));
        q.push_str(&format!("&rikey={}", hex_encode(&params.ri_key)));
        q.push_str(&format!("&rikeyid={}", params.ri_key_id));
        q.push_str(&format!(
            "&localAudioPlayMode={}",
            u8::from(params.play_audio_on_host)
        ));
        q.push_str(&format!(
            "&surroundAudioInfo={}",
            params.surround_audio_info
        ));
        q.push_str("&remoteControllersBitmap=0&gcmap=0&gcpersist=0");
        // corever=1 asks for the encrypted control/RTSP protocol. Sunshine rejects a
        // client without it when its encryption mode is mandatory, and
        // moonlight-common-c handles both variants, so it is always sent.
        q.push_str("&corever=1");
        Request {
            transport: Transport::Tls,
            path_and_query: format!("/{verb}?{q}"),
        }
    }

    /// `/cancel` (TLS only) — stops whatever is running on the host.
    #[must_use]
    pub fn cancel(&self, uuid: &str) -> Request {
        Request {
            transport: Transport::Tls,
            path_and_query: format!("/cancel?{}", self.prefix(uuid)),
        }
    }
}

/// Everything `/launch` needs. The AES key and its id are generated per session and
/// must be handed to the streaming core verbatim — they key the input, control, and
/// audio encryption.
#[derive(Debug, Clone)]
pub struct LaunchParams {
    /// The app to start; `0` means "just the desktop".
    pub app_id: i64,
    /// Send `/resume` instead of `/launch` (an app is already running).
    pub resume: bool,
    /// Requested stream width in pixels.
    pub width: u32,
    /// Requested stream height in pixels.
    pub height: u32,
    /// Requested frame rate.
    pub fps: u32,
    /// "Optimize game settings" — lets the host change the game's own resolution.
    pub optimize_settings: bool,
    /// Also play audio on the host's speakers.
    pub play_audio_on_host: bool,
    /// `(channelMask << 16) | channelCount`; 196610 is stereo.
    pub surround_audio_info: u32,
    /// The 16-byte AES key for input/control/audio.
    pub ri_key: [u8; 16],
    /// The first four IV bytes, big-endian, as a *signed* decimal — it is routinely
    /// negative, and a host parsing it as unsigned would derive a different IV.
    pub ri_key_id: i32,
}

/// `/serverinfo`, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// The host's friendly name.
    pub hostname: String,
    /// `appversion`; its major component selects the pairing hash, and a `-1` fourth
    /// component means Sunshine.
    pub app_version: String,
    /// `GfeVersion`, passed verbatim to the streaming core.
    pub gfe_version: String,
    /// The TLS port to use for everything else.
    pub https_port: u16,
    /// Whether this client is paired. Only meaningful when asked over TLS.
    pub paired: bool,
    /// The app id currently running, `0` when idle — decides launch vs resume.
    pub current_game: i64,
    /// `SUNSHINE_SERVER_FREE`/`_BUSY`, or GFE's states.
    pub state: String,
    /// The `ServerCodecModeSupport` bitmask; absent means H.264 only.
    pub codec_mode_support: u32,
}

impl ServerInfo {
    /// Whether the host is Sunshine, by its self-identifying `-1` fourth version
    /// component. GFE-only workarounds hang off the negation of this.
    #[must_use]
    pub fn is_sunshine(&self) -> bool {
        self.app_version
            .split('.')
            .nth(3)
            .is_some_and(|v| v.starts_with('-'))
    }
}

/// One entry from `/applist`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The id `/launch` takes.
    pub id: i64,
    /// The title to show in the chooser.
    pub title: String,
    /// Whether the host would stream this in HDR (a host-wide fact, despite living
    /// on the app element).
    pub hdr_supported: bool,
}

/// A successful `/launch` or `/resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchResponse {
    /// `sessionUrl0` — the RTSP endpoint, whose scheme (`rtsp://` vs `rtspenc://`)
    /// tells the streaming core whether RTSP is encrypted.
    pub session_url: String,
}

/// Parse `/serverinfo`.
///
/// # Errors
/// [`GameStreamError::Xml`] on malformed XML, [`GameStreamError::Nvhttp`] when the
/// host reported a failure.
pub fn parse_server_info(xml: &str) -> Result<ServerInfo, GameStreamError> {
    let doc = Document::parse(xml)?;
    doc.check_status()?;
    Ok(ServerInfo {
        hostname: doc.text("hostname").unwrap_or_default(),
        app_version: doc.text("appversion").unwrap_or_default(),
        gfe_version: doc.text("GfeVersion").unwrap_or_default(),
        https_port: doc
            .text("HttpsPort")
            .and_then(|v| v.parse().ok())
            .filter(|p| *p != 0)
            .unwrap_or(DEFAULT_HTTPS_PORT),
        paired: doc.text("PairStatus").as_deref() == Some("1"),
        // GFE 2.8+ leaves currentgame set to the last game played, so it is only
        // believed when the state says the host is busy.
        current_game: doc
            .text("state")
            .filter(|s| s.ends_with("_SERVER_BUSY"))
            .and(doc.text("currentgame"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        state: doc.text("state").unwrap_or_default(),
        codec_mode_support: doc
            .text("ServerCodecModeSupport")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
    })
}

/// Parse one `/pair` phase response, returning the named element's text.
///
/// The `paired` flag is returned alongside because phase 4 reports rejection with
/// `status_code=200` and `<paired>0</paired>` — checking the status alone would read
/// a refusal as success.
///
/// # Errors
/// [`GameStreamError::Xml`] / [`GameStreamError::Nvhttp`] as above.
pub fn parse_pair_phase(xml: &str, element: &str) -> Result<(bool, String), GameStreamError> {
    let doc = Document::parse(xml)?;
    doc.check_status()?;
    let paired = doc.text("paired").as_deref() == Some("1");
    Ok((paired, doc.text(element).unwrap_or_default()))
}

/// Parse `/applist`.
///
/// # Errors
/// [`GameStreamError::Xml`] / [`GameStreamError::Nvhttp`] as above.
pub fn parse_app_list(xml: &str) -> Result<Vec<App>, GameStreamError> {
    let doc = Document::parse(xml)?;
    doc.check_status()?;
    let mut apps = Vec::new();
    // Elements arrive flat in document order: each App's children follow it, so an
    // ID resets on the next AppTitle. Sunshine emits IsHdrSupported, AppTitle, ID.
    let mut title: Option<String> = None;
    let mut hdr = false;
    for (name, value) in &doc.elements {
        match name.as_str() {
            "IsHdrSupported" => hdr = value == "1",
            "AppTitle" => title = Some(value.clone()),
            "ID" => {
                if let (Some(t), Ok(id)) = (title.take(), value.parse::<i64>()) {
                    apps.push(App {
                        id,
                        title: t,
                        hdr_supported: hdr,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(apps)
}

/// Parse `/launch` or `/resume`.
///
/// # Errors
/// [`GameStreamError::Nvhttp`] carries the host's own explanation — "an app is
/// already running", "is a display connected", the 403 for mandatory encryption —
/// which is the message worth showing a person.
pub fn parse_launch(xml: &str) -> Result<LaunchResponse, GameStreamError> {
    let doc = Document::parse(xml)?;
    doc.check_status()?;
    let session_url = doc.text("sessionUrl0").ok_or_else(|| {
        GameStreamError::Xml("launch succeeded but carried no sessionUrl0".into())
    })?;
    Ok(LaunchResponse { session_url })
}

/// A flattened NVHTTP response: the root's status plus every leaf element's text, in
/// document order. The API is shallow and element names are unique enough that a full
/// tree buys nothing.
struct Document {
    status_code: i32,
    status_message: String,
    elements: Vec<(String, String)>,
}

impl Document {
    fn parse(xml: &str) -> Result<Self, GameStreamError> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut doc = Self {
            status_code: 0,
            status_message: String::new(),
            elements: Vec::new(),
        };
        let mut saw_root = false;
        let mut pending: Option<String> = None;

        loop {
            match reader
                .read_event()
                .map_err(|e| GameStreamError::Xml(e.to_string()))?
            {
                Event::Start(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "root" {
                        saw_root = true;
                        doc.read_root_attrs(&e)?;
                    }
                    pending = Some(name);
                }
                Event::Text(e) => {
                    if let Some(name) = pending.take() {
                        let text = e
                            .unescape()
                            .map_err(|e| GameStreamError::Xml(e.to_string()))?
                            .to_string();
                        doc.elements.push((name, text));
                    }
                }
                Event::End(_) => {
                    // An element that closed with no text still counts as present and
                    // empty — Sunshine emits `<AppTitle/>` for an unnamed app.
                    if let Some(name) = pending.take() {
                        if name != "root" {
                            doc.elements.push((name, String::new()));
                        }
                    }
                }
                Event::Empty(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    // A self-closing root is how Sunshine writes an error with no
                    // body — `<root status_code="401" .../>`. Missing its attributes
                    // would turn every rejection into "malformed XML" and hide the
                    // one thing the response was sent to say.
                    if name == "root" {
                        saw_root = true;
                        doc.read_root_attrs(&e)?;
                    } else {
                        doc.elements.push((name, String::new()));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }

        if !saw_root {
            // Sunshine's 404 handler writes its body twice, so an unparseable body is
            // the normal shape of "wrong transport for this endpoint".
            return Err(GameStreamError::Xml(
                "response had no <root> element — wrong port for this endpoint?".into(),
            ));
        }
        Ok(doc)
    }

    /// Read `status_code`/`status_message` off the `<root>` element.
    fn read_root_attrs(
        &mut self,
        e: &quick_xml::events::BytesStart<'_>,
    ) -> Result<(), GameStreamError> {
        for attr in e.attributes().flatten() {
            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
            let value = attr
                .unescape_value()
                .map_err(|e| GameStreamError::Xml(e.to_string()))?
                .to_string();
            match key.as_str() {
                // GFE 3.20.3 emits 0xFFFFFFFF here, so parse wide then narrow — a
                // wrapped -1 must not read as a success, and must not fail to parse
                // either (which would look like a malformed body).
                "status_code" => {
                    self.status_code = value
                        .parse::<i64>()
                        .map_or(-1, |v| i32::try_from(v).unwrap_or(-1));
                }
                "status_message" => self.status_message = value,
                _ => {}
            }
        }
        Ok(())
    }

    fn check_status(&self) -> Result<(), GameStreamError> {
        if self.status_code == 200 {
            return Ok(());
        }
        if self.status_code == 401 {
            return Err(GameStreamError::NotPaired {
                host: "this host".into(),
            });
        }
        Err(GameStreamError::Nvhttp {
            code: self.status_code,
            message: self.status_message.clone(),
        })
    }

    fn text(&self, name: &str) -> Option<String> {
        self.elements
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A real Sunshine `/serverinfo` body shape (docs §4.1 field order).
    const SUNSHINE_SERVERINFO: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
  <hostname>somepc</hostname>
  <appversion>7.1.431.-1</appversion>
  <GfeVersion>3.23.0.74</GfeVersion>
  <uniqueid>8b1f0e6c-3f2a-4c7d-9a11-2b6d5e4f7a90</uniqueid>
  <HttpsPort>47984</HttpsPort>
  <ExternalPort>47989</ExternalPort>
  <MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>
  <mac>00:00:00:00:00:00</mac>
  <LocalIP>10.0.0.7</LocalIP>
  <ServerCodecModeSupport>259</ServerCodecModeSupport>
  <PairStatus>1</PairStatus>
  <currentgame>0</currentgame>
  <state>SUNSHINE_SERVER_FREE</state>
</root>"#;

    #[test]
    fn parses_a_sunshine_serverinfo() {
        let info = parse_server_info(SUNSHINE_SERVERINFO).unwrap();
        assert_eq!(info.hostname, "somepc");
        assert_eq!(info.https_port, 47984);
        assert!(info.paired);
        assert_eq!(info.current_game, 0);
        assert_eq!(info.codec_mode_support, 259);
        assert!(
            info.is_sunshine(),
            "the -1 fourth version component is how a host says it is Sunshine"
        );
    }

    #[test]
    fn does_not_believe_currentgame_unless_the_host_says_it_is_busy() {
        // GFE 2.8+ leaves currentgame set to the last game played after it exits.
        // Believing it would send /resume to a host with nothing to resume.
        let xml = SUNSHINE_SERVERINFO.replace(
            "<currentgame>0</currentgame>",
            "<currentgame>881448767</currentgame>",
        );
        assert_eq!(parse_server_info(&xml).unwrap().current_game, 0);
        let busy = xml.replace("SUNSHINE_SERVER_FREE", "SUNSHINE_SERVER_BUSY");
        assert_eq!(parse_server_info(&busy).unwrap().current_game, 881_448_767);
    }

    #[test]
    fn an_unpaired_https_serverinfo_is_not_paired_rather_than_an_error() {
        let xml = SUNSHINE_SERVERINFO.replace("<PairStatus>1", "<PairStatus>0");
        assert!(!parse_server_info(&xml).unwrap().paired);
    }

    #[test]
    fn a_401_body_reports_not_paired() {
        let xml = r#"<root status_code="401" query="/applist" status_message="The client is not authorized. Certificate verification failed." />"#;
        match parse_app_list(xml) {
            Err(GameStreamError::NotPaired { .. }) => {}
            other => panic!("expected NotPaired, got {other:?}"),
        }
    }

    #[test]
    fn parses_an_applist_keeping_titles_with_ids() {
        let xml = r#"<root status_code="200">
  <App><IsHdrSupported>0</IsHdrSupported><AppTitle>Desktop</AppTitle><ID>1</ID></App>
  <App><IsHdrSupported>1</IsHdrSupported><AppTitle>Steam Big Picture</AppTitle><ID>881448767</ID></App>
</root>"#;
        let apps = parse_app_list(xml).unwrap();
        assert_eq!(
            apps,
            vec![
                App {
                    id: 1,
                    title: "Desktop".into(),
                    hdr_supported: false
                },
                App {
                    id: 881_448_767,
                    title: "Steam Big Picture".into(),
                    hdr_supported: true
                },
            ]
        );
    }

    #[test]
    fn an_unnamed_app_still_carries_its_id() {
        // Sunshine serializes an empty name as <AppTitle/>; dropping the app would
        // hide a launchable entry from the chooser.
        let xml = r#"<root status_code="200"><App><AppTitle/><ID>7</ID></App></root>"#;
        let apps = parse_app_list(xml).unwrap();
        assert_eq!(
            apps,
            vec![App {
                id: 7,
                title: String::new(),
                hdr_supported: false
            }]
        );
    }

    #[test]
    fn a_launch_failure_surfaces_the_hosts_own_message() {
        let xml = r#"<root status_code="503" status_message="Failed to initialize video capture/encoding. Is a display connected and turned on?"><gamesession>0</gamesession></root>"#;
        match parse_launch(xml) {
            Err(GameStreamError::Nvhttp { code, message }) => {
                assert_eq!(code, 503);
                assert!(message.contains("display connected"));
            }
            other => panic!("expected an Nvhttp error, got {other:?}"),
        }
    }

    #[test]
    fn parses_an_encrypted_rtsp_session_url() {
        let xml = r#"<root status_code="200"><sessionUrl0>rtspenc://10.0.0.7:48010</sessionUrl0><gamesession>1</gamesession></root>"#;
        assert_eq!(
            parse_launch(xml).unwrap().session_url,
            "rtspenc://10.0.0.7:48010"
        );
    }

    #[test]
    fn a_bodyless_response_names_the_likely_cause() {
        match parse_server_info("not xml at all") {
            Err(GameStreamError::Xml(msg)) => assert!(msg.contains("no <root>")),
            other => panic!("expected an Xml error, got {other:?}"),
        }
    }

    #[test]
    fn builds_a_launch_query_in_moonlights_shape() {
        let b = RequestBuilder::new(UniqueId::new("0123456789abcdef"));
        let req = b.launch(
            &LaunchParams {
                app_id: 881_448_767,
                resume: false,
                width: 3840,
                height: 2160,
                fps: 60,
                optimize_settings: false,
                play_audio_on_host: false,
                surround_audio_info: 196_610,
                ri_key: [0xab; 16],
                // Negative on purpose: the id is the first four IV bytes read as a
                // signed big-endian int, and half of all keys produce one.
                ri_key_id: -559_038_737,
            },
            "deadbeef",
        );
        assert_eq!(req.transport, Transport::Tls);
        assert_eq!(
            req.path_and_query,
            "/launch?uniqueid=0123456789abcdef&uuid=deadbeef\
             &appid=881448767&mode=3840x2160x60&additionalStates=1&sops=0\
             &rikey=abababababababababababababababab&rikeyid=-559038737\
             &localAudioPlayMode=0&surroundAudioInfo=196610\
             &remoteControllersBitmap=0&gcmap=0&gcpersist=0&corever=1"
        );
    }

    #[test]
    fn a_running_app_switches_launch_to_resume() {
        let b = RequestBuilder::new(UniqueId::new("aa"));
        let params = LaunchParams {
            app_id: 1,
            resume: true,
            width: 1920,
            height: 1080,
            fps: 60,
            optimize_settings: true,
            play_audio_on_host: true,
            surround_audio_info: 196_610,
            ri_key: [0; 16],
            ri_key_id: 1,
        };
        let req = b.launch(&params, "u");
        assert!(req.path_and_query.starts_with("/resume?"));
        assert!(req.path_and_query.contains("&sops=1"));
        assert!(req.path_and_query.contains("&localAudioPlayMode=1"));
    }

    #[test]
    fn builds_a_pairing_phase_query() {
        let b = RequestBuilder::new(UniqueId::new("ff00"));
        let phase = PhaseRequest {
            param: ("clientcert", "30820".into()),
            phrase: Some("getservercert"),
            extra: vec![("salt", "aabb".into())],
        };
        let req = b.pair(&phase, Transport::Plain, "uu");
        assert_eq!(req.transport, Transport::Plain);
        assert_eq!(
            req.path_and_query,
            "/pair?uniqueid=ff00&uuid=uu&devicename=roth&updateState=1\
             &phrase=getservercert&salt=aabb&clientcert=30820"
        );
    }

    #[test]
    fn phase_four_rejection_is_visible_despite_a_200() {
        // The failure this catches: Sunshine says status_code=200 and paired=0 when
        // the client's proof did not check out. Reading the status alone calls that
        // success and then every later request 401s for no visible reason.
        let xml = r#"<root status_code="200"><paired>0</paired></root>"#;
        let (paired, _) = parse_pair_phase(xml, "paired").unwrap();
        assert!(!paired);
    }

    #[test]
    fn generated_unique_ids_look_like_moonlights() {
        let id = UniqueId::generate();
        assert!(!id.as_str().is_empty() && id.as_str().len() <= 16);
        assert!(id.as_str().chars().all(|c| c.is_ascii_hexdigit()));
        assert!(id.as_str().chars().all(|c| !c.is_ascii_uppercase()));
    }
}
