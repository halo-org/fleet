use alloc::vec::Vec;
use core::fmt;
use fleet_core::{
    Agent, AgentId, Configured, ContentBlock, Event, JsonObject, MessageId, RoleName,
};

use crate::{EventQuery, TurnEndEvent};

/// Agent access granted to a caller, optionally within a tool call.
///
/// Metadata reads may include ancestors. History reads require permission to inspect the agent's
/// conversation. Mutations require a tool call. An agent's execution belongs to one call at a time;
/// its conversation remains after that call ends. A durable result or accepted interruption closes
/// the scope, stopping owned execution, pending input and nested calls. Dropping an execution
/// attempt does not close it. Retries recover the same operations after restarts.
///
/// Keys identify operations within the calling agent and tool call, independently of the provider.
/// A successful retry never repeats an operation. Reusing a key with different arguments conflicts.
pub trait Agents {
    /// Provider error.
    type Error: fleet_core::Error<AgentsErrorKind> + Send + 'static;

    /// Reads an agent's metadata.
    fn get(&self, agent: AgentId) -> impl Future<Output = Result<Agent, Self::Error>> + Send;

    /// Reads committed events without consuming them.
    fn events(
        &self,
        agent: AgentId,
        query: EventQuery,
    ) -> impl Future<Output = Result<Vec<Event>, Self::Error>> + Send;

    /// Waits for the first turn end after the message was consumed.
    ///
    /// Repeated waits return the same event. Async work does not delay completion. A message
    /// withdrawn before consumption returns [`MessageNotFound`](AgentsErrorKind::MessageNotFound).
    fn wait_for_turn_end(
        &self,
        agent: AgentId,
        message: MessageId,
    ) -> impl Future<Output = Result<TurnEndEvent, Self::Error>> + Send;

    /// Creates an idle agent owned by the calling tool call.
    ///
    /// The host records its parent. A role supplies its workspace; creation does not copy one.
    fn create(
        &self,
        key: &str,
        role: &Configured<RoleName>,
        extensions: &JsonObject,
    ) -> impl Future<Output = Result<AgentId, Self::Error>> + Send;

    /// Queues a message, acquiring the agent for this call if it is idle and unowned.
    ///
    /// Ownership lasts until the driving call ends, not until a response or parent turn ends.
    /// Images must already be readable by the caller. Another call's active or closing execution
    /// returns [`Conflict`](AgentsErrorKind::Conflict).
    fn send(
        &self,
        key: &str,
        agent: AgentId,
        content: &[ContentBlock],
    ) -> impl Future<Output = Result<MessageId, Self::Error>> + Send;
}

/// The kind of error returned by agent access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentsErrorKind {
    /// The agent does not exist.
    AgentNotFound,
    /// The message does not exist or was withdrawn.
    MessageNotFound,
    /// The caller cannot access the agent.
    AccessDenied,
    /// The driving call is absent or closing.
    ScopeClosed,
    /// Ownership, state or operation arguments conflict.
    Conflict,
    /// An error not covered by another kind.
    Other,
}

impl fmt::Display for AgentsErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AgentNotFound => "agent not found",
            Self::MessageNotFound => "message not found",
            Self::AccessDenied => "agent access denied",
            Self::ScopeClosed => "agent scope closed",
            Self::Conflict => "agent operation conflict",
            Self::Other => "other error",
        })
    }
}
