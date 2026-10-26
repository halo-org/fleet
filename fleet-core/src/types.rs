use alloc::{borrow::ToOwned as _, collections::BTreeMap, string::String, vec::Vec};
use core::fmt;

/// A JSON value.
pub type JsonValue = serde_json::Value;

/// A JSON object.
pub type JsonObject = serde_json::Map<String, JsonValue>;

// === Agents ===

/// The metadata of an agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Agent {
    /// The unique identifier of this agent.
    pub id: AgentId,
    /// The parent of this agent.
    pub parent: AgentParent,
    /// The role that defines this agent's behavior.
    pub role: Configured<RoleName>,
}

/// The ID of an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(u128);

/// The parent of an agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentParent {
    /// The user.
    User,
    /// Another agent.
    Agent(AgentId, ToolCallId),
}

/// The runtime state of an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentState {
    /// The cursor of latest event in the agent's event log.
    pub latest_event: Option<EventCursor>,
    /// The cursor of the event the agent is parked at.
    pub parked_at: Option<EventCursor>,
    /// Whether the agent should be interrupted.
    pub interrupt_intent: bool,
}

// === Roles ===

/// The metadata of a role.
///
/// A role defines the behavior and capabilities of an agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Role {
    /// The name that uniquely identifies this role.
    pub name: RoleName,
    /// The parameters this role exposes.
    pub parameters: BTreeMap<RoleParameterName, RoleParameter>,
    /// The model that powers agents with this role.
    pub model: Configured<ModelName>,
    /// The tools available to agent with this role.
    pub tools: BTreeMap<ToolAlias, Configured<ToolName>>,
    /// The system prompt to pass to the model.
    pub system_prompt: String,
    /// The compaction settings for this role.
    ///
    /// When `None`, the model's default compaction settings are used.
    pub compaction: Option<RoleCompaction>,
}

/// The name of a role.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleName(String);

/// The metadata of a role parameter.
///
/// A role parameter enables runtime overrides of model and tool config. Agents can be created with
/// values for the role parameters of its role.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoleParameter {
    /// Where the value of this role parameter should replace.
    pub target: RoleParameterTarget,
}

/// The name of a role parameter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleParameterName(String);

/// Where a role parameter should override.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RoleParameterTarget {
    /// A model's config path.
    Model(Vec<String>),
    /// A tool's config path.
    Tool(ToolAlias, Vec<String>),
}

/// The compaction settings of a role.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoleCompaction {
    /// The input token count that triggers compaction.
    ///
    /// When `None`, [`Model::compaction_threshold`] is used.
    pub threshold: Option<u32>,
    /// Custom summarization instructions.
    ///
    /// When `None`, the model provider uses its default instructions.
    pub instructions: Option<String>,
}

// === Models ===

/// The metadata of a model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Model {
    /// The name that uniquely identifies this model.
    pub name: ModelName,
    /// The JSON schema for this model's config.
    pub config_schema: JsonObject,
    /// The maximum number of combined input and output tokens this model supports.
    pub context_window: u32,
    /// The recommended number of input tokens at which to compact this model's input.
    pub compaction_threshold: u32,
}

/// The name of a model.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelName(String);

/// The context passed to a model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelContext {
    /// The model name and config.
    pub model: Configured<ModelName>,
    /// The system prompt to use.
    pub system_prompt: String,
    /// The tools available to the model.
    pub tools: Vec<ModelTool>,
}

/// A model's view of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelTool {
    /// The name of the tool.
    pub name: ToolAlias,
    /// The description of the tool.
    pub description: Option<String>,
    /// Whether the model may continue without waiting for this tool’s result.
    pub is_async: bool,
    /// The schema of the tool's input object.
    pub input_schema: JsonObject,
}

/// Input to a model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModelInput {
    /// Content sent by the user.
    User(Vec<ContentBlock>),
    /// Content of the previous model responses.
    Assistant(ModelProvenance, Vec<ModelOutput>),
    /// Content of a tool result.
    ToolResult(ToolResult),
    /// Compacted context from a model.
    Compaction(ModelProvenance, ModelCompaction),
}

/// A response from a model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelResponse {
    /// The response output blocks.
    pub output: Vec<ModelOutput>,
    /// How many tokens were used to serve this response.
    pub usage: ModelUsage,
    /// Provenance to identify which underlying model served this response.
    pub provenance: ModelProvenance,
    /// A token to continue from this response.
    pub continuation_token: Option<String>,
}

/// A chunk of a streamed model response.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModelResponseChunk {
    /// The start of a text block.
    TextStart(u32),
    /// The start of a reasoning block.
    ReasoningStart(u32),
    /// The start of a tool call block.
    ToolCallStart(u32, ToolCallId, ToolAlias),
    /// An update to a text block.
    TextDelta(u32, String),
    /// An update to a reasoning block.
    ReasoningDelta(u32, String),
    /// An update to a tool call block.
    ToolInputDelta(u32, String),
    /// The end of a block.
    OutputEnd(u32, ModelOutput),
    /// The end of the response.
    ResponseEnd(ModelResponse),
}

