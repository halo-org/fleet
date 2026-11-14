//! Agent collection scans, application metadata, and inbox management, independent of core execution.
//!
//! Bootstrap with independent scans and resume changes from the minimum of their
//! read revisions. Merge summaries by their version; keep deletion versions while
//! older reads can still arrive. A later page never advances the subscription's
//! applied revision. Persist applied state and the subscription revision together.
//! Scans have no effect on the change stream, which always covers all agents,
//! including agents created by tool calls.

use alloc::{string::String, vec::Vec};
use core::{fmt, num::NonZeroUsize};
use fleet_core::{AgentId, Configured, ContentBlock, JsonObject, MessageId, RoleName, ToolCallId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::{EventDirection, Extension, Revision};

/// Returns the application title stored in an agent's extensions.
#[must_use]
pub fn agent_title(extensions: &JsonObject) -> Option<&str> {
    extensions.get("fleet.title")?.as_str()
}

/// Returns when the agent was pinned, in Unix milliseconds.
#[must_use]
pub fn agent_pinned(extensions: &JsonObject) -> Option<u64> {
    extensions.get("fleet.pinned")?.as_u64()
}

/// The tool call that created an agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentParent {
    /// The agent that made the call.
    #[serde(
        serialize_with = "serialize_agent_id",
        deserialize_with = "deserialize_agent_id"
    )]
    pub agent: AgentId,
    /// The call that created the agent.
    #[serde(
        serialize_with = "serialize_call_id",
        deserialize_with = "deserialize_call_id"
    )]
    pub call: ToolCallId,
}

/// A durable collection revision. Comparable only within the same store.
///
/// This is independent of an individual agent's revision. Providers must order
/// committed changes, not merely allocate numbers before concurrent commits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectionRevision(pub u128);

impl fmt::Display for CollectionRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An agent's inbox section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InboxState {
    /// Idle and awaiting attention.
    Pending,
    /// Waiting for or executing a turn.
    Busy,
    /// Idle and hidden until a deadline.
    Snoozed,
    /// Idle and explicitly settled.
    Settled,
}

/// The sections to scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum InboxFilter {
    /// Every section.
    #[default]
    All,
    /// Pending, busy, and snoozed agents.
    Unsettled,
    /// One section.
    State(InboxState),
}

/// An exclusive scan position, ordered by section, section order, then agent ID.
///
/// Section order is entry time, except snoozes which use their wake deadline.
/// Positions remain valid bounds after the referenced agent moves or disappears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgentPosition {
    /// The section containing this position.
    pub inbox: InboxState,
    /// The provider's section ordering value; not a synchronization revision.
    pub order: u128,
    /// The tie-breaking agent ID.
    pub id: AgentId,
}

/// The current, small collection representation of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    /// The agent ID.
    pub id: AgentId,
    /// Its configured role.
    pub role: RoleName,
    /// The tool call that created the agent, if any.
    pub parent: Option<AgentParent>,
    /// The optional display title.
    pub title: Option<String>,
    /// When the agent was pinned, in Unix milliseconds.
    pub pinned: Option<u64>,
    /// Last metadata change, in this agent's revision sequence.
    pub metadata_version: Revision,
    /// Last collection change to this summary.
    pub version: CollectionRevision,
    /// The inbox section.
    pub inbox: InboxState,
    /// When the agent entered this section, in Unix milliseconds.
    pub since: u64,
    /// The snooze deadline, in Unix milliseconds, if snoozed.
    pub until: Option<u64>,
    /// Whether an interrupt is pending.
    pub interrupt_pending: bool,
    /// The position for subsequent scans.
    pub position: AgentPosition,
}

/// A stateless scan of current agent summaries.
///
/// Bounds are exclusive. `since` filters current summaries by version; it does
/// not report agents which left the scanned sections. Use collection updates
/// for those transitions. A page's revision covers only its scanned range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentsQuery {
    /// Scans the children of this agent, or agents without a parent when `None`.
    pub parent: Option<AgentId>,
    /// Sections to scan.
    pub inbox: InboxFilter,
    /// Exclusive starting position.
    pub from: Option<AgentPosition>,
    /// Exclusive stopping position.
    pub until: Option<AgentPosition>,
    /// Scan direction.
    pub direction: EventDirection,
    /// Maximum returned summaries.
    pub limit: NonZeroUsize,
    /// Only return summaries changed after this revision.
    pub since: Option<CollectionRevision>,
}

