use fleet_core::{ModelOutput, ModelProviderToolName, ModelResponseChunk, ToolAlias, ToolCallId};
use std::collections::BTreeMap;

pub struct PreviewState {
    pub input_cursor: u128,
    pub preview: Option<Preview>,
    pub next_sequence: Option<u32>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Preview {
    Response(BTreeMap<u32, PreviewContent>),
    Compaction,
}

#[derive(Clone, PartialEq, Eq)]
pub enum PreviewContent {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: ToolCallId,
        tool: ToolAlias,
        input: String,
    },
    ProviderToolCall {
        call_id: ToolCallId,
        name: ModelProviderToolName,
        content: String,
    },
}

impl PreviewState {
    pub const fn new(input_cursor: u128) -> Self {
        Self {
            input_cursor,
            preview: Some(Preview::Response(BTreeMap::new())),
            next_sequence: Some(0),
        }
    }

    pub fn invalidate(&mut self) {
        self.preview = None;
        self.next_sequence = None;
    }

    pub fn start_compaction(&mut self) -> bool {
        if self.preview == Some(Preview::Compaction) {
            return false;
        }
        self.preview = Some(Preview::Compaction);
        true
    }

    pub fn apply(&mut self, sequence: u32, chunk: &ModelResponseChunk) -> bool {
        if self.next_sequence != Some(sequence) {
            let changed = self.preview.is_some();
            self.invalidate();
            return changed;
        }
        let Some(next) = sequence.checked_add(1) else {
            self.invalidate();
            return true;
        };
        self.next_sequence = Some(next);
        let replaced = self.preview == Some(Preview::Compaction);
        if replaced {
            self.preview = Some(Preview::Response(BTreeMap::new()));
        }
        let Some(Preview::Response(output)) = &mut self.preview else {
            return false;
        };
        let (index, content) = match chunk {
            ModelResponseChunk::TextStart(index) => (*index, PreviewContent::Text(String::new())),
            ModelResponseChunk::ReasoningStart(index) => {
                (*index, PreviewContent::Reasoning(String::new()))
            }
            ModelResponseChunk::ToolCallStart(index, id, tool) => (
                *index,
                PreviewContent::ToolCall {
                    id: id.clone(),
                    tool: tool.clone(),
                    input: String::new(),
                },
            ),
            ModelResponseChunk::ProviderToolCallStart(index, call_id, name) => (
                *index,
                PreviewContent::ProviderToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: String::new(),
                },
            ),
            ModelResponseChunk::TextDelta(index, text)
            | ModelResponseChunk::ReasoningDelta(index, text)
            | ModelResponseChunk::ToolInputDelta(index, text) => {
                let ((ModelResponseChunk::TextDelta(..), Some(PreviewContent::Text(value)))
                | (
                    ModelResponseChunk::ReasoningDelta(..),
                    Some(PreviewContent::Reasoning(value)),
                )
                | (
                    ModelResponseChunk::ToolInputDelta(..),
                    Some(PreviewContent::ToolCall { input: value, .. }),
                )) = (chunk, output.get_mut(index))
                else {
                    self.invalidate();
                    return true;
                };
                value.push_str(text);
                return !text.is_empty();
            }
            ModelResponseChunk::OutputEnd(index, value) => (*index, value.into()),
            ModelResponseChunk::ResponseEnd(response) => {
                let replacement = (0..).zip(response.output.iter().map(Into::into)).collect();
                if *output == replacement {
                    return replaced;
                }
                *output = replacement;
                return true;
            }
        };
        if output.get(&index) == Some(&content) {
            return false;
        }
        output.insert(index, content);
        true
    }
}

impl From<&ModelOutput> for PreviewContent {
    fn from(output: &ModelOutput) -> Self {
        match output {
            ModelOutput::Text(text) => Self::Text(text.clone()),
            ModelOutput::Reasoning(reasoning) => {
                Self::Reasoning(reasoning.content.clone().unwrap_or_default())
            }
            ModelOutput::ToolCall(call) => Self::ToolCall {
                id: call.id.clone(),
                tool: call.tool.clone(),
                input: fleet_core::JsonValue::Object(call.input.clone()).to_string(),
            },
            ModelOutput::ProviderToolCall(call) => Self::ProviderToolCall {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                content: call.content.clone().unwrap_or_default(),
            },
        }
    }
}