/// A model output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModelOutput {
    /// Text.
    Text(String),
    /// Reasoning.
    Reasoning(ModelReasoning),
    /// Tool call.
    ToolCall(ToolCall),
}

/// Reasoning output from a model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelReasoning {
    /// Readable content.
    ///
    /// Depending on the model, this may contain the full reasoning content or just a summary.
    pub content: Option<String>,
    /// Redacted content.
    ///
    /// This is opaque to us, but can be read by the model on future turns.
    pub redacted_content: Option<String>,
    /// An attestation of this reasoning output.
    ///
    /// Depending on the model, this may be a signature or an ID.
    pub attestation: Option<String>,
}

/// A response from compacting model input.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelCompactionResponse {
    /// The input that replaces the compacted context.
    pub input: Vec<ModelInput>,
    /// How many tokens were used for compaction.
    pub usage: ModelUsage,
}

/// Compaction output from a model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelCompaction {
    /// Readable summary.
    pub content: Option<String>,
    /// Redacted content.
    ///
    /// This is opaque to us, but can be read by the model on future turns.
    pub redacted_content: Option<String>,
    /// An attestation of this compaction output.
    ///
    /// Depending on the model, this may be a signature or an ID.
    pub attestation: Option<String>,
}

/// Token usage of a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModelUsage {
    /// Input tokens.
    ///
    /// This includes cache reads and writes.
    pub input: u32,
    /// Output tokens.
    pub output: u32,
    /// Tokens read from cache.
    pub cache_read: u32,
    /// Tokens written to cache.
    pub cache_write: u32,
}

/// Continuation from a model response.
///
/// With this, a model can continue from a point in a conversation without being provided the input
/// items prior to that point. This enables significant optimizations on loading and sending context
/// to the model. Note that this is not the same as the model's context cache, which can be used
/// without this.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelContinuation {
    /// The continuation token.
    pub token: String,
    /// Provenance to identify which underlying model the token can be used with.
    pub provenance: ModelProvenance,
}

/// Model provenance.
///
/// Some parts of a model's output are not portable between different models and providers. Because
/// of that, it's important to know which model each output came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelProvenance {
    /// The model provider ID.
    pub provider_id: String,
    /// The model ID.
    pub model_id: String,
}

// === Tools ===

/// The metadata of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tool {
    /// The name that uniquely identifies this tool.
    pub name: ToolName,
    /// A description of how to use this tool.
    ///
    /// This is intended to provide context to a model with access to the tool, helping it call the
    /// tool correctly.
    pub description: Option<String>,
    /// Whether the model may continue without waiting for this tool’s result.
    pub is_async: bool,
    /// The JSON schema for this tool's config.
    pub config_schema: JsonObject,
    /// The JSON schema for this tool's input.
    pub input_schema: JsonObject,
    /// The JSON schema for this tool's structured output.
    ///
    /// When `None`, the tool doesn't support structured output.
    pub output_schema: Option<JsonObject>,
}

/// The name of a tool.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolName(String);

/// An alias for a tool.
///
/// This is what models will see as the name of the tool.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolAlias(String);

/// The context passed to a tool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolContext {
    /// The tool name and config.
    pub tool: Configured<ToolName>,
    /// The ID of the agent using the tool.
    pub agent_id: AgentId,
    /// The tool call ID.
    pub call_id: ToolCallId,
}

/// A request to call a tool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCall {
    /// The unique identifier of this tool call.
    ///
    /// This is only unique within an agent's own history. There is no guarantee of uniqueness
    /// between different agents.
    pub id: ToolCallId,
    /// The tool to call.
    pub tool: ToolAlias,
    /// Whether the model may continue without waiting for this tool call’s result.
    pub is_async: bool,
    /// The input passed to the tool.
    ///
    /// Input that doesn't match the tool's [`input_schema`](Tool::input_schema) may cause the tool
    /// to return an error.
    pub input: JsonObject,
}

/// The result of a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolResult {
    /// The tool call ID.
    ///
    /// This must match the ID of the corresponding tool call.
    pub call_id: ToolCallId,
    /// The output of the tool.
    pub output: ToolOutput,
}

/// ID of a tool call.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolCallId(String);

/// The output of a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolOutput {
    /// The output content.
    pub content: Vec<ContentBlock>,
    /// The output structured content.
    pub structured_content: Option<JsonObject>,
    /// Whether the tool returned an error.
    pub is_error: bool,
}

// === Prompts ===

/// The metadata of a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Prompt {
    /// The name that uniquely identifies this prompt.
    pub name: PromptName,
    /// The JSON schema for this prompt's config.
    pub config_schema: JsonObject,
    /// The JSON schema for this prompt's input.
    pub input_schema: JsonObject,
}

/// The name of a prompt.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PromptName(String);

/// The context passed to a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PromptContext {
    /// The prompt name and config.
    pub prompt: Configured<PromptName>,
    /// The ID of the agent the prompt is being rendered for.
    pub agent_id: AgentId,
}

