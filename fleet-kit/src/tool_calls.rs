use crate::{Extension, Revision};
use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::fmt;
use fleet_core::{ToolAlias, ToolCallId};
use serde::{Deserialize, Serialize};

/// Access to the calling agent's own tool calls within a driving tool call.
///
/// Access closes once the driving call is absent, resolved or being interrupted. The driving call
/// is not listed and cannot be interrupted through this access.
///
/// Task handles let a model name its tool calls. A tool declares the input property that holds one
/// with [`TaskHandleProperty`]. Handles are resolved within the driving call's context: the tool
/// calls the model could see when it made the driving call. A call never sees later calls that
/// reuse its handle, while calls in the same response see each other. Tools decide how to report
/// duplicate and unknown handles.
///
/// While access is open, message counts never decrease. A completed handle stays completed unless
/// the agent's role or the tool declarations change. Waits on these conditions do not depend on
/// timing.
pub trait ToolCalls {
    /// Provider error.
    type Error: fleet_core::Error<ToolCallsErrorKind> + Send + 'static;

    /// Returns the state of the caller's tool calls once it is newer than `after`.
    ///
    /// Without `after`, returns the current state. The state reports the status of the requested
    /// task handles in the driving call's context.
    fn state(
        &self,
        after: Option<Revision>,
        task_handles: &[String],
    ) -> impl Future<Output = Result<ToolCallsState, Self::Error>> + Send;

    /// Interrupts unresolved tool calls of the caller.
    ///
    /// Resolved calls are ignored. Keys identify operations within the caller and driving call. A
    /// retry never repeats an operation and succeeds even after access closes. Reusing a key with
    /// different calls conflicts.
    fn interrupt(
        &self,
        key: &str,
        calls: &[ToolCallId],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// The state of an agent's tool calls, seen from a driving call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallsState {
    /// The revision at which this state was read.
    pub revision: Revision,
    /// The agent's other unresolved tool calls, ordered by ID.
    pub pending_tool_calls: Vec<PendingToolCall>,
    /// The status of each requested task handle.
    pub task_handles: BTreeMap<String, TaskStatus>,
    /// Messages the agent had not seen when the driving call was made, including those sent since.
    ///
    /// Consuming a message does not change these counts.
    pub messages: MessageCounts,
}

/// An unresolved tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingToolCall {
    /// The tool call ID.
    pub id: ToolCallId,
    /// The tool alias.
    pub tool: ToolAlias,
    /// Whether the agent can continue before the call completes.
    pub is_async: bool,
    /// The task handle in the call's input.
    pub task_handle: Option<String>,
}

/// The status of a task handle in the driving call's context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// No tool call has the handle.
    Unknown,
    /// The tool call with the handle has no result yet.
    Pending(ToolCallId),
    /// The tool call with the handle has a committed result.
    Completed(ToolCallId),
    /// More than one tool call has the handle.
    Duplicate,
}

/// Message counts by mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MessageCounts {
    /// Steering messages.
    pub steering: u64,
    /// Queued messages.
    pub queued: u64,
}

/// The input property that holds a tool call's task handle.
///
/// Tools declare this in their extensions. Calls without a string in the property have no handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskHandleProperty(pub String);

/// The kind of error returned by tool call access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallsErrorKind {
    /// The driving call is absent, resolved or being interrupted.
    ScopeClosed,
    /// State or operation arguments conflict.
    Conflict,
    /// An error not covered by another kind.
    Other,
}

impl Extension for TaskHandleProperty {
    const KEY: &'static str = "fleet.task_handle";
}

impl fmt::Display for ToolCallsErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ScopeClosed => "tool call scope closed",
            Self::Conflict => "tool call operation conflict",
            Self::Other => "other error",
        })
    }
}
