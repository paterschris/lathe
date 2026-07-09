//! Lathe-owned ACP instrumentation: the stream-message model, the per-connection
//! log tap, and the connection registry that backs the ACP logs panel.
//!
//! Child module of [`super`] (`acp_tools`). These types were added by Lathe on
//! top of the upstream `AcpTools` view, which now renders from them. They live
//! here, out of the upstream-owned `acp_tools.rs`, so future upstream merges do
//! not collide, and are re-exported via `pub use lathe::*` so existing
//! `acp_tools::` paths resolve unchanged.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StreamMessageDirection {
    Incoming,
    Outgoing,
    /// Lines captured from the agent's stderr. These are not part of the
    /// JSON-RPC protocol, but agents often emit useful diagnostics there.
    Stderr,
}

#[derive(Clone)]
pub enum StreamMessageContent {
    Request {
        id: acp::RequestId,
        method: Arc<str>,
        params: Option<serde_json::Value>,
    },
    Response {
        id: acp::RequestId,
        result: Result<Option<serde_json::Value>, acp::Error>,
    },
    Notification {
        method: Arc<str>,
        params: Option<serde_json::Value>,
    },
    /// A raw stderr line from the agent process.
    Stderr { line: Arc<str> },
}

#[derive(Clone)]
pub struct StreamMessage {
    pub direction: StreamMessageDirection,
    pub message: StreamMessageContent,
}

impl StreamMessage {
    /// Build a `StreamMessage` from a raw line captured off the transport.
    ///
    /// For `Stderr`, the line is wrapped as-is (no JSON parsing). For
    /// `Incoming`/`Outgoing`, the line is parsed as JSON-RPC; returns `None`
    /// if it doesn't look like a valid JSON-RPC message.
    pub fn from_raw_line(direction: StreamMessageDirection, line: &str) -> Option<Self> {
        if direction == StreamMessageDirection::Stderr {
            return Some(StreamMessage {
                direction,
                message: StreamMessageContent::Stderr {
                    line: Arc::from(line),
                },
            });
        }

        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let obj = value.as_object()?;

        let parsed_id = obj
            .get("id")
            .map(|raw| serde_json::from_value::<acp::RequestId>(raw.clone()));

        let message = if let Some(method) = obj.get("method").and_then(|m| m.as_str()) {
            match parsed_id {
                Some(Ok(id)) => StreamMessageContent::Request {
                    id,
                    method: method.into(),
                    params: obj.get("params").cloned(),
                },
                Some(Err(err)) => {
                    log::warn!("Skipping JSON-RPC message with unparsable id: {err}");
                    return None;
                }
                None => StreamMessageContent::Notification {
                    method: method.into(),
                    params: obj.get("params").cloned(),
                },
            }
        } else if let Some(parsed_id) = parsed_id {
            let id = match parsed_id {
                Ok(id) => id,
                Err(err) => {
                    log::warn!("Skipping JSON-RPC response with unparsable id: {err}");
                    return None;
                }
            };
            if let Some(error) = obj.get("error") {
                let acp_err =
                    serde_json::from_value::<acp::Error>(error.clone()).unwrap_or_else(|err| {
                        log::warn!("Failed to deserialize ACP error: {err}");
                        acp::Error::internal_error().data(error.to_string())
                    });
                StreamMessageContent::Response {
                    id,
                    result: Err(acp_err),
                }
            } else {
                StreamMessageContent::Response {
                    id,
                    result: Ok(obj.get("result").cloned()),
                }
            }
        } else {
            return None;
        };

        Some(StreamMessage { direction, message })
    }
}

struct GlobalAcpConnectionRegistry(Entity<AcpConnectionRegistry>);

impl Global for GlobalAcpConnectionRegistry {}

/// A raw line captured from the transport (or from stderr), tagged with
/// direction. Deserialization into [`StreamMessage`] happens on the
/// registry's foreground task so the ring buffer can be replayed to late
/// subscribers.
struct RawStreamLine {
    direction: StreamMessageDirection,
    line: Arc<str>,
}

/// Handle to an ACP connection's log tap. Passed back by
/// [`AcpConnectionRegistry::set_active_connection`] so that the connection
/// can publish transport and stderr lines without knowing anything about
/// the logs panel's channel.
///
/// The tap carries a shared `enabled` flag that the registry flips on when
/// the first observer subscribes. Until then, `emit_*` methods are
/// effectively free: they check an atomic and return. This keeps the
/// logs panel's memory footprint opt-in — if no one ever opens it, the
/// transport never allocates a line or pushes a channel item.
#[derive(Clone)]
pub struct AcpLogTap {
    enabled: Arc<AtomicBool>,
    sender: smol::channel::Sender<RawStreamLine>,
}

