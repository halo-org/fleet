use crate::{EventDirection, TurnEndEvent};
use alloc::{boxed::Box, rc::Rc, sync::Arc};
use core::fmt;
use fleet_core::{AgentId, Event, EventCursor, MessageId};
use futures_util::Stream;

// === Queries ===

/// Provides queries for stored agents.
pub trait Queries {
    /// The error type returned by this provider.
    type Error: fleet_core::Error<QueriesErrorKind> + 'static;

    /// Streams events in the given direction.
    ///
    /// Both bounds are exclusive. When `from` is `None`, reading starts at the oldest event for
    /// [`Forward`](EventDirection::Forward) or the newest for
    /// [`Backward`](EventDirection::Backward). When `until` is `None`, reading continues to the
    /// opposite end of the log. The stream does not wait for new events.
    ///
    /// # Errors
    ///
    /// Yields errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](QueriesErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`Other`](QueriesErrorKind::Other) when there are any other provider specific errors.
    ///
    /// An error terminates the stream.
    fn events(
        &self,
        agent_id: AgentId,
        from: Option<EventCursor>,
        until: Option<EventCursor>,
        direction: EventDirection,
    ) -> impl Stream<Item = Result<Event, Self::Error>> + Send;

    /// Waits for the turn containing the message to end.
    ///
    /// Repeated waits return the same turn end, even after later activity. Other pending messages
    /// and outstanding async tool calls do not delay completion.
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
    /// An error not covered by any other kind.
    Other,
}

// === Macros ===

macro_rules! impl_queries_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T> Queries for $t
        where
            T: Queries + ?Sized,
        {
            type Error = T::Error;

            fn events(
                &self,
                agent_id: AgentId,
                from: Option<EventCursor>,
                until: Option<EventCursor>,
                direction: EventDirection,
            ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
                (**self).events(agent_id, from, until, direction)
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

impl_queries_for_pointer!(&T);
impl_queries_for_pointer!(&mut T);
impl_queries_for_pointer!(Box<T>);
impl_queries_for_pointer!(Rc<T>);
impl_queries_for_pointer!(Arc<T>);

impl fmt::Display for QueriesErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentNotFound => f.write_str("agent not found"),
            Self::MessageNotFound => f.write_str("message not found"),
            Self::Other => f.write_str("other error"),
        }
    }
}
