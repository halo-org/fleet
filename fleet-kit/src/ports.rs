use crate::{
    AgentActivity, AgentMetadata, AgentUpdates, EventQuery, EventRecord, MessageRecord, Preview,
    QueryResponse, QueryUpdate, Revision, ToolCallRecord, TurnEndEvent,
};
use alloc::{boxed::Box, rc::Rc, sync::Arc, vec::Vec};
use core::fmt;
use fleet_core::{AgentId, MessageId};
use futures_util::Stream;

// === Queries ===

/// Provides queries for stored agents.
///
/// # Revisions
///
/// Snapshot reads use one consistent agent revision. Revisions order changes to stored resources
/// independently of event cursors. Revisions cannot be compared across agents. Preview changes do
/// not advance revisions.
///
/// Reads return current resource versions. When `since` is provided, only resources changed after
/// that revision are included. Deleted resources must remain available until their revisions
/// expire. Revision zero requests current state and never expires.
pub trait Queries {
    /// The error type returned by this provider.
    type Error: fleet_core::Error<QueriesErrorKind> + 'static;

    /// Returns the agent's metadata and its last-change version.
    ///
    /// The metadata version is suitable for edit preconditions; the response's
    /// read revision still identifies the snapshot and may be newer.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`InvalidRevision`](QueriesErrorKind::InvalidRevision) when the provided revision is ahead
    ///   of the agent's current revision.
    /// - [`RevisionExpired`](QueriesErrorKind::RevisionExpired) when changes after the provided
    ///   revision are no longer available.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    fn agent(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> impl Future<Output = Result<QueryResponse<QueryUpdate<AgentMetadata>>, Self::Error>> + Send;

    /// Returns the agent's pending messages in queue order.
    ///
    /// When `since` is provided, returns changed message records, including consumed and deleted
    /// messages.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`InvalidRevision`](QueriesErrorKind::InvalidRevision) when the provided revision is ahead
    ///   of the agent's current revision.
    /// - [`RevisionExpired`](QueriesErrorKind::RevisionExpired) when changes after the provided
    ///   revision are no longer available.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    fn messages(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> impl Future<Output = Result<QueryResponse<Vec<MessageRecord>>, Self::Error>> + Send;

    /// Returns the agent's activity.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`InvalidRevision`](QueriesErrorKind::InvalidRevision) when the provided revision is ahead
    ///   of the agent's current revision.
    /// - [`RevisionExpired`](QueriesErrorKind::RevisionExpired) when changes after the provided
    ///   revision are no longer available.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    fn activity(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> impl Future<Output = Result<QueryResponse<QueryUpdate<AgentActivity>>, Self::Error>> + Send;

    /// Returns the agent's pending tool calls.
    ///
    /// When `since` is provided, returns changed tool call records, including resolved calls.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`InvalidRevision`](QueriesErrorKind::InvalidRevision) when the provided revision is ahead
    ///   of the agent's current revision.
    /// - [`RevisionExpired`](QueriesErrorKind::RevisionExpired) when changes after the provided
    ///   revision are no longer available.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    fn tool_calls(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> impl Future<Output = Result<QueryResponse<Vec<ToolCallRecord>>, Self::Error>> + Send;

    /// Streams the agent's transient model activity and response output.
    ///
    /// The stream first yields the current preview, or `None` when there is no preview. Later items
    /// replace the previous preview. Intermediate previews may be omitted.
    ///
    /// The stream owns its subscription and does not borrow the provider.
    ///
    /// # Errors
    ///
    /// Yields errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    ///
    /// An error terminates the stream.
    fn previews(
        &self,
        agent_id: AgentId,
    ) -> impl Stream<Item = Result<Option<Preview>, Self::Error>> + Send + use<Self>;

