use crate::bindings::fleet::providers::blobs;
use fleet_core::BlobId;
use fleet_kit::{BlobMetadata, Blobs, BlobsErrorKind, ImageDimensions};

pub struct HostBlobs;

impl Blobs for HostBlobs {
    type Error = blobs::BlobsError;

    async fn get(&self, id: BlobId) -> Result<Option<BlobMetadata>, Self::Error> {
        blobs::get(id.into())
            .await
            .map(|value| value.map(Into::into))
    }

    async fn read(&self, id: BlobId, limit: u64) -> Result<Vec<u8>, Self::Error> {
        blobs::read(id.into(), limit).await
    }

    async fn put(
        &self,
        id: BlobId,
        media_type: &str,
        content: &[u8],
    ) -> Result<BlobMetadata, Self::Error> {
        blobs::put(id.into(), media_type.into(), content.to_vec())
            .await
            .map(Into::into)
    }
}

impl From<blobs::BlobMetadata> for BlobMetadata {
    fn from(value: blobs::BlobMetadata) -> Self {
        Self {
            media_type: value.media_type,
            size: value.size,
            image: value.image.map(|image| ImageDimensions {
                width: image.width,
                height: image.height,
            }),
        }
    }
}

impl fleet_core::Error<BlobsErrorKind> for blobs::BlobsError {
    fn kind(&self) -> BlobsErrorKind {
        match self.kind {
            blobs::BlobsErrorKind::NotFound => BlobsErrorKind::NotFound,
            blobs::BlobsErrorKind::Denied => BlobsErrorKind::Denied,
            blobs::BlobsErrorKind::TooLarge => BlobsErrorKind::TooLarge,
            blobs::BlobsErrorKind::InvalidData => BlobsErrorKind::InvalidData,
            blobs::BlobsErrorKind::Conflict => BlobsErrorKind::Conflict,
            blobs::BlobsErrorKind::Other => BlobsErrorKind::Other,
        }
    }
}
