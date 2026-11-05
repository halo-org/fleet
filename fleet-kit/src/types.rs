use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::{fmt, num::NonZeroUsize};
use fleet_core::{Agent, Event, EventCursor, Message, MessageId, ToolAlias, ToolCallId, TurnEnd};

// === Queries ===

/// A revision of an agent's stored state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(u128);

/// A value read at a consistent revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResponse<T> {
    /// The revision at which the value was read.
    pub revision: Revision,
    /// The requested value.
    pub value: T,
}

/// Whether a resource changed since a requested revision.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum QueryUpdate<T> {
    /// The resource has not changed.
    #[default]
    Unchanged,
    /// The current value of the resource.
    Changed(T),
}

/// Current resource versions with the same last-change revision.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AgentUpdates {
    /// The last-change revision of every resource in this group.
    pub revision: Revision,
    /// The agent's metadata.
    pub agent: QueryUpdate<Agent>,
    /// The changed messages.
    pub messages: Vec<MessageRecord>,
    /// The agent's activity.
    pub activity: QueryUpdate<AgentActivity>,
    /// The changed tool calls.
    pub tool_calls: Vec<ToolCallRecord>,
    /// The changed events, ordered by cursor.
    pub events: Vec<EventRecord>,
}

// === Agents ===

/// An agent's activity.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AgentActivity {
    /// Whether a turn is in progress.
    pub turn: TurnStatus,
    /// Whether an interrupt is pending.
    pub interrupt_pending: bool,
}

/// A turn's current status.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnStatus {
    /// The agent is responding.
    Active,
    /// The agent has no active turn.
    #[default]
    Idle,
}

// === Messages ===

/// A message's current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRecord {
    /// A message waiting to be consumed.
    Pending {
        /// The stable position in the queue.
        position: u128,
        /// The message.
        message: Message,
    },
    /// A message that was consumed.
    Consumed(MessageId),
    /// A message that was deleted.
    Deleted(MessageId),
}

// === Tool calls ===

/// A tool call's current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallRecord {
    /// A tool call without a committed result.
    Pending {
        /// The tool call ID.
        id: ToolCallId,
        /// The tool alias.
        tool: ToolAlias,
        /// Whether the agent can continue before the call completes.
        is_async: bool,
    },
    /// A tool call with a committed result.
    Resolved(ToolCallId),
}

// === Previews ===

/// An accumulated response preview.
///
/// A later committed event makes this preview obsolete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// The event the response is answering.
    pub input_cursor: EventCursor,
    /// The output blocks, keyed by index.
    pub output: BTreeMap<u32, PreviewContent>,
}

/// The partial content of a preview block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewContent {
    /// Response text.
    Text(String),
    /// Reasoning text.
    Reasoning(String),
    /// A tool call with partial JSON input.
    ToolCall {
        /// The tool call ID.
        id: ToolCallId,
        /// The tool alias.
        tool: ToolAlias,
        /// The input received so far.
        input: String,
    },
}

// === Turns ===

/// An event marking the end of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnEndEvent {
    /// The cursor of the event that ended the turn.
    pub cursor: EventCursor,
    /// The payload of this event.
    pub payload: TurnEnd,
}

// === Events ===

/// An event's current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventRecord {
    /// An event in the log.
    Settled(Event),
    /// An event that was deleted.
    Deleted(EventCursor),
}

/// A query for an event history page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventQuery {
    /// The exclusive starting cursor.
    pub from: Option<EventCursor>,
    /// The exclusive stopping cursor.
    pub until: Option<EventCursor>,
    /// The direction to read events.
    pub direction: EventDirection,
    /// The maximum number of records to return.
    pub limit: NonZeroUsize,
    /// Only return resources changed after this revision.
    pub since: Option<Revision>,
}

/// The direction to read events.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventDirection {
    /// Reads from oldest to newest.
    #[default]
    Forward,
    /// Reads from newest to oldest.
    Backward,
}

// === Impls ===

impl From<u128> for Revision {
    fn from(value: u128) -> Self {
        Self(value)
    }
}

impl From<Revision> for u128 {
    fn from(value: Revision) -> Self {
        value.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Default for EventQuery {
    fn default() -> Self {
        Self {
            from: None,
            until: None,
            direction: EventDirection::Forward,
            limit: NonZeroUsize::new(100).expect("limit is positive"),
            since: None,
        }
    }
}
