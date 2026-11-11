use crate::{
    Config, HostBlobs, HostCredentials, ModelsError, OpenAi, WasiHttp, bindings,
    bindings::{
        exports::fleet::providers::models::Guest,
        fleet::providers::{configuration, types},
    },
};
use core::pin::pin;
use fleet_core::Models as _;
use fleet_models::OpenAiModels;
use futures_util::StreamExt as _;
use wit_bindgen::{StreamReader, StreamWriter};

impl Guest for OpenAi {
    async fn get(name: String) -> Result<Option<types::Model>, ModelsError> {
        provider()?
            .get(&name.into())
            .await
            .map(|model| model.map(Into::into))
            .map_err(ModelsError::from)
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn generate(
        context: types::ModelContext,
        continuation: Option<types::ModelContinuation>,
        input: Vec<types::ModelInput>,
    ) -> StreamReader<Result<types::ModelResponseChunk, ModelsError>> {
        let (mut writer, reader) = bindings::wit_stream::new();
        drop(wit_bindgen::spawn_local(async move {
            if let Err(error) = generate(context, continuation, input, &mut writer).await {
                drop(writer.write_one(Err(error)).await);
            }
        }));
        reader
    }

    async fn compact(
        context: types::ModelContext,
        continuation: Option<types::ModelContinuation>,
        input: Vec<types::ModelInput>,
        instructions: Option<String>,
    ) -> Result<types::ModelCompactionResponse, ModelsError> {
        let provider = provider()?;
        let context = context.try_into().map_err(crate::error)?;
        let continuation = continuation.map(Into::into);
        let input = input
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, _>>()
            .map_err(crate::error)?;
        provider
            .compact(
                &context,
                continuation.as_ref(),
                &input,
                instructions.as_deref(),
            )
            .await
            .map(Into::into)
            .map_err(ModelsError::from)
    }
}

fn provider() -> Result<OpenAiModels<WasiHttp, HostCredentials, HostBlobs>, ModelsError> {
    let config: Config = serde_json::from_str(&configuration::get()).map_err(crate::error)?;
    OpenAiModels::new(WasiHttp, HostCredentials, config.into())
        .map(|models| models.with_blobs(HostBlobs))
        .map_err(ModelsError::from)
}

async fn generate(
    context: types::ModelContext,
    continuation: Option<types::ModelContinuation>,
    input: Vec<types::ModelInput>,
    writer: &mut StreamWriter<Result<types::ModelResponseChunk, ModelsError>>,
) -> Result<(), ModelsError> {
    let provider = provider()?;
    let context = context.try_into().map_err(crate::error)?;
    let continuation = continuation.map(Into::into);
    let input = input
        .into_iter()
        .map(TryInto::try_into)
        .collect::<Result<Vec<_>, _>>()
        .map_err(crate::error)?;
    let mut stream = pin!(provider.generate(&context, continuation.as_ref(), &input));
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map(Into::into).map_err(ModelsError::from)?;
        if writer.write_one(Ok(chunk)).await.is_some() {
            break;
        }
    }
    Ok(())
}
