use super::{
    api,
    client::{Client, ErrorScope},
    images,
};
use crate::{Credentials, Http, OpenAiError};
use fleet_core::{
    ModelCompaction, ModelCompactionResponse, ModelInput, ModelProvenance, ModelsErrorKind,
};
use fleet_kit::Blobs;
use futures_util::StreamExt as _;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const INSTRUCTIONS: &str = "Summarize the conversation for another model to continue. Preserve \
    the user's goals, constraints, decisions, completed work, pending work, and important \
    references. Be concise and factual.";

#[derive(Serialize)]
struct Request {
    #[serde(flatten)]
    base: api::Request,
    context_management: [ContextManagement; 1],
    tool_choice: &'static str,
}

#[derive(Serialize)]
struct ContextManagement {
    r#type: &'static str,
    compact_threshold: u32,
}

struct Compaction {
    output: Vec<Value>,
    summary: Option<String>,
    usage: api::Usage,
}

pub async fn compact<H, C, B>(
    client: &Client<H, C>,
    blobs: &B,
    mut request: api::Request,
    provenance: ModelProvenance,
    instructions: Option<&str>,
    scope: ErrorScope,
) -> Result<ModelCompactionResponse, OpenAiError>
where
    H: Http + Sync,
    C: Credentials + Sync,
    B: Blobs + Sync,
    B::Error: Send + Sync,
{
    let references = images::resolve(&mut request.input, blobs).await?;
    let response = client
        .compact(
            &api::CompactRequest {
                model: &request.model,
                input: &request.input,
                instructions: instructions.or(request.instructions.as_deref()),
                previous_response_id: request.previous_response_id.as_deref(),
            },
            scope,
        )
        .await;
    let mut response = match response {
        Ok(response) => Compaction {
            output: response.output,
            summary: None,
            usage: response.usage,
        },
        Err(error) if error.is_unsupported_route() => {
            stream(client, request, instructions, scope).await?
        }
        Err(error) => return Err(error),
    };
    let redacted_content = if response.output.is_empty() {
        if response.summary.is_none() {
            return Err(invalid("compaction returned no context"));
        }
        None
    } else {
        images::retain(&mut response.output, &references, scope.continuation)?;
        let state = api::Compaction {
            version: 1,
            output: response.output,
        };
        Some(
            serde_json::to_string(&state)
                .map_err(|error| OpenAiError::source("failed to encode compaction state", error))?,
        )
    };

    Ok(ModelCompactionResponse {
        input: vec![ModelInput::Compaction(
            provenance,
            ModelCompaction {
                content: response.summary,
                redacted_content,
                attestation: None,
            },
        )],
        usage: response.usage.into(),
    })
}

async fn stream<H: Http + Sync, C: Credentials + Sync>(
    client: &Client<H, C>,
    mut request: api::Request,
    instructions: Option<&str>,
    scope: ErrorScope,
) -> Result<Compaction, OpenAiError> {
    if scope.continuation {
        return Err(OpenAiError::new(
            ModelsErrorKind::InvalidContinuation,
            "streaming compaction requires the full input",
        ));
    }
    let instructions = instructions.unwrap_or(INSTRUCTIONS);
    request.input.push(json!({
        "type": "message",
        "role": "developer",
        "content": format!(
            "For this response only, prepare a context handoff. Do not continue the task or \
             call tools. Do not carry this maintenance request forward as pending work. \
             Subsequent responses should continue the original task.\n\n{instructions}",
        ),
    }));
    request.store = false;
    let request = Request {
        base: request,
        context_management: [ContextManagement {
            r#type: "compaction",
            compact_threshold: 1024,
        }],
        tool_choice: "none",
    };
    let mut events = client.events(&request, scope).await?;
    let mut output = BTreeMap::new();
    let mut pending = BTreeSet::new();
    while let Some(event) = events.next().await {
        match event? {
            api::Event::OutputAdded { output_index, item } => {
                if !matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("message" | "reasoning" | "compaction")
                ) {
                    return Err(invalid("compaction returned unexpected output"));
                }
                if !pending.insert(output_index) || output.contains_key(&output_index) {
                    return Err(invalid("compaction repeated an output block"));
                }
            }
            api::Event::OutputDone { output_index, item } => {
                pending.remove(&output_index);
                if output.insert(output_index, item).is_some() {
                    return Err(invalid("compaction repeated an output block"));
                }
            }
            api::Event::Completed { mut response } => {
                if !pending.is_empty() {
                    return Err(invalid("compaction contains unfinished output"));
                }
                if response.output.as_ref().is_none_or(Vec::is_empty) {
                    response.output = Some(output.into_values().collect());
                }
                return finish(response);
            }
            api::Event::Incomplete { .. }
            | api::Event::Failed { .. }
            | api::Event::Error { .. } => {
                return Err(invalid("compaction stopped before completion"));
            }
            _ => {}
        }
    }
    Err(invalid("compaction ended before completion"))
}

fn finish(response: api::Response) -> Result<Compaction, OpenAiError> {
    if response.error.is_some() {
        return Err(invalid("compaction failed"));
    }
    let usage = response
        .usage
        .ok_or_else(|| invalid("compaction has no token usage"))?;
    let mut output = response
        .output
        .ok_or_else(|| invalid("compaction has no output"))?;
    let mut checkpoint = None;
    let mut text = Vec::new();
    for (index, item) in output.iter().enumerate() {
        if item
            .get("status")
            .is_some_and(|status| !status.is_null() && status != "completed")
        {
            return Err(invalid("compaction contains unfinished output"));
        }
        match item.get("type").and_then(Value::as_str) {
            Some("compaction") => {
                if item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(invalid("compaction has no encrypted content"));
                }
                checkpoint = Some(index);
            }
            Some("reasoning") => {}
            Some("message") if item.get("role").and_then(Value::as_str) == Some("assistant") => {
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("compaction has invalid message content"))?;
                for block in content {
                    if block.get("type").and_then(Value::as_str) != Some("output_text") {
                        return Err(invalid("compaction did not produce a summary"));
                    }
                    let block = block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("compaction has invalid summary text"))?;
                    if !block.trim().is_empty() {
                        text.push(block);
                    }
                }
            }
            _ => return Err(invalid("compaction returned unexpected output")),
        }
    }
    let summary = if let Some(index) = checkpoint {
        output.drain(..index);
        None
    } else {
        if text.is_empty() {
            return Err(invalid("compaction returned no summary"));
        }
        let summary = text.join("\n\n");
        output.clear();
        Some(summary)
    };
    Ok(Compaction {
        output,
        summary,
        usage,
    })
}

const fn invalid(message: &'static str) -> OpenAiError {
    OpenAiError::new(ModelsErrorKind::Other, message)
}
