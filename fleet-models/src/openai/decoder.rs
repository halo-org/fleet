use super::api;
use crate::OpenAiError;
use fleet_core::{
    ModelOutput, ModelProvenance, ModelProviderToolCall, ModelReasoning, ModelResponse,
    ModelResponseChunk, ModelTool, ModelUsage, ModelsErrorKind, ToolCall,
};
use serde::Deserialize as _;
use serde_json::Value;
use std::collections::BTreeMap;

pub enum Step {
    Chunk(ModelResponseChunk),
    Continue,
    Skip,
}

enum Start {
    Text,
    Reasoning,
    ToolCall(String, String),
    ProviderToolCall(String),
}

pub struct Decoder<'a> {
    tools: &'a [ModelTool],
    provenance: ModelProvenance,
    store: bool,
    started: Vec<Start>,
    output: BTreeMap<u32, ModelOutput>,
    raw_output: BTreeMap<u32, Value>,
    offset: u32,
    last_phase: Option<api::Phase>,
    has_tool_call: bool,
    output_tokens: u32,
}

impl<'a> Decoder<'a> {
    pub const fn new(tools: &'a [ModelTool], provenance: ModelProvenance, store: bool) -> Self {
        Self {
            tools,
            provenance,
            store,
            started: Vec::new(),
            output: BTreeMap::new(),
            raw_output: BTreeMap::new(),
            offset: 0,
            last_phase: None,
            has_tool_call: false,
            output_tokens: 0,
        }
    }

    pub fn decode(&mut self, event: api::Event) -> Result<Step, OpenAiError> {
        let chunk = match event {
            api::Event::OutputAdded { output_index, item } => self.start(output_index, item)?,
            api::Event::TextDelta {
                output_index,
                delta,
            } => {
                let index = self.index(output_index)?;
                if !matches!(self.pending(index)?, Start::Text) {
                    return Err(protocol("text delta does not belong to a text block"));
                }
                ModelResponseChunk::TextDelta(index, delta)
            }
            api::Event::ReasoningDelta {
                output_index,
                delta,
            } => {
                let index = self.index(output_index)?;
                if !matches!(self.pending(index)?, Start::Reasoning) {
                    return Err(protocol(
                        "reasoning delta does not belong to a reasoning block",
                    ));
                }
                ModelResponseChunk::ReasoningDelta(index, delta)
            }
            api::Event::ToolInputDelta {
                output_index,
                delta,
            } => {
                let index = self.index(output_index)?;
                if !matches!(self.pending(index)?, Start::ToolCall(..)) {
                    return Err(protocol("tool input delta does not belong to a tool call"));
                }
                ModelResponseChunk::ToolInputDelta(index, delta)
            }
            api::Event::OutputDone { output_index, item } => {
                self.finish_output(output_index, item)?
            }
            api::Event::Completed { response } => return self.finish(response, false),
            api::Event::Incomplete { response } => return self.finish(response, true),
            api::Event::Failed { .. } | api::Event::Error { .. } => {
                return Err(protocol("model response failed"));
            }
            api::Event::Unknown => return Ok(Step::Skip),
        };

        Ok(Step::Chunk(chunk))
    }

    pub fn continuation(&mut self) -> Result<Vec<Value>, OpenAiError> {
        let offset = u32::try_from(self.started.len())
            .map_err(|error| OpenAiError::source("too many model output blocks", error))?;
        if offset == self.offset {
            return Err(protocol("model response made no progress"));
        }

        self.offset = offset;
        self.last_phase = None;
        self.has_tool_call = false;
        Ok(self.raw_output.values().cloned().collect())
    }

    fn index(&self, output_index: u32) -> Result<u32, OpenAiError> {
        self.offset
            .checked_add(output_index)
            .ok_or_else(|| protocol("model output index is too large"))
    }

    fn pending(&self, index: u32) -> Result<&Start, OpenAiError> {
        if self.output.contains_key(&index) {
            return Err(protocol("model output block has already ended"));
        }
        self.started
            .get(index as usize)
            .ok_or_else(|| protocol("model output block has not started"))
    }

    fn start(
        &mut self,
        output_index: u32,
        item: api::Output,
    ) -> Result<ModelResponseChunk, OpenAiError> {
        let index = self.index(output_index)?;
        if index as usize != self.started.len() {
            return Err(protocol("model output blocks are out of order"));
        }

        let (start, chunk) = match item {
            api::Output::Message { .. } => (Start::Text, ModelResponseChunk::TextStart(index)),
            api::Output::Reasoning { .. } => {
                (Start::Reasoning, ModelResponseChunk::ReasoningStart(index))
            }
            api::Output::FunctionCall { call_id, name, .. } => {
                let chunk = ModelResponseChunk::ToolCallStart(
                    index,
                    call_id.clone().into(),
                    name.clone().into(),
                );
                (Start::ToolCall(call_id, name), chunk)
            }
            api::Output::WebSearchCall { id, .. } => {
                let chunk = ModelResponseChunk::ProviderToolCallStart(
                    index,
                    id.clone().into(),
                    "web_search".into(),
                );
                (Start::ProviderToolCall(id), chunk)
            }
        };
        self.started.push(start);
        Ok(chunk)
    }

