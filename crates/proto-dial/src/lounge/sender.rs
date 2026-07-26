//! The *sender* half of the Lounge bind channel: what a remote control sends.
//!
//! The parser next door reads what a screen pushes; this builds what a controller pushes
//! back. We need it because the receiver drives its own screen — to skip a sponsor it
//! attaches as a second remote, exactly as a phone does, and issues `seekTo`.
//!
//! Pure and sans-I/O (ground rule 3): every method returns the query string and body for
//! a request someone else performs, so the whole `RID`/`AID`/`ofs` sequencing — the part
//! that silently breaks a channel when it drifts — is testable with no socket.
//!
//! The typestate is the point: [`Unbound`] can only produce a bind request, and the only
//! way to reach [`Bound`] (which can send commands) is through a bind *response* that
//! actually carried a session. A command before the handshake does not compile.

use url::form_urlencoded;

use crate::error::DialError;
use crate::lounge::{parse_chunks, LoungeCommand};

/// One HTTP request for the actor to perform: a query string, and a form body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoungeRequest {
    /// Query string, without the leading `?`.
    pub query: String,
    /// `application/x-www-form-urlencoded` body. Empty for the receive channel, which is
    /// a GET.
    pub body: String,
}

/// Who we say we are. The Lounge shows this name to other controllers on the screen.
#[derive(Debug, Clone)]
pub struct SenderIdentity {
    /// A stable per-device uuid.
    pub device_id: String,
    /// The display name.
    pub name: String,
}

/// A sender that has a token but no session yet.
#[derive(Debug, Clone)]
pub struct Unbound {
    token: String,
    identity: SenderIdentity,
    rid: u32,
}

/// A bound session: the ids the channel is keyed on, and the counters it advances.
#[derive(Debug, Clone)]
pub struct Bound {
    token: String,
    identity: SenderIdentity,
    rid: u32,
    sid: String,
    gsession: String,
    /// Highest array id seen from the screen; the channel resumes from it.
    aid: i64,
    /// Command offset, which the server uses to detect a replay.
    ofs: u32,
}

impl Unbound {
    /// A sender identified by `identity`, authenticated by a lounge token.
    ///
    /// `rid_seed` starts the request-id sequence. It is a parameter rather than a random
    /// number because this crate stays deterministic (ground rule 3) — the actor picks it.
    #[must_use]
    pub fn new(token: impl Into<String>, identity: SenderIdentity, rid_seed: u32) -> Self {
        Self {
            token: token.into(),
            identity,
            rid: rid_seed,
        }
    }

    /// The request that opens the channel.
    pub fn bind_request(&mut self) -> LoungeRequest {
        self.rid = self.rid.wrapping_add(1);
        LoungeRequest {
            query: base_params(&self.token, &self.identity)
                .append_pair("RID", &self.rid.to_string())
                .finish(),
            body: form_urlencoded::Serializer::new(String::new())
                .append_pair("count", "0")
                .finish(),
        }
    }

    /// Read the bind response and become a session.
    ///
    /// # Errors
    /// [`DialError::MalformedChunk`] if the body is not bind-channel framing, or
    /// [`DialError::MissingField`] if it carried no `SID`/`gsessionid` — which is what a
    /// rejected or expired token looks like.
    pub fn bound(self, response: &str) -> Result<Bound, DialError> {
        let mut sid = None;
        let mut gsession = None;
        let mut aid = 0;
        for command in parse_chunks(response)? {
            aid = aid.max(command.aid);
            match (command.name.as_str(), command.payload.as_str()) {
                ("c", Some(value)) => sid = Some(value.to_string()),
                ("S", Some(value)) => gsession = Some(value.to_string()),
                _ => {}
            }
        }
        Ok(Bound {
            token: self.token,
            identity: self.identity,
            rid: self.rid,
            sid: sid.ok_or(DialError::MissingField("SID"))?,
            gsession: gsession.ok_or(DialError::MissingField("gsessionid"))?,
            aid,
            ofs: 0,
        })
    }
}

impl Bound {
    /// The long-poll that streams what the screen pushes. Perform as a GET.
    #[must_use]
    pub fn receive_request(&self) -> LoungeRequest {
        LoungeRequest {
            query: base_params(&self.token, &self.identity)
                .append_pair("RID", "rpc")
                .append_pair("SID", &self.sid)
                .append_pair("gsessionid", &self.gsession)
                .append_pair("AID", &self.aid.to_string())
                .append_pair("CI", "0")
                .append_pair("TYPE", "xmlhttp")
                .finish(),
            body: String::new(),
        }
    }

    /// A command to the screen, e.g. `seekTo` with `newTime`.
    pub fn command_request(&mut self, command: &str, args: &[(&str, &str)]) -> LoungeRequest {
        self.rid = self.rid.wrapping_add(1);
        let mut body = form_urlencoded::Serializer::new(String::new());
        body.append_pair("count", "1")
            .append_pair("ofs", &self.ofs.to_string())
            .append_pair("req0__sc", command);
        for (key, value) in args {
            body.append_pair(&format!("req0_{key}"), value);
        }
        self.ofs = self.ofs.wrapping_add(1);
        LoungeRequest {
            query: base_params(&self.token, &self.identity)
                .append_pair("RID", &self.rid.to_string())
                .append_pair("SID", &self.sid)
                .append_pair("gsessionid", &self.gsession)
                .append_pair("AID", &self.aid.to_string())
                .finish(),
            body: body.finish(),
        }
    }

