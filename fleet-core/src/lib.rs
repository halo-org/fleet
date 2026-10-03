//! An AI agent harness.

#![no_std]

extern crate alloc;

#[cfg_attr(not(test), expect(dead_code, reason = "consumer not yet implemented"))]
mod task;
mod types;

pub use self::types::{
    Agent, AgentId, AgentParent, AgentState, Configured, ContentBlock, Event, EventCursor,
    EventPayload, JsonObject, JsonValue, Message, MessageId, MessageMode, Model, ModelContext,
    ModelContinuation, ModelInput, ModelName, ModelOutput, ModelProvenance, ModelReasoning,
    ModelResponse, ModelResponseChunk, ModelTool, ModelUsage, Role, RoleName, RoleParameter,
    RoleParameterName, RoleParameterTarget, Tool, ToolAlias, ToolCall, ToolCallId, ToolContext,
    ToolName, ToolOutput, ToolResult, Wake,
};
