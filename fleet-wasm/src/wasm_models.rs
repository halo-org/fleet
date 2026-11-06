use crate::{
    WasmCapabilities, WasmError, WasmState,
    bindings::{
        models::{ModelProviderPre, exports::fleet::providers::models},
        types,
    },
};
use core::{
    pin::Pin,
    task::{Context, Poll},
};
use fleet_core::{
    Model, ModelCompactionResponse, ModelContext, ModelContinuation, ModelInput, ModelName,
    ModelResponseChunk, Models,
};
use futures_channel::{mpsc, oneshot};
use futures_util::{Sink as _, Stream, StreamExt as _, future, stream};
use wasmtime::{
    Store, StoreContextMut,
    component::{Component, Linker, Source, StreamConsumer, StreamResult},
};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;

/// A WebAssembly model provider.
///
/// A response stream keeps its instance until the response ends or the stream is dropped.
pub struct WasmModels {
    instance: ModelProviderPre<WasmState>,
    filesystem: WasiFilesystemCtx,
}

struct ResponseConsumer {
    sender: mpsc::Sender<Result<ModelResponseChunk, WasmError>>,
    done: Option<oneshot::Sender<()>>,
}

impl WasmModels {
    pub(crate) fn new(
        component: &Component,
        linker: &Linker<WasmState>,
        capabilities: WasmCapabilities,
    ) -> Result<Self, WasmError> {
        let instance = linker
            .instantiate_pre(component)
            .map_err(WasmError::runtime)?;
        Ok(Self {
            instance: ModelProviderPre::new(instance).map_err(WasmError::runtime)?,
            filesystem: capabilities.open_filesystem()?,
        })
    }
}

impl Models for WasmModels {
    type Error = WasmError;

    async fn get(&self, name: &ModelName) -> Result<Option<Model>, Self::Error> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(self.filesystem.clone()),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let model = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_models()
                    .call_get(accessor, name.as_ref().into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_model)?;

        model.map(TryInto::try_into).transpose()
    }

    fn generate(
        &self,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
    ) -> impl Stream<Item = Result<ModelResponseChunk, Self::Error>> + Send {
        let instance = self.instance.clone();
        let filesystem = self.filesystem.clone();
        let context = types::ModelContext::from(context);
        let continuation = continuation.map(types::ModelContinuation::from);
        let input = input.iter().map(types::ModelInput::from).collect();
        let (sender, receiver) = mpsc::channel(0);
        let run = async move {
            let mut store = Store::new(instance.engine(), WasmState::new(filesystem));
            let instance = instance
                .instantiate_async(&mut store)
                .await
                .map_err(WasmError::runtime)?;
            let (done, finished) = oneshot::channel();
            store
                .run_concurrent(async |accessor| {
                    let response = instance
                        .fleet_providers_models()
                        .call_generate(accessor, context, continuation, input)
                        .await
                        .map_err(WasmError::runtime)?;
                    accessor
                        .with(|mut access| {
                            response.pipe(
                                &mut access,
                                ResponseConsumer {
                                    sender,
                                    done: Some(done),
                                },
                            )
                        })
                        .map_err(WasmError::runtime)?;
                    finished
                        .await
                        .map_err(|_| WasmError::incomplete_response())?;
                    Ok::<_, WasmError>(())
                })
                .await
                .map_err(WasmError::runtime)?
        };
        let errors = stream::once(run)
            .filter_map(|result| future::ready(result.err().map(Err)))
            .boxed();
        let output = stream::select(receiver, errors).boxed();

        stream::unfold(Some(output), |output| async move {
            let mut output = output?;
            let chunk = output
                .next()
                .await
                .unwrap_or_else(|| Err(WasmError::incomplete_response()));
            let output = match &chunk {
                Ok(ModelResponseChunk::ResponseEnd(_)) | Err(_) => None,
                Ok(_) => Some(output),
            };
            Some((chunk, output))
        })
    }

    async fn compact(
        &self,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
        instructions: Option<&str>,
    ) -> Result<ModelCompactionResponse, Self::Error> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(self.filesystem.clone()),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let response = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_models()
                    .call_compact(
                        accessor,
                        context.into(),
                        continuation.map(Into::into),
                        input.iter().map(Into::into).collect(),
                        instructions.map(Into::into),
                    )
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_model)?;

        response.try_into()
    }
}

impl StreamConsumer<WasmState> for ResponseConsumer {
    type Item = Result<types::ModelResponseChunk, models::ModelsError>;

    fn poll_consume(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<WasmState>,
        mut source: Source<'_, Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = self.get_mut();
        match Pin::new(&mut this.sender).poll_ready(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(_)) => return Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => return Poll::Pending,
        }

        let mut chunk = None;
        source.read(&mut store, &mut chunk)?;
        if let Some(chunk) = chunk {
            let chunk = chunk
                .map_err(WasmError::from_model)
                .and_then(TryInto::try_into);
            let finished = matches!(&chunk, Ok(ModelResponseChunk::ResponseEnd(_)) | Err(_));
            if Pin::new(&mut this.sender).start_send(chunk).is_err() || finished {
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
        }

        Poll::Ready(Ok(StreamResult::Completed))
    }
}

impl Drop for ResponseConsumer {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done.send(()).unwrap_or_default();
        }
    }
}
