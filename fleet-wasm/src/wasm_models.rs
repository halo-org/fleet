use crate::{
    WasmEnvironment, WasmError, WasmState, WasmWorkers,
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
    Model, ModelCompactionResponse, ModelContext, ModelContinuation, ModelInput, ModelResponseChunk,
};
use futures_channel::{mpsc, oneshot};
use futures_util::{Sink as _, Stream, StreamExt as _, future, stream};
use wasmtime::{
    StoreContextMut,
    component::{InstancePre, Source, StreamConsumer, StreamResult},
};

/// A WebAssembly model provider.
///
/// A response stream keeps its instance until the response ends or the stream is dropped.
#[derive(Clone)]
pub struct WasmModels {
    blobs: Option<crate::WasmBlobs>,
    instance: ModelProviderPre<WasmState>,
    environment: WasmEnvironment,
    workers: Option<WasmWorkers>,
}

struct ResponseConsumer {
    sender: mpsc::Sender<Result<ModelResponseChunk, WasmError>>,
    done: Option<oneshot::Sender<()>>,
}

impl WasmModels {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        environment: WasmEnvironment,
        workers: Option<WasmWorkers>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            blobs: None,
            instance: ModelProviderPre::new(instance).map_err(WasmError::runtime)?,
            environment,
            workers,
        })
    }

    pub(crate) fn set_blobs(&mut self, blobs: Option<crate::WasmBlobs>) {
        self.blobs = blobs;
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Model>, WasmError> {
        let mut store = WasmState::new(self.environment.clone(), self.workers.clone())
            .into_store(self.instance.engine());
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let model = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_models()
                    .call_get(accessor, name.into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_model)?;

        model.map(TryInto::try_into).transpose()
    }

    pub(crate) fn generate(
        &self,
        name: &str,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
    ) -> impl Stream<Item = Result<ModelResponseChunk, WasmError>> + Send + use<> {
        let instance = self.instance.clone();
        let environment = self.environment.clone();
        let workers = self.workers.clone();
        let context = types::ModelContext::new(name, context);
        let blobs = self.blobs.as_ref().map(crate::WasmBlobs::for_model);
        let continuation = continuation.map(types::ModelContinuation::from);
        let input = input.iter().map(types::ModelInput::from).collect();
        let (sender, receiver) = mpsc::channel(0);
        let run = async move {
            let mut store = WasmState::new(environment, workers)
                .with_blobs(blobs)
                .into_store(instance.engine());
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

    pub(crate) async fn compact(
        &self,
        name: &str,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
        instructions: Option<&str>,
    ) -> Result<ModelCompactionResponse, WasmError> {
        let blobs = self.blobs.as_ref().map(crate::WasmBlobs::for_model);
        let mut store = WasmState::new(self.environment.clone(), self.workers.clone())
            .with_blobs(blobs)
            .into_store(self.instance.engine());
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
                        types::ModelContext::new(name, context),
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
