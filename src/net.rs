//! The relay connection.
//!
//! One task owns the WebSocket for the whole process. It speaks NIP-01 frames,
//! completes the NIP-42 challenge the relay issues on connect, and republishes
//! every live subscription after a reconnect so the interface never has to know
//! the socket dropped. Callers interact with it through two channels: commands
//! in, events out.
//!
//! Publishing is deliberately queued rather than rejected while offline. A chat
//! client that loses a message because a socket blinked is a broken chat client.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use nostr::event::{Event, EventId, FinalizeEvent};
use nostr::filter::Filter;
use nostr::key::Keys;
use nostr::message::{ClientMessage, RelayMessage, SubscriptionId};
use nostr::nips::nip42::ClientAuthentication;
use nostr::types::RelayUrl;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, warn};

/// Buzz relays cap frames at 64 KiB; refuse to build anything larger locally
/// rather than having the connection torn down for a protocol violation.
const MAX_FRAME: usize = 64 * 1024;

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Where the connection is in its lifecycle, as shown in the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Offline,
    Connecting,
    /// Socket is open; the NIP-42 challenge has not yet been answered.
    Authenticating,
    /// Authenticated and carrying subscriptions.
    Ready,
    /// Terminal for this attempt; the task is already backing off to retry.
    Failed(String),
}

impl ConnState {
    pub fn is_ready(&self) -> bool {
        matches!(self, ConnState::Ready)
    }
}

/// Instructions from the interface to the connection.
#[derive(Debug)]
pub enum Command {
    /// Sign-and-send is done by the caller so that the optimistic local echo
    /// and the wire event are guaranteed to carry the same id.
    Publish(Box<Event>),
    /// Registers or replaces a subscription. Replacing reuses the id, which the
    /// relay treats as an implicit close-and-reopen.
    Subscribe {
        id: String,
        filters: Vec<Filter>,
        /// One-shot subscriptions are closed by the task as soon as the relay
        /// reports end-of-stored-events, which is what NIP-50 search wants.
        oneshot: bool,
    },
    Unsubscribe(String),
    /// Points the connection at a different relay, dropping all subscriptions.
    Switch(String),
    Shutdown,
}

/// Everything the connection tells the interface.
#[derive(Debug)]
pub enum Update {
    State(ConnState),
    Event {
        subscription: String,
        event: Box<Event>,
    },
    EndOfStored(String),
    /// A verdict on something we published.
    Verdict {
        id: String,
        accepted: bool,
        message: String,
    },
    Closed {
        subscription: String,
        message: String,
    },
    Notice(String),
}

/// Handle held by the interface.
#[derive(Clone)]
pub struct Relay {
    tx: UnboundedSender<Command>,
}

impl Relay {
    pub fn send(&self, command: Command) {
        // A closed channel means the connection task is gone, which only
        // happens during shutdown; dropping the command is correct there.
        let _ = self.tx.send(command);
    }

    pub fn publish(&self, event: Event) {
        self.send(Command::Publish(Box::new(event)));
    }

    pub fn subscribe(&self, id: impl Into<String>, filters: Vec<Filter>) {
        self.send(Command::Subscribe {
            id: id.into(),
            filters,
            oneshot: false,
        });
    }

    pub fn query(&self, id: impl Into<String>, filters: Vec<Filter>) {
        self.send(Command::Subscribe {
            id: id.into(),
            filters,
            oneshot: true,
        });
    }

    pub fn unsubscribe(&self, id: impl Into<String>) {
        self.send(Command::Unsubscribe(id.into()));
    }
}

/// Starts the connection task and returns the handle used to drive it.
pub fn spawn(relay_url: String, keys: Keys) -> (Relay, UnboundedReceiver<Update>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (update_tx, update_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        Connection::new(relay_url, keys, cmd_rx, update_tx)
            .run()
            .await;
    });
    (Relay { tx: cmd_tx }, update_rx)
}

