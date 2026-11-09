//! Agent collection scans and inbox management, independent of core execution.
//!
//! Bootstrap with independent scans and resume changes from the minimum of their
//! read revisions. Merge summaries by their version; keep deletion versions while
//! older reads can still arrive. A later page never advances the subscription's
//! applied revision. Persist applied state and the subscription revision together.
//! Scans have no effect on the change stream, which always covers all agents.

use alloc::vec::Vec;
use core::{fmt, num::NonZeroUsize};
use fleet_core::{AgentId, RoleName};

use crate::EventDirection;

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

    /// Applies an idempotent action if the agent is idle at `expected`.
    fn set_inbox(
        &self,
        idempotency_key: &str,
        agent: AgentId,
        expected: CollectionRevision,
        action: InboxAction,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