impl Default for AgentsQuery {
    fn default() -> Self {
        Self {
            parent: None,
            inbox: InboxFilter::All,
            from: None,
            until: None,
            direction: EventDirection::Forward,
            limit: NonZeroUsize::new(100).expect("positive limit"),
            since: None,
        }
    }
}

/// One consistent collection page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsPage {
    /// Collection revision at which this range was read, even for an empty page.
    pub revision: CollectionRevision,
    /// Current summaries in scan order.
    pub agents: Vec<AgentSummary>,
    /// Continue from this position if more matching rows were present.
    pub next: Option<AgentPosition>,
}

/// A complete group of collection changes at one revision.
///
/// Superseded versions may be omitted. Upsert summaries by ID; removals are
/// idempotent. Apply the group before saving its revision. Parallel bootstrap
/// reads resume from their minimum read revision and merge by summary version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsUpdates {
    /// Revision of all changes in this group.
    pub revision: CollectionRevision,
    /// Current changed summaries.
    pub agents: Vec<AgentSummary>,
    /// Deleted agent IDs, at this group's revision.
    pub removed: Vec<AgentId>,
}

/// An explicit inbox action for an idle agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxAction {
    /// Restore an idle agent to pending.
    Pending,
    /// Settle an idle agent.
    Settle,
    /// Snooze an idle agent until these Unix milliseconds.
    Snooze(u64),
}

/// Collection queries and mutations provided above the core store interface.
pub trait AgentCollection {
    /// Provider error.
    type Error: core::error::Error + Send + 'static;

    /// Reads one page without changing subscription coverage.
    fn agents(
        &self,
        query: AgentsQuery,
    ) -> impl Future<Output = Result<AgentsPage, Self::Error>> + Send;

    /// Waits for the next complete current-version group after `after`.
    /// Cancellation does not consume changes. Revision zero never expires.
    fn agents_updates(
        &self,
        after: CollectionRevision,
    ) -> impl Future<Output = Result<AgentsUpdates, Self::Error>> + Send;

    /// Sets or clears the title if the metadata is still at `expected`.
    ///
    /// `expected` is [`AgentSummary::metadata_version`], not its collection version.
    /// Successful retries with the same key and arguments never reapply an edit.
    /// Unchanged titles do not advance revisions. Activity does not prevent edits.
    fn set_title(
        &self,
        idempotency_key: &str,
        agent: AgentId,
        expected: Revision,
        title: Option<&str>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Pins or unpins the agent if its metadata is still at `expected`.
    ///
    /// `expected` is [`AgentSummary::metadata_version`], not its collection version.
    /// Pinning records the current time and keeps an existing pin. Settled agents
    /// cannot be pinned. Activity does not prevent changes.
    fn set_pinned(
        &self,
        idempotency_key: &str,
        agent: AgentId,
        expected: Revision,
        pinned: bool,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Applies an idempotent action if the agent is idle at `expected`.
    ///
    /// Settling also unpins the agent.
    fn set_inbox(
        &self,
        idempotency_key: &str,
        agent: AgentId,
        expected: CollectionRevision,
        action: InboxAction,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Creates agents on behalf of one tool call.
pub trait Subagents {
    /// Provider error.
    type Error: core::error::Error + Send + 'static;

    /// Creates an agent with a queued initial message.
    ///
    /// The agent records the call as its [`AgentParent`]. Retries with the same arguments
    /// return the same agent and message.
    fn create(
        &self,
        role: &Configured<RoleName>,
        extensions: &JsonObject,
        content: &[ContentBlock],
    ) -> impl Future<Output = Result<(AgentId, MessageId), Self::Error>> + Send;
}

impl Extension for AgentParent {
    const KEY: &'static str = "fleet.parent";
}

fn serialize_agent_id<S: Serializer>(id: &AgentId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(id)
}

fn deserialize_agent_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<AgentId, D::Error> {
    let id = String::deserialize(deserializer)?;
    id.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| id.parse::<u128>().ok())
        .flatten()
        .map(AgentId::from)
        .ok_or_else(|| D::Error::custom("agent ID must be a decimal number"))
}

fn serialize_call_id<S: Serializer>(id: &ToolCallId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(id.as_ref())
}

fn deserialize_call_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ToolCallId, D::Error> {
    String::deserialize(deserializer).map(ToolCallId::from)
}