/// Subscriptions and pending publishes survive across reconnects; the socket
/// itself does not, so it is created fresh inside each session.
struct Connection {
    url: String,
    keys: Keys,
    commands: UnboundedReceiver<Command>,
    updates: UnboundedSender<Update>,
    subscriptions: HashMap<String, (Vec<Filter>, bool)>,
    /// Signed events awaiting a successful write. An entry is removed only
    /// after the socket has accepted it, so a write that fails mid-queue leaves
    /// the event and everything behind it to be retried on the next session.
    outbox: VecDeque<Event>,
    backoff: Duration,
}

/// Why a session ended, which decides whether we retry and how loudly.
enum Exit {
    /// The socket dropped or the relay misbehaved; reconnect after a backoff.
    Retry(String),
    /// The interface asked us to stop.
    Shutdown,
    /// The interface pointed us at a different relay.
    Switch(String),
}

impl Connection {
    fn new(
        url: String,
        keys: Keys,
        commands: UnboundedReceiver<Command>,
        updates: UnboundedSender<Update>,
    ) -> Self {
        Self {
            url,
            keys,
            commands,
            updates,
            subscriptions: HashMap::new(),
            outbox: VecDeque::new(),
            backoff: BACKOFF_MIN,
        }
    }

    async fn run(mut self) {
        loop {
            self.emit(Update::State(ConnState::Connecting));
            match self.session().await {
                Exit::Shutdown => {
                    self.emit(Update::State(ConnState::Offline));
                    return;
                }
                Exit::Switch(url) => {
                    self.url = url;
                    self.subscriptions.clear();
                    self.backoff = BACKOFF_MIN;
                }
                Exit::Retry(reason) => {
                    warn!(relay = %self.url, %reason, "relay session ended");
                    self.emit(Update::State(ConnState::Failed(reason)));
                    // Sleeping here rather than at the top of the loop keeps the
                    // first connection attempt immediate.
                    if !self.wait_backoff().await {
                        self.emit(Update::State(ConnState::Offline));
                        return;
                    }
                }
            }
        }
    }