    /// Returns a page of events in the given direction.
    ///
    /// Both cursor bounds are exclusive. When `from` is `None`, reads start at the oldest event for
    /// [`Forward`](crate::EventDirection::Forward) or the newest for
    /// [`Backward`](crate::EventDirection::Backward). When `until` is `None`, there is no stopping
    /// cursor. The returned revision covers only the scanned range, including gaps between records.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`InvalidRevision`](QueriesErrorKind::InvalidRevision) when the provided revision is ahead
    ///   of the agent's current revision.
    /// - [`RevisionExpired`](QueriesErrorKind::RevisionExpired) when changes after the provided
    ///   revision are no longer available.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    fn events(
        &self,
        agent_id: AgentId,
        query: EventQuery,
    ) -> impl Future<Output = Result<QueryResponse<Vec<EventRecord>>, Self::Error>> + Send;

    /// Waits for changes after the given revision.
    ///
    /// Each group contains all current resource versions that last changed at the same revision.
    /// Groups are read consistently and ordered by that revision. A group is never split across
    /// responses. Superseded versions may be omitted.
    ///
    /// Passing the returned revision to the next call resumes without gaps. Cancelling the call
    /// does not consume changes.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`InvalidRevision`](QueriesErrorKind::InvalidRevision) when the provided revision is ahead
    ///   of the agent's current revision.
    /// - [`RevisionExpired`](QueriesErrorKind::RevisionExpired) when changes after the provided
    ///   revision are no longer available.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    fn updates(
        &self,
        agent_id: AgentId,
        after: Revision,
    ) -> impl Future<Output = Result<AgentUpdates, Self::Error>> + Send;

    /// Waits for the turn containing the message to end.
    ///
    /// Repeated calls return the same turn end, even after later activity. Other pending messages
    /// and unresolved async tool calls do not delay completion.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`MessageNotFound`](QueriesErrorKind::MessageNotFound) when the message does not exist for
    ///   the agent.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    fn wait_for_turn_end(
        &self,
        agent_id: AgentId,
        message_id: MessageId,
    ) -> impl Future<Output = Result<TurnEndEvent, Self::Error>> + Send;
}

/// The kind of error returned by a query provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueriesErrorKind {
    /// The agent does not exist.
    AgentNotFound,
    /// The message does not exist.
    MessageNotFound,
    /// The revision is ahead of the agent.
    InvalidRevision,
    /// Changes after the revision are no longer available.
    RevisionExpired,
    /// An error not covered by any other kind.
    Other,
}

// === Macros ===

macro_rules! impl_queries_for_pointer {
    ($t:ty $(, $lifetime:lifetime)?) => {
        #[warn(clippy::missing_trait_methods)]
        impl<$($lifetime,)? T> Queries for $t
        where
            T: Queries + ?Sized,
        {
            type Error = T::Error;

            fn agent(
                &self,
                agent_id: AgentId,
                since: Option<Revision>,
            ) -> impl Future<Output = Result<QueryResponse<QueryUpdate<AgentMetadata>>, Self::Error>> + Send {
                (**self).agent(agent_id, since)
            }

            fn messages(
                &self,
                agent_id: AgentId,
                since: Option<Revision>,
            ) -> impl Future<Output = Result<QueryResponse<Vec<MessageRecord>>, Self::Error>> + Send {
                (**self).messages(agent_id, since)
            }

            fn activity(
                &self,
                agent_id: AgentId,
                since: Option<Revision>,
            ) -> impl Future<Output = Result<QueryResponse<QueryUpdate<AgentActivity>>, Self::Error>> + Send {
                (**self).activity(agent_id, since)
            }

            fn tool_calls(
                &self,
                agent_id: AgentId,
                since: Option<Revision>,
            ) -> impl Future<Output = Result<QueryResponse<Vec<ToolCallRecord>>, Self::Error>> + Send {
                (**self).tool_calls(agent_id, since)
            }

            fn previews(
                &self,
                agent_id: AgentId,
            ) -> impl Stream<Item = Result<Option<Preview>, Self::Error>> + Send + use<$($lifetime,)? T> {
                (**self).previews(agent_id)
            }

            fn events(
                &self,
                agent_id: AgentId,
                query: EventQuery,
            ) -> impl Future<Output = Result<QueryResponse<Vec<EventRecord>>, Self::Error>> + Send {
                (**self).events(agent_id, query)
            }

            fn updates(
                &self,
                agent_id: AgentId,
                after: Revision,
            ) -> impl Future<Output = Result<AgentUpdates, Self::Error>> + Send {
                (**self).updates(agent_id, after)
            }

            fn wait_for_turn_end(
                &self,
                agent_id: AgentId,
                message_id: MessageId,
            ) -> impl Future<Output = Result<TurnEndEvent, Self::Error>> + Send {
                (**self).wait_for_turn_end(agent_id, message_id)
            }
        }
    };
}

// === Impls ===

impl_queries_for_pointer!(&'a T, 'a);
impl_queries_for_pointer!(&'a mut T, 'a);
impl_queries_for_pointer!(Box<T>);
impl_queries_for_pointer!(Rc<T>);
impl_queries_for_pointer!(Arc<T>);

impl fmt::Display for QueriesErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentNotFound => f.write_str("agent not found"),
            Self::MessageNotFound => f.write_str("message not found"),
            Self::InvalidRevision => f.write_str("invalid revision"),
            Self::RevisionExpired => f.write_str("revision expired"),
            Self::Other => f.write_str("other error"),
        }
    }
}
