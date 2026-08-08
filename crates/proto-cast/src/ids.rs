//! Role-named ids for the Cast session layer (#214).
//!
//! The CASTv2 envelope's `source_id`/`destination_id` stay raw `String`s — they are
//! wire routing, and a transport id, a sender id, and `receiver-0` all legitimately
//! appear in either slot. These types begin where the session *names roles*: which
//! sender launched an app, which session id the page echoes to its cloud, which
//! transport id media messages address. Handing one where another belongs is a V2
//! message routed to a destination that ignores it — a silent misroute — so it must
//! not typecheck. Each holds its string exactly as received; nothing is normalised.

use serde::{Deserialize, Serialize};

macro_rules! id_string {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap an id, stored exactly as given.
            #[must_use]
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// The id as it appears on the wire.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(id.to_owned())
            }
        }
    };
}

id_string!(
    /// An application id (`appId`): what a sender asks about and launches.
    AppId
);
id_string!(
    /// A session id (`sessionId`): minted at `LAUNCH`, reported in `RECEIVER_STATUS`,
    /// and echoed by a hosted page to its vendor's cloud.
    SessionId
);
id_string!(
    /// A transport id (`transportId`): the virtual-connection id media messages
    /// address once an application is running.
    TransportId
);
id_string!(
    /// A sender id: the peer end of a virtual connection (`sender-0`, a page's SDK
    /// client, …). `*` addresses every sender where a broadcast is meant.
    SenderId
);

impl AppId {
    /// Case-insensitive comparison with a known id. App ids are hex tokens senders
    /// spell in either case, so equality against the catalogue is never case-sensitive.
    #[must_use]
    pub fn eq_ignore_ascii_case(&self, other: &str) -> bool {
        self.0.eq_ignore_ascii_case(other)
    }
}
