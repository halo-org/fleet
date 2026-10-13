//! An AI agent harness.

#![no_std]

extern crate alloc;

mod error;
mod ports;
#[cfg_attr(not(test), expect(dead_code, reason = "consumer not yet implemented"))]
mod task;
mod types;

pub use self::{
    error::Error,
    ports::{
        ClientStore, ClientStoreErrorKind, ErrorHandler, IdleHandler, Models, ModelsErrorKind,
        Prompts, Roles, RuntimeStore, RuntimeStoreErrorKind, Tools, ToolsErrorKind,
    },
    types::{
        Agent, AgentId, AgentParent, AgentState, Compaction, Configured, ContentBlock, Event,
        EventCursor, EventPayload, JsonObject, JsonValue, Message, MessageId, MessageMode, Model,
        ModelCompaction, ModelCompactionResponse, ModelContext, ModelContinuation, ModelInput,
        ModelName, ModelOutput, ModelProvenance, ModelReasoning, ModelResponse, ModelResponseChunk,
        ModelTool, ModelUsage, Prompt, PromptContext, PromptName, PromptOutput, PromptResponse,
        Role, RoleCompaction, RoleName, RoleParameter, RoleParameterName, RoleParameterTarget,
        Tool, ToolAlias, ToolCall, ToolCallId, ToolContext, ToolName, ToolOutput, ToolResult, Wake,
    },
};
