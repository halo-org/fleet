use crate::{
    WasmState,
    bindings::{blobs, types},
};
use core::fmt;
use fleet_core::{AgentId, BlobId, ContentBlock};
use fleet_kit::{Blobs, BlobsErrorKind};
use futures_util::future::BoxFuture;
use std::sync::Arc;
use wasmtime::component::{Accessor, HasSelf};

#[cfg(test)]
mod tests;

/// Blob capabilities available to a WebAssembly provider.
///
/// Model calls receive read-only access to the provider. Tools receive access through the
/// agent factory. Metadata lookups receive no blob access.
#[derive(Clone)]
pub struct WasmBlobs {
    provider: Arc<dyn BlobsObject>,
    agents: Option<Arc<dyn Fn(AgentId) -> Self + Send + Sync>>,
    writable: bool,
}

trait BlobsObject: Send + Sync {
    fn get(
        &self,
        id: BlobId,
    ) -> BoxFuture<'_, Result<Option<blobs::BlobMetadata>, blobs::BlobsError>>;
    fn read(&self, id: BlobId, limit: u64) -> BoxFuture<'_, Result<Vec<u8>, blobs::BlobsError>>;
    fn put(
        &self,
        id: BlobId,
        media_type: String,
        content: Vec<u8>,
    ) -> BoxFuture<'_, Result<blobs::BlobMetadata, blobs::BlobsError>>;
}

impl WasmBlobs {
    /// Wraps a blob provider. This does not grant tools access to the entire provider.
    pub fn new(provider: impl Blobs + Send + Sync + 'static) -> Self {
        Self {
            provider: Arc::new(provider),
            agents: None,
            writable: true,
        }
    }

    /// Supplies a factory for agent-authorized tool access.
    #[must_use]
    pub fn with_agents<B: Blobs + Send + Sync + 'static>(
        mut self,
        factory: impl Fn(AgentId) -> B + Send + Sync + 'static,
    ) -> Self {
        self.agents = Some(Arc::new(move |id| Self::new(factory(id))));
        self
    }

    pub(crate) fn for_model(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            agents: None,
            writable: false,
        }
    }

    pub(crate) fn for_agent(&self, id: AgentId) -> Option<Self> {
        self.agents.as_ref().map(|factory| factory(id))
    }

    pub(crate) async fn validate_content(
        &self,
        content: &[ContentBlock],
    ) -> Result<(), blobs::BlobsError> {
        let mut count = 0_u32;
        let mut size = 0_u64;
        for block in content {
            if let ContentBlock::Image(id) = block {
                let info = self
                    .provider
                    .get(*id)
                    .await?
                    .ok_or_else(|| error(&BlobsErrorKind::NotFound))?;
                if info.image.is_none() {
                    return Err(error(&BlobsErrorKind::InvalidData));
                }
                count = count.saturating_add(1);
                size = size.saturating_add(info.size);
                if count > 20 || size > 40 * 1024 * 1024 {
                    return Err(error(&BlobsErrorKind::TooLarge));
                }
            }
        }
        Ok(())
    }
}

impl fmt::Debug for WasmBlobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmBlobs").finish_non_exhaustive()
    }
}

fn metadata(value: fleet_kit::BlobMetadata) -> blobs::BlobMetadata {
    blobs::BlobMetadata {
        media_type: value.media_type,
        size: value.size,
        image: value.image.map(|image| blobs::ImageDimensions {
            width: image.width,
            height: image.height,
        }),
    }
}

fn error(source: &impl fleet_core::Error<BlobsErrorKind>) -> blobs::BlobsError {
    blobs::BlobsError {
        kind: match source.kind() {
            BlobsErrorKind::NotFound => blobs::BlobsErrorKind::NotFound,
            BlobsErrorKind::Denied => blobs::BlobsErrorKind::Denied,
            BlobsErrorKind::TooLarge => blobs::BlobsErrorKind::TooLarge,
            BlobsErrorKind::InvalidData => blobs::BlobsErrorKind::InvalidData,
            BlobsErrorKind::Conflict => blobs::BlobsErrorKind::Conflict,
            BlobsErrorKind::Other => blobs::BlobsErrorKind::Other,
        },
        message: source.to_string(),
    }
}

impl<B: Blobs + Send + Sync> BlobsObject for B {
    fn get(
        &self,
        id: BlobId,
    ) -> BoxFuture<'_, Result<Option<blobs::BlobMetadata>, blobs::BlobsError>> {
        Box::pin(async move {
            Blobs::get(self, id)
                .await
                .map(|value| value.map(metadata))
                .map_err(|source| error(&source))
        })
    }

    fn read(&self, id: BlobId, limit: u64) -> BoxFuture<'_, Result<Vec<u8>, blobs::BlobsError>> {
        Box::pin(async move {
            Blobs::read(self, id, limit)
                .await
                .map_err(|source| error(&source))
        })
    }

    fn put(
        &self,
        id: BlobId,
        media_type: String,
        content: Vec<u8>,
    ) -> BoxFuture<'_, Result<blobs::BlobMetadata, blobs::BlobsError>> {
        Box::pin(async move {
            Blobs::put(self, id, &media_type, &content)
                .await
                .map(metadata)
                .map_err(|source| error(&source))
        })
    }
}

impl blobs::Host for WasmState {}

impl blobs::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn get(
        store: &Accessor<WasmState, Self>,
        id: types::BlobId,
    ) -> Result<Option<blobs::BlobMetadata>, blobs::BlobsError> {
        let host = host(store)?;
        host.provider.get(id.into()).await
    }

    async fn read(
        store: &Accessor<WasmState, Self>,
        id: types::BlobId,
        limit: u64,
    ) -> Result<Vec<u8>, blobs::BlobsError> {
        let host = host(store)?;
        host.provider
            .read(id.into(), limit.min(10 * 1024 * 1024))
            .await
    }

    async fn put(
        store: &Accessor<WasmState, Self>,
        id: types::BlobId,
        media_type: String,
        content: Vec<u8>,
    ) -> Result<blobs::BlobMetadata, blobs::BlobsError> {
        let host = host(store)?;
        if !host.writable {
            return Err(error(&BlobsErrorKind::Denied));
        }
        if content.len() > 10 * 1024 * 1024 {
            return Err(error(&BlobsErrorKind::TooLarge));
        }
        host.provider.put(id.into(), media_type, content).await
    }
}

fn host(store: &Accessor<WasmState, HasSelf<WasmState>>) -> Result<WasmBlobs, blobs::BlobsError> {
    store
        .with(|mut access| access.get().blobs())
        .ok_or_else(|| error(&BlobsErrorKind::Denied))
}
