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
use serde_json::Value;

#[derive(Serialize)]
struct Request {
    #[serde(flatten)]
    base: api::Request,
    context_management: [ContextManagement; 1],
}

#[derive(Serialize)]
struct ContextManagement {
    r#type: &'static str,
    compact_threshold: u32,
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
        Ok(response) => response,
        Err(error) if error.is_unsupported_route() => stream(client, request, scope).await?,
        Err(error) => return Err(error),
    };
    if response.output.is_empty() {
        return Err(invalid("compaction returned no context"));
    }
    images::retain(&mut response.output, &references, scope.continuation)?;
    let state = api::Compaction {
        version: 1,
        output: response.output,
    };
    let redacted_content = serde_json::to_string(&state)
        .map_err(|error| OpenAiError::source("failed to encode compaction state", error))?;

    Ok(ModelCompactionResponse {
        input: vec![ModelInput::Compaction(
            provenance,
            ModelCompaction {
                content: None,
                redacted_content: Some(redacted_content),
                attestation: None,
            },
        )],
        usage: response.usage.into(),
    })
}

async fn stream<H: Http + Sync, C: Credentials + Sync>(
    client: &Client<H, C>,
    mut request: api::Request,
    scope: ErrorScope,
) -> Result<api::CompactResponse, OpenAiError> {
    if scope.continuation {
        return Err(OpenAiError::new(
            ModelsErrorKind::InvalidContinuation,
            "streaming compaction requires the full input",
        ));
    }
    request.store = false;
    let request = Request {
        base: request,
        context_management: [ContextManagement {
            r#type: "compaction",
            compact_threshold: 1024,
        }],
    };
    let mut events = client.events(&request, scope).await?;
    while let Some(event) = events.next().await {
        match event? {
            api::Event::OutputAdded {
                output_index: 0,
                item,
            } if item.get("type").and_then(Value::as_str) == Some("compaction") => {}
            api::Event::OutputDone {
                output_index: 0,
                item,
            } if item.get("type").and_then(Value::as_str) == Some("compaction") => {
                if item
                    .get("status")
                    .is_some_and(|status| !status.is_null() && status != "completed")
                {
                    return Err(invalid("compaction contains unfinished output"));
                }
                if item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(invalid("compaction has no encrypted content"));
                }
                return Ok(api::CompactResponse {
                    output: vec![item],
                    usage: api::Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                        input_tokens_details: api::InputTokensDetails::default(),
                    },
                });
            }
            api::Event::Unknown => {}
            _ => {
                return Err(invalid(
                    "compaction did not return a checkpoint before other output",
                ));
            }
        }
    }
    Err(invalid("compaction ended before completing a checkpoint"))
}

const fn invalid(message: &'static str) -> OpenAiError {
    OpenAiError::new(ModelsErrorKind::Other, message)
}
