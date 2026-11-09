use fleet_core::{JsonObject, ModelUsage, ModelsErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub effort: Option<String>,
    pub speed: Option<String>,
    pub tools: Option<Vec<ProviderTool>>,
}

#[derive(Serialize)]
pub struct Request {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    pub store: bool,
    pub stream: bool,
    pub include: [&'static str; 1],
}

#[derive(Serialize)]
pub struct Reasoning {
    pub effort: String,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Tool {
    Function(FunctionTool),
    Provider(ProviderTool),
}

#[derive(Serialize)]
pub struct FunctionTool {
    pub r#type: &'static str,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    pub strict: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderTool {
    WebSearch {
        #[serde(flatten)]
        config: JsonObject,
    },
}

#[derive(Serialize)]
pub struct CompactRequest {
    pub model: String,
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
}

#[derive(Deserialize)]
pub struct CompactResponse {
    pub output: Vec<Value>,
    pub usage: Usage,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compaction {
    pub version: u32,
    pub output: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Output {
    Message {
        #[serde(default)]
        phase: Option<Phase>,
        content: Vec<Content>,
    },
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<Summary>,
        encrypted_content: Option<String>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
        status: Option<String>,
    },
    WebSearchCall {
        id: String,
        status: String,
        action: Option<Value>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Commentary,
    FinalAnswer,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    OutputText { text: String },
    Refusal { refusal: String },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Summary {
    SummaryText { text: String },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "response.output_item.added")]
    OutputAdded { output_index: u32, item: Output },
    #[serde(
        rename = "response.output_text.delta",
        alias = "response.refusal.delta"
    )]
    TextDelta { output_index: u32, delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningDelta { output_index: u32, delta: String },
    #[serde(rename = "response.function_call_arguments.delta")]
    ToolInputDelta { output_index: u32, delta: String },
    #[serde(rename = "response.output_item.done")]
    OutputDone { output_index: u32, item: Value },
    #[serde(rename = "response.completed")]
    Completed { response: Response },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: Response },
    #[serde(rename = "response.failed")]
    Failed { response: Response },
    #[serde(rename = "error")]
    Error {
        #[serde(flatten)]
        detail: ApiError,
        error: Option<ApiError>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub struct Response {
    pub id: Option<String>,
    pub usage: Option<Usage>,
    pub error: Option<ApiError>,
    pub incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
pub struct IncompleteDetails {
    pub reason: String,
}

#[derive(Default, Deserialize)]
pub struct ApiError {
    pub code: Option<String>,
    pub message: Option<String>,
    pub param: Option<String>,
}

impl ApiError {
    pub fn kind(&self, has_continuation: bool, has_compaction: bool) -> ModelsErrorKind {
        match self.code.as_deref() {
            Some("context_length_exceeded" | "context_window_exceeded") => {
                ModelsErrorKind::ContextTooLong
            }
            Some("previous_response_not_found" | "response_not_found") if has_continuation => {
                ModelsErrorKind::InvalidContinuation
            }
            Some("invalid_encrypted_content" | "invalid_compaction") if has_compaction => {
                ModelsErrorKind::InvalidCompaction
            }
            _ if has_continuation && self.param.as_deref() == Some("previous_response_id") => {
                ModelsErrorKind::InvalidContinuation
            }
            _ => ModelsErrorKind::Other,
        }
    }
}

#[derive(Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub input_tokens_details: InputTokensDetails,
}

#[derive(Default, Deserialize)]
pub struct InputTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
}

impl From<Usage> for ModelUsage {
    fn from(usage: Usage) -> Self {
        Self {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.input_tokens_details.cached_tokens,
            cache_write: usage.input_tokens_details.cache_write_tokens,
        }
    }
}
