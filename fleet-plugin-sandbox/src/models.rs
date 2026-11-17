use crate::{
    ContentBlock, ModelCompaction, ModelInput, ModelOutput, ModelProvenance, ModelsError,
    ModelsErrorKind, Sandbox,
    bindings::{
        self,
        exports::fleet::providers::models::Guest,
        fleet::providers::{
            credentials::{self, Credential},
            types::{
                Model, ModelCompactionResponse, ModelContext, ModelContinuation, ModelReasoning,
                ModelResponse, ModelResponseChunk, ModelUsage, ToolCall,
            },
        },
        wasi::{clocks::monotonic_clock, random::random},
    },
    script::Step,
};
use wit_bindgen::{StreamReader, StreamWriter};

const MODEL: &str = "scripted";

type Writer = StreamWriter<Result<ModelResponseChunk, ModelsError>>;

impl Guest for Sandbox {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Model>, ModelsError> {
        Ok((name == MODEL).then(|| Model {
            name,
            context_window: 1_000_000,
            compaction_threshold: 800_000,
            extensions: serde_json::json!({
                "fleet.title": "Scripted",
                "fleet.description": "Follows the script in the latest user message.",
            })
            .to_string(),
        }))
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn generate(
        context: ModelContext,
        continuation: Option<ModelContinuation>,
        input: Vec<ModelInput>,
    ) -> StreamReader<Result<ModelResponseChunk, ModelsError>> {
        let (mut writer, reader) = bindings::wit_stream::new();
        drop(wit_bindgen::spawn_local(async move {
            if continuation.is_some() {
                drop(writer.write_one(Err(unsupported_continuation())).await);
                return;
            }
            let step = crate::script::next(&input);
            pause(step.delay_ms).await;
            if let Some(failure) = step.error {
                let error = ModelsError {
                    kind: failure.kind.into(),
                    message: failure.message,
                };
                drop(writer.write_one(Err(error)).await);
                return;
            }
            respond(&context, &input, step, &mut writer).await;
        }));
        reader
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn compact(
        context: ModelContext,
        continuation: Option<ModelContinuation>,
        input: Vec<ModelInput>,
        _instructions: Option<String>,
    ) -> Result<ModelCompactionResponse, ModelsError> {
        if continuation.is_some() {
            return Err(unsupported_continuation());
        }
        let position = crate::script::position(&input)
            .map(|position| serde_json::to_string(&position).expect("positions serialize"));
        let compaction = ModelCompaction {
            content: Some(format!("Compacted {} inputs.", input.len())),
            redacted_content: position,
            attestation: None,
        };
        Ok(ModelCompactionResponse {
            usage: usage(&input, 0),
            input: vec![ModelInput::Compaction((provenance(&context), compaction))],
        })
    }
}

/// Streams the step's outputs and ends the response, unless the reader is dropped.
async fn respond(
    context: &ModelContext,
    input: &[ModelInput],
    step: Step,
    writer: &mut Writer,
) -> Option<()> {
    let mut text = step.text;
    if step.credential || step.refresh {
        let mut notes = Vec::from_iter(text.take());
        if step.refresh
            && let Some(note) = refresh().await
        {
            notes.push(note);
        }
        notes.push(credential().await);
        text = Some(notes.join("\n\n"));
    }
    let delay_ms = step.delay_ms;
    let mut output = Vec::new();
    if let Some(reasoning) = step.reasoning {
        output.push(stream_reasoning(writer, index(&output), reasoning, delay_ms).await?);
    }
    if let Some(text) = text {
        output.push(stream_text(writer, index(&output), text, delay_ms).await?);
    }
    for call in step.tool_calls {
        pause(delay_ms).await;
        let call = ToolCall {
            id: format!("call_{:016x}", random::get_random_u64()),
            is_async: context
                .tools
                .iter()
                .any(|tool| tool.name == call.tool && tool.is_async),
            tool: call.tool,
            input: serde_json::Value::Object(call.input).to_string(),
        };
        output.push(stream_call(writer, index(&output), call).await?);
    }
    let response = ModelResponse {
        usage: usage(input, size(&output)),
        output,
        provenance: provenance(context),
        continuation_token: None,
    };
    send(writer, ModelResponseChunk::ResponseEnd(response)).await
}

async fn stream_reasoning(
    writer: &mut Writer,
    index: u32,
    reasoning: String,
    delay_ms: u64,
) -> Option<ModelOutput> {
    send(writer, ModelResponseChunk::ReasoningStart(index)).await?;
    for piece in reasoning.split_inclusive(' ') {
        pause(delay_ms).await;
        send(
            writer,
            ModelResponseChunk::ReasoningDelta((index, piece.into())),
        )
        .await?;
    }
    let reasoning = ModelOutput::Reasoning(ModelReasoning {
        content: Some(reasoning),
        redacted_content: None,
        attestation: None,
    });
    send(
        writer,
        ModelResponseChunk::OutputEnd((index, reasoning.clone())),
    )
    .await?;
    Some(reasoning)
}

async fn stream_text(
    writer: &mut Writer,
    index: u32,
    text: String,
    delay_ms: u64,
) -> Option<ModelOutput> {
    send(writer, ModelResponseChunk::TextStart(index)).await?;
    for piece in text.split_inclusive(' ') {
        pause(delay_ms).await;
        send(writer, ModelResponseChunk::TextDelta((index, piece.into()))).await?;
    }
    let text = ModelOutput::Text(text);
    send(writer, ModelResponseChunk::OutputEnd((index, text.clone()))).await?;
    Some(text)
}

async fn stream_call(writer: &mut Writer, index: u32, call: ToolCall) -> Option<ModelOutput> {
    let start = (index, call.id.clone(), call.tool.clone());
    send(writer, ModelResponseChunk::ToolCallStart(start)).await?;
    let delta = (index, call.input.clone());
    send(writer, ModelResponseChunk::ToolInputDelta(delta)).await?;
    let call = ModelOutput::ToolCall(call);
    send(writer, ModelResponseChunk::OutputEnd((index, call.clone()))).await?;
    Some(call)
}

/// Rejects the current OAuth token, returning a note unless it was refreshed.
async fn refresh() -> Option<String> {
    match credentials::get(crate::CONNECTION.into()).await {
        Ok(Some(Credential::Oauth(credential))) => {
            match credentials::refresh(crate::CONNECTION.into(), credential.access_token).await {
                Ok(true) => None,
                Ok(false) => Some("The OAuth token was not refreshed.".into()),
                Err(error) => Some(format!("Could not refresh the OAuth token: {error}")),
            }
        }
        Ok(_) => Some("There is no OAuth token to refresh.".into()),
        Err(error) => Some(format!("Could not read the credential: {error}")),
    }
}

async fn credential() -> String {
    match credentials::get(crate::CONNECTION.into()).await {
        Ok(None) => "Not signed in.".into(),
        Ok(Some(Credential::ApiKey(key))) => format!("Signed in with API key {key}."),
        Ok(Some(Credential::Oauth(credential))) => {
            format!("Signed in with OAuth token {}.", credential.access_token)
        }
        Err(error) => format!("Could not read the credential: {error}"),
    }
}

async fn send(writer: &mut Writer, chunk: ModelResponseChunk) -> Option<()> {
    writer.write_one(Ok(chunk)).await.is_none().then_some(())
}

async fn pause(delay_ms: u64) {
    if delay_ms > 0 {
        monotonic_clock::wait_for(delay_ms.saturating_mul(1_000_000)).await;
    }
}

fn index(output: &[ModelOutput]) -> u32 {
    u32::try_from(output.len()).unwrap_or(u32::MAX)
}

fn unsupported_continuation() -> ModelsError {
    ModelsError {
        kind: ModelsErrorKind::InvalidContinuation,
        message: "the scripted model does not issue continuations".into(),
    }
}

fn provenance(context: &ModelContext) -> ModelProvenance {
    ModelProvenance {
        provider_id: crate::PROVIDER.into(),
        model_id: context.model.name.clone(),
    }
}

/// Counts bytes of text as tokens.
fn usage(input: &[ModelInput], output: usize) -> ModelUsage {
    let input = input
        .iter()
        .map(|input| match input {
            ModelInput::User(content) => blocks(content),
            ModelInput::Assistant((_, output)) => size(output),
            ModelInput::ToolResult(result) => blocks(&result.output.content),
            ModelInput::Compaction((_, compaction)) => {
                compaction.content.as_ref().map_or(0, String::len)
            }
        })
        .fold(0, usize::saturating_add);
    ModelUsage {
        input: u32::try_from(input).unwrap_or(u32::MAX),
        output: u32::try_from(output).unwrap_or(u32::MAX),
        cache_read: 0,
        cache_write: 0,
    }
}

fn size(output: &[ModelOutput]) -> usize {
    output
        .iter()
        .map(|output| match output {
            ModelOutput::Text(text) => text.len(),
            ModelOutput::Reasoning(reasoning) => reasoning.content.as_ref().map_or(0, String::len),
            ModelOutput::ToolCall(call) => call.input.len(),
            ModelOutput::ProviderToolCall(call) => call.payload.len(),
        })
        .fold(0, usize::saturating_add)
}

fn blocks(content: &[ContentBlock]) -> usize {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.len(),
            ContentBlock::Image(_) => 0,
        })
        .fold(0, usize::saturating_add)
}
