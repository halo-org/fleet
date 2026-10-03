use crate::{EventCursor, ModelName, ParametersError, RoleName, ToolAlias, ToolCallId, ToolName};
use core::fmt;

#[derive(Debug)]
pub enum OperationError<S, R, M, T> {
    ReadWakes(S),
    ReadEvents {
        cursor: EventCursor,
        source: S,
    },
    LoadAgent(S),
    LoadAgentState(S),
    ConsumeMessages(S),
    ParkAgent(S),
    SettleInterrupt(S),
    PrepareToolCall {
        call_id: ToolCallId,
        source: S,
    },
    AppendToolResult {
        call_id: ToolCallId,
        source: S,
    },
    AppendModelResponse {
        model: ModelName,
        source: S,
    },
    AppendCompaction {
        model: ModelName,
        source: S,
    },
    PublishModelResponseChunk {
        model: ModelName,
        source: S,
    },
    LoadRole {
        role: RoleName,
        source: R,
    },
    LoadModel {
        model: ModelName,
        source: M,
    },
    LoadTool {
        tool: ToolName,
        source: T,
    },
    ApplyModelParameters {
        role: RoleName,
        source: ParametersError,
    },
    ApplyToolParameters {
        role: RoleName,
        source: ParametersError,
    },
    GenerateModelResponse {
        model: ModelName,
        source: M,
    },
    CompactModelInput {
        model: ModelName,
        source: M,
    },
    ExecuteTool {
        tool: ToolName,
        call_id: ToolCallId,
        source: T,
    },
    ProduceInterruptedToolOutput {
        tool: ToolName,
        call_id: ToolCallId,
        source: T,
    },
    RoleNotFound {
        role: RoleName,
    },
    ModelNotFound {
        role: RoleName,
        model: ModelName,
    },
    ToolNotFound {
        role: RoleName,
        alias: ToolAlias,
        tool: ToolName,
    },
    IncompleteModelResponse {
        model: ModelName,
        latest_event: EventCursor,
    },
}

impl<S, R, M, T> fmt::Display for OperationError<S, R, M, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadWakes(_) => f.write_str("could not read wakes from store"),
            Self::ReadEvents { cursor, source: _ } => {
                write!(
                    f,
                    "could not read events from store at event cursor {cursor}",
                )
            }
            Self::LoadAgent(_) => f.write_str("could not load agent"),
            Self::LoadAgentState(_) => f.write_str("could not load agent state"),
            Self::ConsumeMessages(_) => f.write_str("could not consume messages"),
            Self::ParkAgent(_) => f.write_str("could not park agent"),
            Self::SettleInterrupt(_) => f.write_str("could not settle interrupt"),
            Self::PrepareToolCall { call_id, source: _ } => {
                write!(f, r#"could not prepare tool call "{call_id}""#)
            }
            Self::AppendToolResult { call_id, source: _ } => {
                write!(f, r#"could not append result for tool call "{call_id}""#)
            }
            Self::AppendModelResponse { model, source: _ } => {
                write!(f, r#"could not append response from model "{model}""#)
            }
            Self::AppendCompaction { model, source: _ } => {
                write!(f, r#"could not append compaction from model "{model}""#)
            }
            Self::PublishModelResponseChunk { model, source: _ } => {
                write!(
                    f,
                    r#"could not publish response chunk from model "{model}""#,
                )
            }
            Self::LoadRole { role, source: _ } => write!(f, r#"could not load role "{role}""#),
            Self::LoadModel { model, source: _ } => write!(f, r#"could not load model "{model}""#),
            Self::LoadTool { tool, source: _ } => write!(f, r#"could not load tool "{tool}""#),
            Self::ApplyModelParameters { role, source: _ } => {
                write!(f, r#"could not apply model parameters for role "{role}""#)
            }
            Self::ApplyToolParameters { role, source: _ } => {
                write!(f, r#"could not apply tool parameters for role "{role}""#)
            }
            Self::GenerateModelResponse { model, source: _ } => {
                write!(f, r#"could not generate response using model "{model}""#)
            }
            Self::CompactModelInput { model, source: _ } => {
                write!(f, r#"could not compact input using model "{model}""#)
            }
            Self::ExecuteTool {
                tool,
                call_id,
                source: _,
            } => {
                write!(
                    f,
                    r#"could not execute tool "{tool}" for tool call "{call_id}""#,
                )
            }
            Self::ProduceInterruptedToolOutput {
                tool,
                call_id,
                source: _,
            } => write!(
                f,
                r#"could not produce interrupted output from tool "{tool}" for tool call "{call_id}""#,
            ),
            Self::RoleNotFound { role } => write!(f, r#"could not find role "{role}""#),
            Self::ModelNotFound { role, model } => {
                write!(f, r#"could not find model "{model}" used by role "{role}""#)
            }
            Self::ToolNotFound { role, alias, tool } => write!(
                f,
                r#"could not find tool "{tool}" used as "{alias}" by role "{role}""#,
            ),
            Self::IncompleteModelResponse {
                model,
                latest_event,
            } => write!(
                f,
                r#"model "{model}" finished streaming without a final response after event {latest_event}"#,
            ),
        }
    }
}

impl<S, R, M, T> core::error::Error for OperationError<S, R, M, T>
where
    S: core::error::Error + 'static,
    R: core::error::Error + 'static,
    M: core::error::Error + 'static,
    T: core::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::ReadWakes(source)
            | Self::ReadEvents { source, .. }
            | Self::LoadAgent(source)
            | Self::LoadAgentState(source)
            | Self::ConsumeMessages(source)
            | Self::ParkAgent(source)
            | Self::SettleInterrupt(source)
            | Self::PrepareToolCall { source, .. }
            | Self::AppendToolResult { source, .. }
            | Self::AppendModelResponse { source, .. }
            | Self::AppendCompaction { source, .. }
            | Self::PublishModelResponseChunk { source, .. } => Some(source),
            Self::LoadRole { source, .. } => Some(source),
            Self::LoadModel { source, .. }
            | Self::GenerateModelResponse { source, .. }
            | Self::CompactModelInput { source, .. } => Some(source),
            Self::LoadTool { source, .. }
            | Self::ExecuteTool { source, .. }
            | Self::ProduceInterruptedToolOutput { source, .. } => Some(source),
            Self::ApplyModelParameters { source, .. }
            | Self::ApplyToolParameters { source, .. } => Some(source),
            Self::RoleNotFound { .. }
            | Self::ModelNotFound { .. }
            | Self::ToolNotFound { .. }
            | Self::IncompleteModelResponse { .. } => None,
        }
    }
}