    /// Sleeps out the backoff while staying responsive to commands. Returns
    /// false when the interface asked us to shut down while waiting.
    async fn wait_backoff(&mut self) -> bool {
        let delay = self.backoff;
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        let deadline = tokio::time::sleep(delay);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return true,
                command = self.commands.recv() => match command {
                    Some(Command::Shutdown) | None => return false,
                    // Reconnect immediately when redirected; the user is waiting.
                    Some(Command::Switch(url)) => {
                        self.url = url;
                        self.subscriptions.clear();
                        self.backoff = BACKOFF_MIN;
                        return true;
                    }
                    Some(other) => self.stage(other),
                },
            }
        }
    }

    /// Records a command that arrived while the socket was down.
    fn stage(&mut self, command: Command) {
        match command {
            Command::Publish(event) => self.outbox.push_back(*event),
            Command::Subscribe {
                id,
                filters,
                oneshot,
            } => {
                self.subscriptions.insert(id, (filters, oneshot));
            }
            Command::Unsubscribe(id) => {
                self.subscriptions.remove(&id);
            }
            Command::Switch(_) | Command::Shutdown => {}
        }
    }

    async fn session(&mut self) -> Exit {
        let config = WebSocketConfig::default()
            .max_frame_size(Some(MAX_FRAME))
            .max_message_size(Some(MAX_FRAME));

        let socket = match tokio_tungstenite::connect_async_with_config(
            self.url.as_str(),
            Some(config),
            false,
        )
        .await
        {
            Ok((socket, _response)) => socket,
            Err(err) => return Exit::Retry(format!("connect: {err}")),
        };

        self.emit(Update::State(ConnState::Authenticating));
        let (mut sink, mut stream) = socket.split();

        // The relay issues its challenge unprompted, so a session begins by
        // waiting for it rather than by sending anything.
        let mut authenticated = false;
        let mut auth_event: Option<EventId> = None;

        loop {
            tokio::select! {
                frame = stream.next() => {
                    let Some(frame) = frame else {
                        return Exit::Retry("relay closed the connection".into());
                    };
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(err) => return Exit::Retry(format!("read: {err}")),
                    };

                    let text = match frame {
                        WsMessage::Text(text) => text,
                        WsMessage::Binary(_) => continue,
                        WsMessage::Ping(payload) => {
                            // The relay drops clients after three missed pongs.
                            if sink.send(WsMessage::Pong(payload)).await.is_err() {
                                return Exit::Retry("write: pong failed".into());
                            }
                            continue;
                        }
                        WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
                        WsMessage::Close(frame) => {
                            let reason = frame
                                .map(|f| format!("relay closed: {} {}", f.code, f.reason))
                                .unwrap_or_else(|| "relay closed".to_string());
                            return Exit::Retry(reason);
                        }
                    };

                    let message = match RelayMessage::from_json(text.as_bytes()) {
                        Ok(message) => message,
                        Err(err) => {
                            debug!(%err, "unparsable relay frame");
                            continue;
                        }
                    };

                    match message {
                        RelayMessage::Auth { challenge } => {
                            let event = match self.sign_auth(&challenge) {
                                Ok(event) => event,
                                Err(err) => return Exit::Retry(format!("auth: {err}")),
                            };
                            auth_event = Some(event.id);
                            let frame = ClientMessage::auth(event).as_json();
                            if sink.send(WsMessage::text(frame)).await.is_err() {
                                return Exit::Retry("write: auth failed".into());
                            }
                        }
                        RelayMessage::Ok { event_id, status, message } => {
                            if Some(event_id) == auth_event {
                                if !status {
                                    return Exit::Retry(format!("auth rejected: {message}"));
                                }
                                authenticated = true;
                                self.backoff = BACKOFF_MIN;
                                self.emit(Update::State(ConnState::Ready));
                                if let Err(reason) = self.replay(&mut sink).await {
                                    return Exit::Retry(reason);
                                }
                                continue;
                            }
                            self.emit(Update::Verdict {
                                id: event_id.to_hex(),
                                accepted: status,
                                message: message.into_owned(),
                            });
                        }
                        RelayMessage::Event { subscription_id, event } => {
                            let event = event.into_owned();
                            // The relay is not the only party that can put bytes
                            // on this socket, so verify rather than assume.
                            if event.verify().is_err() {
                                warn!(id = %event.id, "discarding event with a bad signature");
                                continue;
                            }
                            self.emit(Update::Event {
                                subscription: subscription_id.to_string(),
                                event: Box::new(event),
                            });
                        }
                        RelayMessage::EndOfStoredEvents(subscription_id) => {
                            let id = subscription_id.to_string();
                            if self.subscriptions.get(&id).is_some_and(|(_, one)| *one) {
                                self.subscriptions.remove(&id);
                                let frame = ClientMessage::close(SubscriptionId::new(id.clone()));
                                if sink.send(WsMessage::text(frame.as_json())).await.is_err() {
                                    return Exit::Retry("write: close failed".into());
                                }
                            }
                            self.emit(Update::EndOfStored(id));
                        }
                        RelayMessage::Closed { subscription_id, message } => {
                            let id = subscription_id.to_string();
                            self.subscriptions.remove(&id);
                            self.emit(Update::Closed {
                                subscription: id,
                                message: message.into_owned(),
                            });
                        }
                        RelayMessage::Notice(message) => {
                            self.emit(Update::Notice(message.into_owned()));
                        }
                        RelayMessage::Count { .. }
                        | RelayMessage::NegMsg { .. }
                        | RelayMessage::NegErr { .. } => {}
                    }
                }

                command = self.commands.recv() => {
                    let Some(command) = command else { return Exit::Shutdown };
                    match command {
                        Command::Shutdown => {
                            let _ = sink.send(WsMessage::Close(None)).await;
                            return Exit::Shutdown;
                        }
                        Command::Switch(url) => {
                            let _ = sink.send(WsMessage::Close(None)).await;
                            return Exit::Switch(url);
                        }
                        Command::Publish(event) => {
                            // Queue unconditionally, then try to drain. The event
                            // is only dropped from the queue once the socket has
                            // taken it, so a blink costs a retry, not a message.
                            self.outbox.push_back(*event);
                            if authenticated
                                && let Err(reason) = self.flush(&mut sink).await {
                                    return Exit::Retry(reason);
                                }
                        }
                        Command::Subscribe { id, filters, oneshot } => {
                            self.subscriptions.insert(id.clone(), (filters.clone(), oneshot));
                            if !authenticated {
                                continue;
                            }
                            let frame = ClientMessage::req(SubscriptionId::new(id), filters);
                            if sink.send(WsMessage::text(frame.as_json())).await.is_err() {
                                return Exit::Retry("write: subscribe failed".into());
                            }
                        }
                        Command::Unsubscribe(id) => {
                            self.subscriptions.remove(&id);
                            if !authenticated {
                                continue;
                            }
                            let frame = ClientMessage::close(SubscriptionId::new(id));
                            if sink.send(WsMessage::text(frame.as_json())).await.is_err() {
                                return Exit::Retry("write: unsubscribe failed".into());
                            }
                        }
                    }
                }
            }
        }
    }

    /// Re-establishes every subscription and drains the offline outbox.
    async fn replay<S>(&mut self, sink: &mut S) -> Result<(), String>
    where
        S: SinkExt<WsMessage> + Unpin,
    {
        for (id, (filters, _)) in &self.subscriptions {
            let frame = ClientMessage::req(SubscriptionId::new(id.clone()), filters.clone());
            sink.send(WsMessage::text(frame.as_json()))
                .await
                .map_err(|_| "write: resubscribe failed".to_string())?;
        }
        self.flush(sink).await
    }

    /// Writes queued events in order, removing each only once the socket has
    /// accepted it. Returns on the first failure with the remainder intact.
    async fn flush<S>(&mut self, sink: &mut S) -> Result<(), String>
    where
        S: SinkExt<WsMessage> + Unpin,
    {
        while let Some(event) = self.outbox.front() {
            // Borrowing avoids cloning an event we may have to keep.
            let frame = ClientMessage::Event(Cow::Borrowed(event)).as_json();
            sink.send(WsMessage::text(frame))
                .await
                .map_err(|_| "write: publish failed".to_string())?;
            self.outbox.pop_front();
        }
        Ok(())
    }

    fn sign_auth(&self, challenge: &str) -> Result<Event> {
        let url = RelayUrl::parse(&self.url)
            .with_context(|| format!("{} is not a ws:// or wss:// URL", self.url))?;
        let event = ClientAuthentication::new(challenge, url)
            .finalize(&self.keys)
            .context("signing the NIP-42 challenge")?;
        Ok(event)
    }

    fn emit(&self, update: Update) {
        let _ = self.updates.send(update);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::event::Kind;
    use nostr::filter::SingleLetterTag;

    #[test]
    fn auth_events_carry_the_challenge_and_relay() {
        let keys = Keys::generate();
        let (_tx, rx) = mpsc::unbounded_channel();
        let (utx, _urx) = mpsc::unbounded_channel();
        let conn = Connection::new("ws://localhost:3000".into(), keys, rx, utx);

        let event = conn.sign_auth("abc123").unwrap();
        assert_eq!(event.kind.as_u16(), 22242);
        assert_eq!(crate::proto::tag_value(&event, "challenge"), Some("abc123"));
        assert!(
            crate::proto::tag_value(&event, "relay")
                .unwrap()
                .starts_with("ws://localhost:3000")
        );
        event.verify().expect("auth event must be self-consistent");
    }

    #[test]
    fn auth_rejects_non_websocket_urls() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let (utx, _urx) = mpsc::unbounded_channel();
        let conn = Connection::new("http://localhost:3000".into(), Keys::generate(), rx, utx);
        assert!(conn.sign_auth("abc").is_err());
    }

    /// A sink that accepts a fixed number of writes and then fails forever,
    /// standing in for a socket that drops mid-flush.
    struct FlakySink {
        remaining: usize,
        written: Vec<String>,
    }

    impl futures_util::Sink<WsMessage> for FlakySink {
        type Error = std::io::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(
            mut self: std::pin::Pin<&mut Self>,
            item: WsMessage,
        ) -> Result<(), Self::Error> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("socket closed"));
            }
            self.remaining -= 1;
            self.written
                .push(item.to_text().unwrap_or_default().to_string());
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn chat(keys: &Keys, body: &str) -> Event {
        use nostr::event::{EventBuilder, Tag};
        EventBuilder::new(Kind::from_u16(9), body)
            .tag(Tag::parse(["h", "room"]).unwrap())
            .finalize(keys)
            .unwrap()
    }

    /// The module promises that a socket blink costs a retry, not a message.
    #[tokio::test]
    async fn a_write_failure_keeps_the_unsent_events_queued() {
        let keys = Keys::generate();
        let (_tx, rx) = mpsc::unbounded_channel();
        let (utx, _urx) = mpsc::unbounded_channel();
        let mut conn = Connection::new("ws://localhost:3000".into(), keys.clone(), rx, utx);

        for body in ["first", "second", "third", "fourth"] {
            conn.outbox.push_back(chat(&keys, body));
        }

        // The socket takes two writes and then dies.
        let mut sink = FlakySink {
            remaining: 2,
            written: Vec::new(),
        };
        let outcome = conn.flush(&mut sink).await;

        assert!(outcome.is_err(), "a dead socket must be reported");
        assert_eq!(sink.written.len(), 2, "only two writes got through");
        assert_eq!(
            conn.outbox.len(),
            2,
            "the failed event and its successors must stay queued"
        );
        assert!(
            conn.outbox[0].content == "third" && conn.outbox[1].content == "fourth",
            "the queue must keep its order, got {:?}",
            conn.outbox.iter().map(|e| &e.content).collect::<Vec<_>>()
        );

        // A healthy socket then drains the remainder, in order.
        let mut sink = FlakySink {
            remaining: 8,
            written: Vec::new(),
        };
        conn.flush(&mut sink).await.expect("a live socket drains");
        assert!(conn.outbox.is_empty());
        assert!(sink.written[0].contains("third"));
        assert!(sink.written[1].contains("fourth"));
    }

    #[test]
    fn commands_staged_while_offline_are_remembered() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let (utx, _urx) = mpsc::unbounded_channel();
        let mut conn = Connection::new("ws://localhost:3000".into(), Keys::generate(), rx, utx);

        conn.stage(Command::Subscribe {
            id: "main".into(),
            filters: vec![Filter::new().kinds(crate::proto::kinds::filter([9]))],
            oneshot: false,
        });
        assert!(conn.subscriptions.contains_key("main"));

        conn.stage(Command::Unsubscribe("main".into()));
        assert!(conn.subscriptions.is_empty());
    }

    /// The wire format is the contract with the relay, so pin it down.
    #[test]
    fn channel_filters_serialise_to_the_h_tag_form_buzz_expects() {
        let filter = Filter::new()
            .kinds(crate::proto::kinds::filter(
                crate::proto::kinds::CHANNEL_STREAM,
            ))
            .custom_tag(SingleLetterTag::LOWERCASE_H, "room-uuid")
            .limit(200);
        let json = filter.as_json();
        assert!(json.contains(r##""#h":["room-uuid"]"##), "{json}");
        assert!(json.contains(r#""limit":200"#), "{json}");

        let frame = ClientMessage::req(SubscriptionId::new("chan"), vec![filter]).as_json();
        assert!(frame.starts_with(r##"["REQ","chan",{"##), "{frame}");
    }

    #[test]
    fn a_kind_nine_message_carries_its_channel_tag() {
        use nostr::event::{EventBuilder, Tag};
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::from_u16(9), "hello")
            .tag(Tag::parse(["h", "room-uuid"]).unwrap())
            .finalize(&keys)
            .unwrap();
        assert_eq!(crate::proto::channel_of(&event), Some("room-uuid"));
        event.verify().unwrap();
    }
}