    /// Seek to a position, in seconds.
    pub fn seek_to(&mut self, seconds: f64) -> LoungeRequest {
        self.command_request("seekTo", &[("newTime", &format!("{seconds:.3}"))])
    }

    /// Note an array id from the screen so a reconnect resumes rather than replays.
    pub fn observe(&mut self, command: &LoungeCommand) {
        self.aid = self.aid.max(command.aid);
    }

    /// The highest array id seen.
    #[must_use]
    pub fn aid(&self) -> i64 {
        self.aid
    }
}

/// The parameters every request on this channel carries.
fn base_params(
    token: &str,
    identity: &SenderIdentity,
) -> form_urlencoded::Serializer<'static, String> {
    let mut params = form_urlencoded::Serializer::new(String::new());
    params
        .append_pair("device", "REMOTE_CONTROL")
        .append_pair("id", &identity.device_id)
        .append_pair("name", &identity.name)
        .append_pair("app", "castaway")
        .append_pair("mdx-version", "3")
        .append_pair("loungeIdToken", token)
        .append_pair("VER", "8")
        .append_pair("v", "2")
        .append_pair("t", "1")
        .append_pair("CVER", "1");
    params
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn identity() -> SenderIdentity {
        SenderIdentity {
            device_id: "device-uuid".into(),
            name: "castaway sponsorblock".into(),
        }
    }

    /// A real bind response: the session ids arrive as commands in the framing.
    fn bind_response() -> String {
        let json = r#"[[0,["c","SID-VALUE","",8]],[1,["S","GSESSION-VALUE"]]]"#;
        format!("{}\n{}", json.chars().count(), json)
    }

    #[test]
    fn a_bind_response_without_a_session_is_an_error_not_a_sender() {
        // What an expired or rejected token looks like. Continuing from here would send
        // commands into a channel that does not exist.
        let json = r#"[[0,["noop"]]]"#;
        let body = format!("{}\n{}", json.chars().count(), json);
        let unbound = Unbound::new("token", identity(), 100);
        assert!(matches!(
            unbound.bound(&body),
            Err(DialError::MissingField("SID"))
        ));
    }

    #[test]
    fn binding_carries_the_identity_and_the_token() {
        let mut unbound = Unbound::new("TOKEN-VALUE", identity(), 100);
        let request = unbound.bind_request();
        assert!(request.query.contains("device=REMOTE_CONTROL"));
        assert!(request.query.contains("loungeIdToken=TOKEN-VALUE"));
        assert!(request.query.contains("RID=101"), "{}", request.query);
        // Spaces in the name must be encoded, or the query breaks at the first one.
        assert!(request.query.contains("name=castaway+sponsorblock"));
        assert_eq!(request.body, "count=0");
    }

    #[test]
    fn commands_advance_rid_and_ofs_and_carry_the_session() {
        let mut sender = Unbound::new("token", identity(), 100)
            .bind_request_then_bind(&bind_response())
            .unwrap();

        let first = sender.seek_to(42.5);
        assert!(first.query.contains("SID=SID-VALUE"));
        assert!(first.query.contains("gsessionid=GSESSION-VALUE"));
        assert!(first.body.contains("req0__sc=seekTo"));
        assert!(first.body.contains("req0_newTime=42.500"));
        assert!(first.body.contains("ofs=0"));

        let second = sender.seek_to(90.0);
        // Both counters have to move: a repeated ofs reads as a replay, and a repeated
        // RID reads as a retry of the request before it.
        assert!(second.body.contains("ofs=1"), "{}", second.body);
        let rid_of = |q: &str| {
            q.split('&')
                .find_map(|p| p.strip_prefix("RID="))
                .unwrap()
                .to_string()
        };
        assert_ne!(rid_of(&first.query), rid_of(&second.query));
    }

    #[test]
    fn the_receive_channel_resumes_from_the_last_array_id() {
        let mut sender = Unbound::new("token", identity(), 100)
            .bind_request_then_bind(&bind_response())
            .unwrap();
        assert!(sender.receive_request().query.contains("AID=1"));

        sender.observe(&LoungeCommand {
            aid: 57,
            name: "onStateChange".into(),
            payload: serde_json::Value::Null,
        });
        let request = sender.receive_request();
        assert!(request.query.contains("AID=57"), "{}", request.query);
        assert!(request.query.contains("TYPE=xmlhttp"));
        assert!(
            request.body.is_empty(),
            "the receive channel is a GET; a body would make it a command"
        );
    }

    /// Test helper: the two steps a caller always does together.
    impl Unbound {
        fn bind_request_then_bind(mut self, response: &str) -> Result<Bound, DialError> {
            let _ = self.bind_request();
            self.bound(response)
        }
    }
}
