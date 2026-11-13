use super::api;
use crate::OpenAiError;
use fleet_core::{ContentBlock, ModelInput, ModelOutput, ModelProvenance, ModelsErrorKind};
use serde_json::{Value, json};

pub fn encode(
    input: &[ModelInput],
    provenance: &ModelProvenance,
) -> Result<Vec<Value>, OpenAiError> {
    let mut items = Vec::new();

    for input in input {
        match input {
            ModelInput::User(content) => {
                let content = encode_content(content);
                items.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": content,
                }));
            }
            ModelInput::Assistant(source, output) => {
                items.extend(encode_output(output, source == provenance)?);
            }
            ModelInput::ToolResult(result) => {
                let mut content = encode_content(&result.output.content);
                if let Some(structured) = &result.output.structured_content {
                    let text = serde_json::to_string(structured).map_err(|error| {
                        OpenAiError::source("failed to encode tool output", error)
                    })?;
                    content.push(json!({
                        "type": "input_text",
                        "text": text,
                    }));
                }
                if result.output.is_error {
                    content.insert(
                        0,
                        json!({
                            "type": "input_text",
                            "text": "Tool returned an error:",
                        }),
                    );
                }
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": result.call_id.as_ref(),
                    "output": content,
                }));
            }
            ModelInput::Compaction(source, compaction) => {
                if let Some(redacted) = &compaction.redacted_content {
                    let state = (source == provenance)
                        .then(|| serde_json::from_str::<api::Compaction>(redacted).ok())
                        .flatten()
                        .filter(|state| state.version == 1 && !state.output.is_empty())
                        .ok_or_else(|| {
                            OpenAiError::new(
                                ModelsErrorKind::InvalidCompaction,
                                "compaction is incompatible with this model",
                            )
                        })?;
                    items.extend(state.output);
                } else if let Some(content) = &compaction.content {
                    items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                } else {
                    return Err(OpenAiError::new(
                        ModelsErrorKind::InvalidCompaction,
                        "compaction has no usable content",
                    ));
                }
            }
        }
    }

    Ok(items)
}

fn encode_content(content: &[ContentBlock]) -> Vec<Value> {
    let mut encoded = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text(text) => encoded.push(json!({
                "type": "input_text",
                "text": text,
            })),
            ContentBlock::Image(id) => {
                encoded.push(json!({
                    "type": "input_text",
                    "text": format!("Image reference: {id}"),
                }));
                encoded.push(json!({
                    "type": "input_image",
                    "image_url": format!("fleet-blob:{id}"),
                }));
            }
        }
    }
    encoded
}

fn encode_output(output: &[ModelOutput], compatible: bool) -> Result<Vec<Value>, OpenAiError> {
    let mut items = Vec::new();
    let has_tool_call = output
        .iter()
        .any(|output| matches!(output, ModelOutput::ToolCall(_)));

    for (index, item) in output.iter().enumerate() {
        match item {
            ModelOutput::Text(text) => {
                let phase = if !has_tool_call && index.saturating_add(1) == output.len() {
                    "final_answer"
                } else {
                    "commentary"
                };
                items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "phase": phase,
                    "content": [{
                        "type": "output_text",
                        "text": text,
                    }],
                }));
            }
            ModelOutput::Reasoning(reasoning) if compatible => {
                if let Some(id) = &reasoning.attestation {
                    let summary: Vec<_> = reasoning
                        .content
                        .iter()
                        .map(|text| {
                            json!({
                                "type": "summary_text",
                                "text": text,
                            })
                        })
                        .collect();
                    let mut item = serde_json::Map::from_iter([
                        ("type".into(), "reasoning".into()),
                        ("id".into(), id.clone().into()),
                        ("summary".into(), summary.into()),
                    ]);
                    if let Some(encrypted) = &reasoning.redacted_content {
                        item.insert("encrypted_content".into(), encrypted.clone().into());
                    }
                    items.push(item.into());
                }
            }
            ModelOutput::ToolCall(call) => {
                let mut item = serde_json::Map::from_iter([
                    ("type".into(), "function_call".into()),
                    ("call_id".into(), call.id.as_ref().into()),
                    ("name".into(), call.tool.as_ref().into()),
                    (
                        "arguments".into(),
                        Value::Object(call.input.clone()).to_string().into(),
                    ),
                    ("status".into(), "completed".into()),
                ]);
                if call.is_async {
                    item.insert("async".into(), true.into());
                }
                items.push(item.into());
            }
            ModelOutput::ProviderToolCall(call) if compatible => {
                let item: Value = serde_json::from_str(&call.payload).map_err(|error| {
                    OpenAiError::source("failed to decode provider tool call", error)
                })?;
                if call.name.as_ref() != "web_search"
                    || item.get("type").and_then(Value::as_str) != Some("web_search_call")
                    || item.get("id").and_then(Value::as_str) != Some(call.call_id.as_ref())
                    || !matches!(
                        item.get("status").and_then(Value::as_str),
                        Some("completed" | "failed")
                    )
                {
                    return Err(OpenAiError::new(
                        ModelsErrorKind::Other,
                        "invalid provider tool call",
                    ));
                }
                items.push(item);
            }
            ModelOutput::Reasoning(_) | ModelOutput::ProviderToolCall(_) => {}
        }
    }

    Ok(items)
}
