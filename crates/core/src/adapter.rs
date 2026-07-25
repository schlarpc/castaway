//! The [`SourceAdapter`] trait and the plumbing that carries [`SessionEvent`]s from
//! an adapter to the session manager.

use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::error::CoreError;
use crate::event::{Advertisement, SessionEvent};
use crate::types::ProtocolKind;

/// Identifies one running source: which protocol, plus a per-protocol instance tag
/// (a sender may connect twice; the tag disambiguates).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceId {
    /// The protocol family.
    pub kind: ProtocolKind,
    /// A per-adapter instance discriminator (connection id, sender name, …).
    pub instance: Arc<str>,
}

impl SourceId {
    /// Construct a source id for a protocol and instance tag.
    pub fn new(kind: ProtocolKind, instance: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            instance: instance.into(),
        }
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.kind, self.instance)
    }
}

/// A `SessionEvent` tagged with the source that produced it.
#[derive(Debug)]
pub struct SourceMessage {
    /// Which source emitted this event.
    pub source: SourceId,
    /// The event itself.
    pub event: SessionEvent,
}

/// The write end an adapter uses to emit events to the session manager. Cloneable and
/// pre-tagged with the adapter's [`SourceId`] so adapters can't spoof another source.
#[derive(Debug, Clone)]
pub struct SessionSink {
    source: SourceId,
    tx: mpsc::Sender<SourceMessage>,
}

impl SessionSink {
    /// Create a sink bound to `source` that forwards to `tx`.
    #[must_use]
    pub fn new(source: SourceId, tx: mpsc::Sender<SourceMessage>) -> Self {
        Self { source, tx }
    }

    /// The source this sink is bound to.
    #[must_use]
    pub fn source(&self) -> &SourceId {
        &self.source
    }

    /// A sink onto the same channel with a different instance tag — one per accepted
    /// connection, so a listening adapter's two senders arrive as distinct sources.
    /// The [`ProtocolKind`] is carried over, so this still can't spoof another protocol.
    #[must_use]
    pub fn with_instance(&self, instance: impl Into<Arc<str>>) -> Self {
        Self {
            source: SourceId::new(self.source.kind, instance),
            tx: self.tx.clone(),
        }
    }

    /// Emit an event to the session manager.
    ///
    /// # Errors
    /// Returns [`CoreError::ChannelClosed`] if the session manager has shut down.
    pub async fn emit(&self, event: SessionEvent) -> Result<(), CoreError> {
        self.tx
            .send(SourceMessage {
                source: self.source.clone(),
                event,
            })
            .await
            .map_err(|_| CoreError::ChannelClosed)
    }
}

/// A network source of media: one protocol adapter. Runs its whole lifecycle as an
/// async actor and emits [`SessionEvent`]s; it never touches the GPU or the pipeline
/// directly (ground rules 3 & 4).
#[async_trait::async_trait]
pub trait SourceAdapter: Send + Sync {
    /// The protocol this adapter implements.
    fn kind(&self) -> ProtocolKind;

    /// What this adapter needs advertised for senders to discover it.
    fn advertisements(&self) -> Vec<Advertisement>;

    /// Run the adapter's whole lifecycle, emitting events through `sink`. Returns when
    /// the adapter shuts down (listener closed) or errors.
    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError>;
}

/// Miracast is the one genuinely per-OS adapter. It does not share the IP substrate,
/// so it sits behind its own backend trait: Linux yields encoded frames, Windows
/// `MiracastReceiver` yields decoded frames (ground rule 5, architecture §3).
#[async_trait::async_trait]
pub trait MiracastBackend: Send + Sync {
    /// Acquire a P2P Group-Owner interface, run the WFD/RTSP session, and emit frames
    /// through `sink` as a [`SessionEvent::Mirror`].
    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::event::SessionEvent;

    #[tokio::test]
    async fn sink_tags_events_with_its_source() {
        let (tx, mut rx) = mpsc::channel(4);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "conn-1"), tx);
        sink.emit(SessionEvent::End).await.unwrap();
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.source.kind, ProtocolKind::Dlna);
        assert_eq!(&*msg.source.instance, "conn-1");
    }

    #[tokio::test]
    async fn retagging_keeps_the_protocol_but_changes_the_instance() {
        let (tx, mut rx) = mpsc::channel(4);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Cast, "listener"), tx);
        let conn = sink.with_instance("10.0.0.7:41234");
        conn.emit(SessionEvent::End).await.unwrap();
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.source.kind, ProtocolKind::Cast);
        assert_eq!(&*msg.source.instance, "10.0.0.7:41234");
    }

    #[tokio::test]
    async fn emit_after_close_reports_channel_closed() {
        let (tx, rx) = mpsc::channel(1);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Cast, "x"), tx);
        drop(rx);
        assert!(matches!(
            sink.emit(SessionEvent::End).await,
            Err(CoreError::ChannelClosed)
        ));
    }
}