/// A response from rendering a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PromptResponse {
    /// The response output items.
    pub output: Vec<PromptOutput>,
}

/// A rendered prompt output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PromptOutput {
    /// Content attributed to the user.
    User(Vec<ContentBlock>),
    /// Content attributed to the assistant.
    Assistant(Vec<ContentBlock>),
}

// === Configuration ===

/// An item's name and config.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Configured<N> {
    /// The name of the item being configured.
    pub name: N,
    /// The config object.
    pub config: JsonObject,
}

// === Content ===

/// A content block.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContentBlock {
    /// Text content.
    Text(String),
}

// === Messages ===

/// A message to an agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Message {
    /// The unique identifier of this message.
    pub id: MessageId,
    /// The message mode.
    pub mode: MessageMode,
    /// The message content.
    pub content: Vec<ContentBlock>,
}

/// The ID of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(u128);

/// The mode of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageMode {
    /// The message should steer ongoing turns.
    ///
    /// This will cause the message to be added to the conversation at the soonest point possible.
    /// Multiple steering messages can be added at once.
    Steer,
    /// The message should wait until the current turn finishes.
    ///
    /// This will cause the message to be added to the conversation after the current turn finishes.
    /// Only one queued message will be added at a time. Others will wait for their turn.
    /// Outstanding async tool calls do not delay the next turn.
    Queue,
}

// === Compaction ===

/// A compaction of an agent's model context.
///
/// This replaces model input through the given cursor. The original events remain in the log.
/// Retained input is not new conversation activity and must not be displayed or executed again.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Compaction {
    /// The last event included in this compaction.
    pub last_event: EventCursor,
    /// The input that replaces the compacted context.
    pub input: Vec<ModelInput>,
    /// All unresolved async tool calls from before this compaction.
    pub pending_tool_calls: Vec<ToolCall>,
    /// How many tokens were used for compaction.
    pub usage: ModelUsage,
}

// === Turns ===

/// The end of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnEnd {
    /// The cursor of the event that ended the turn.
    pub cursor: EventCursor,
    /// Why the turn ended.
    pub reason: TurnEndReason,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnEndReason {
    /// The agent finished responding.
    Completed,
    /// The turn was interrupted.
    Interrupted,
}

// === Events ===

/// An immutable agent event.
///
/// Events form an event log which contains the full history of everything an agent sees and does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Event {
    /// The cursor pointing to this event's position in the event log.
    pub cursor: EventCursor,
    /// The payload of this event.
    pub payload: EventPayload,
}

/// A cursor into an event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventCursor(u128);

/// An agent event payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventPayload {
    /// Input from the agent's parent.
    Input(MessageId, Vec<ContentBlock>),
    /// Response from the agent.
    ModelResponse(ModelResponse),
    /// Result of a tool called by the agent.
    ToolResult(ToolResult),
    /// Replacement model context and pending tool calls.
    Compaction(Compaction),
    /// The end of a turn.
    TurnEnd(TurnEndReason),
}

// === Wakes ===

/// A wake event from a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Wake {
    /// The agent may have work to process.
    Reconcile(AgentId),
    /// The agent got an interrupt.
    ///
    /// Interrupts should complete all pending tool calls with an output informing the agent of what
    /// happened. All in-flight model requests should also be aborted.
    ///
    /// When there are queued messages for the agent, it will become active again immediately after
    /// the interrupt in order to process them.
    Interrupt(AgentId),
    /// The agent claim should be released.
    ///
    /// This is only relevant for stores which partition agents between wake consumers. Releasing a
    /// claim on an agent would happen in the case of a repartition.
    ///
    /// The consumer should abort any in-flight work for the agent and assume it won't see any more
    /// wakes for it unless it gets repartitioned back.
    Release(AgentId),
    /// The store has delivered all pending wakes to the consumer.
    ///
    /// The store can still deliver future wakes when there is new activity.
    Idle,
}

// === Macros ===

macro_rules! impl_newtype {
    ($wrapper:ty, String) => {
        impl_newtype!(@common $wrapper, String);

        impl From<&str> for $wrapper {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $wrapper {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };

    ($wrapper:ty, $inner:ty) => {
        impl_newtype!(@common $wrapper, $inner);
    };

   (@common $wrapper:ty, $inner:ty) => {
       impl From<$inner> for $wrapper {
           fn from(value: $inner) -> Self {
               Self(value)
           }
       }

       impl From<$wrapper> for $inner {
           fn from(value: $wrapper) -> Self {
               value.0
           }
       }

       impl fmt::Display for $wrapper {
           fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
               fmt::Display::fmt(&self.0, f)
           }
       }
   };
}

// === Impls ===

impl_newtype!(AgentId, u128);
impl_newtype!(RoleName, String);
impl_newtype!(RoleParameterName, String);
impl_newtype!(ModelName, String);
impl_newtype!(ToolName, String);
impl_newtype!(ToolAlias, String);
impl_newtype!(ToolCallId, String);
impl_newtype!(PromptName, String);
impl_newtype!(MessageId, u128);
impl_newtype!(EventCursor, u128);