impl AcpLogTap {
    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }

    fn emit(&self, direction: StreamMessageDirection, line: &str) {
        if !self.is_enabled() {
            return;
        }
        self.sender
            .try_send(RawStreamLine {
                direction,
                line: Arc::from(line),
            })
            .log_err();
    }

    /// Record a line read from the agent's stdout.
    pub fn emit_incoming(&self, line: &str) {
        self.emit(StreamMessageDirection::Incoming, line);
    }

    /// Record a line written to the agent's stdin.
    pub fn emit_outgoing(&self, line: &str) {
        self.emit(StreamMessageDirection::Outgoing, line);
    }

    /// Record a line read from the agent's stderr.
    pub fn emit_stderr(&self, line: &str) {
        self.emit(StreamMessageDirection::Stderr, line);
    }
}

/// Maximum number of messages retained in the registry's backlog.
///
/// Mirrors `MAX_STORED_LOG_ENTRIES` in the LSP log store, so that opening the
/// ACP logs panel after a session has been running for a while still shows
/// meaningful history.
const MAX_BACKLOG_MESSAGES: usize = 2000;

#[derive(Default)]
pub struct AcpConnectionRegistry {
    pub(super) active_agent_id: Option<AgentId>,
    pub(super) generation: u64,
    /// Bounded ring buffer of every message observed on the current connection.
    /// When a new connection is set, this is cleared.
    backlog: VecDeque<StreamMessage>,
    subscribers: Vec<smol::channel::Sender<StreamMessage>>,
    /// The tap handed to the currently active connection, so the registry
    /// can flip its `enabled` flag the first time someone subscribes.
    active_tap: Option<AcpLogTap>,
    _broadcast_task: Option<Task<()>>,
}

impl AcpConnectionRegistry {
    pub fn default_global(cx: &mut App) -> Entity<Self> {
        if cx.has_global::<GlobalAcpConnectionRegistry>() {
            cx.global::<GlobalAcpConnectionRegistry>().0.clone()
        } else {
            let registry = cx.new(|_cx| AcpConnectionRegistry::default());
            cx.set_global(GlobalAcpConnectionRegistry(registry.clone()));
            registry
        }
    }

    /// Register a new active connection and return an [`AcpLogTap`] that
    /// the connection should hand to its transport + stderr readers.
    ///
    /// The tap starts out disabled: transport lines are dropped cheaply
    /// until someone subscribes via [`Self::subscribe`], at which point
    /// the tap is flipped on and subsequent lines are broadcast to all
    /// current and future subscribers.
    pub fn set_active_connection(
        &mut self,
        agent_id: AgentId,
        cx: &mut Context<Self>,
    ) -> AcpLogTap {
        let (sender, raw_rx) = smol::channel::unbounded::<RawStreamLine>();
        let tap = AcpLogTap {
            enabled: Arc::new(AtomicBool::new(false)),
            sender,
        };

        self.active_agent_id = Some(agent_id);
        self.generation += 1;
        self.backlog.clear();
        self.subscribers.clear();
        self.active_tap = Some(tap.clone());

        self._broadcast_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(raw) = raw_rx.recv().await {
                this.update(cx, |this, _cx| {
                    let Some(message) = StreamMessage::from_raw_line(raw.direction, &raw.line)
                    else {
                        return;
                    };

                    if this.backlog.len() == MAX_BACKLOG_MESSAGES {
                        this.backlog.pop_front();
                    }
                    this.backlog.push_back(message.clone());

                    this.subscribers.retain(|sender| !sender.is_closed());
                    for sender in &this.subscribers {
                        sender.try_send(message.clone()).log_err();
                    }
                })
                .log_err();
            }

            // The transport closed — clear state so observers (e.g. the ACP
            // logs tab) can transition back to the disconnected state.
            this.update(cx, |this, cx| {
                this.active_agent_id = None;
                this.subscribers.clear();
                this.active_tap = None;
                cx.notify();
            })
            .log_err();
        }));

        cx.notify();
        tap
    }

    /// Clear the retained message history for the current connection and force
    /// watchers to resubscribe so their local correlation state is reset too.
    pub fn clear_messages(&mut self, cx: &mut Context<Self>) {
        self.backlog.clear();
        self.generation += 1;
        self.subscribers.clear();
        cx.notify();
    }

    /// Subscribe to messages on the current connection.
    ///
    /// Returns the existing backlog (already-observed messages) together with
    /// a receiver for new messages. The caller is responsible for flushing the
    /// backlog into its local state before draining the receiver, so that no
    /// messages are dropped between the snapshot and live subscription.
    ///
    /// The first subscription enables the connection's log tap; prior
    /// messages are therefore not available. This is intentional: the tap
    /// is opt-in so that the default case (no one ever opens the ACP logs
    /// panel) performs zero per-message bookkeeping.
    pub fn subscribe(&mut self) -> (Vec<StreamMessage>, smol::channel::Receiver<StreamMessage>) {
        if let Some(tap) = &self.active_tap {
            tap.enable();
        }
        let backlog = self.backlog.iter().cloned().collect();
        let (sender, receiver) = smol::channel::unbounded();
        self.subscribers.push(sender);
        (backlog, receiver)
    }
}