    fn finish_output(
        &mut self,
        output_index: u32,
        item: Value,
    ) -> Result<ModelResponseChunk, OpenAiError> {
        let index = self.index(output_index)?;
        let output = api::Output::deserialize(&item)
            .map_err(|error| OpenAiError::source("failed to decode model output", error))?;
        let output = match (self.pending(index)?, output) {
            (Start::Text, api::Output::Message { phase, content }) => {
                self.last_phase = phase;
                ModelOutput::Text(
                    content
                        .into_iter()
                        .map(|content| match content {
                            api::Content::OutputText { text } => text,
                            api::Content::Refusal { refusal } => refusal,
                        })
                        .collect(),
                )
            }
            (
                Start::Reasoning,
                api::Output::Reasoning {
                    id,
                    summary,
                    encrypted_content,
                },
            ) => ModelOutput::Reasoning(ModelReasoning {
                content: (!summary.is_empty()).then(|| {
                    summary
                        .into_iter()
                        .map(|api::Summary::SummaryText { text }| text)
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
                redacted_content: encrypted_content,
                attestation: Some(id),
            }),
            (
                Start::ToolCall(id, tool),
                api::Output::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    status,
                },
            ) if *id == call_id && *tool == name => {
                if status
                    .as_deref()
                    .is_some_and(|status| status != "completed")
                {
                    return Err(protocol("model response contains an unfinished tool call"));
                }
                self.has_tool_call = true;
                ModelOutput::ToolCall(ToolCall {
                    id: call_id.into(),
                    is_async: self
                        .tools
                        .iter()
                        .find(|tool| tool.name.as_ref() == name)
                        .is_some_and(|tool| tool.is_async),
                    tool: name.into(),
                    input: serde_json::from_str(&arguments).map_err(|error| {
                        OpenAiError::source("failed to decode tool input", error)
                    })?,
                })
            }
            (
                Start::ProviderToolCall(start_id),
                api::Output::WebSearchCall { id, status, action },
            ) if *start_id == id => {
                let content = match status.as_str() {
                    "completed" => action.as_ref().and_then(web_search_content),
                    "failed" => Some("Web search failed.".into()),
                    _ => {
                        return Err(protocol(
                            "model response contains an unfinished provider tool call",
                        ));
                    }
                };
                ModelOutput::ProviderToolCall(ModelProviderToolCall {
                    call_id: id.into(),
                    name: "web_search".into(),
                    content,
                    payload: item.to_string(),
                })
            }
            _ => return Err(protocol("model output does not match its start")),
        };

        self.raw_output.insert(index, item);
        self.output.insert(index, output.clone());
        Ok(ModelResponseChunk::OutputEnd(index, output))
    }

    fn finish(&mut self, response: api::Response, incomplete: bool) -> Result<Step, OpenAiError> {
        if response.error.is_some()
            || self.output.len() != self.started.len()
            || response.output.as_ref().map(Vec::len)
                != Some(self.started.len().saturating_sub(self.offset as usize))
        {
            return Err(protocol("model response did not finish all output blocks"));
        }
        if incomplete
            && response
                .incomplete_details
                .as_ref()
                .map(|detail| detail.reason.as_str())
                != Some("max_output_tokens")
        {
            return Err(protocol("model response stopped before completion"));
        }

        let mut usage: ModelUsage = response
            .usage
            .ok_or_else(|| protocol("model response has no token usage"))?
            .into();
        self.output_tokens = self.output_tokens.saturating_add(usage.output);
        usage.output = self.output_tokens;

        if !self.has_tool_call && (incomplete || self.last_phase == Some(api::Phase::Commentary)) {
            return Ok(Step::Continue);
        }

        let continuation_token = if self.store {
            Some(
                response
                    .id
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| protocol("stored model response has no ID"))?,
            )
        } else {
            None
        };
        Ok(Step::Chunk(ModelResponseChunk::ResponseEnd(
            ModelResponse {
                output: core::mem::take(&mut self.output).into_values().collect(),
                usage,
                provenance: self.provenance.clone(),
                continuation_token,
            },
        )))
    }
}

fn web_search_content(action: &Value) -> Option<String> {
    match action.get("type")?.as_str()? {
        "search" => {
            let queries = action
                .get("queries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            let query = if queries.is_empty() {
                action.get("query")?.as_str()?.to_owned()
            } else {
                queries.join("; ")
            };
            Some(format!("Search: {query}"))
        }
        "open_page" => Some(format!("Open: {}", action.get("url")?.as_str()?)),
        "find_in_page" => Some(format!(
            "Find {} in {}",
            action.get("pattern")?.as_str()?,
            action.get("url")?.as_str()?,
        )),
        _ => None,
    }
}

const fn protocol(message: &'static str) -> OpenAiError {
    OpenAiError::new(ModelsErrorKind::Other, message)
}
