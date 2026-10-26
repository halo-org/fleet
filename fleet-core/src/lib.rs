//! An AI agent harness.

#![no_std]

extern crate alloc;

mod backoff;
mod error;
mod event_log;
mod model_request;
mod operation_error;
mod parameters;
mod ports;
mod runtime;
mod runtime_error;
mod signal;
mod types;

pub(crate) use self::{
    backoff::Backoff,
    event_log::EventLog,
    model_request::ModelRequest,
    operation_error::OperationError,
    parameters::{ParametersError, apply_model_parameters, apply_tool_parameters},
    runtime_error::RuntimeError,
    signal::{PrioritySignal, Signal, SignalReceiver, SignalSender, signal_channel},
};
pub use self::{
    error::Error,
    ports::{
        ClientStore, ClientStoreErrorKind, ErrorHandler, IdleHandler, Models, ModelsErrorKind,
        Prompts, Roles, RuntimeStore, RuntimeStoreErrorKind, Tools, ToolsErrorKind,
    },
    runtime::run,
    types::{
        Agent, AgentId, AgentParent, AgentState, Compaction, Configured, ContentBlock, Event,
        EventCursor, EventPayload, JsonObject, JsonValue, Message, MessageId, MessageMode, Model,
        ModelCompaction, ModelCompactionResponse, ModelContext, ModelContinuation, ModelInput,
        ModelName, ModelOutput, ModelProvenance, ModelReasoning, ModelResponse, ModelResponseChunk,
        ModelTool, ModelUsage, Prompt, PromptContext, PromptName, PromptOutput, PromptResponse,
        Role, RoleCompaction, RoleName, RoleParameter, RoleParameterName, RoleParameterTarget,
        Tool, ToolAlias, ToolCall, ToolCallId, ToolContext, ToolName, ToolOutput, ToolResult,
        TurnEnd, TurnEndReason, Wake,
    },
};
