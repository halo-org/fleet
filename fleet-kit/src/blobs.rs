use alloc::{boxed::Box, rc::Rc, string::String, sync::Arc, vec::Vec};
use core::fmt;
use fleet_core::BlobId;

/// The metadata of an immutable blob.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobMetadata {
    /// The media type of the stored bytes.
    pub media_type: String,
    /// The length of the stored bytes.
    pub size: u64,
    /// Validated image dimensions, when this blob is an image.
    pub image: Option<ImageDimensions>,
}

/// The dimensions of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageDimensions {
    /// The width in pixels.
    pub width: u32,
    /// The height in pixels.
    pub height: u32,
}

/// Provides access to immutable blobs.
///
/// Implementations control access. Writes must complete before their references are published.
/// Blobs referenced by retained history must remain available, including after compaction.
pub trait Blobs {
    /// The error type returned by this provider.
    type Error: fleet_core::Error<BlobsErrorKind> + 'static;

    /// Returns metadata without reading the payload.
    ///
    /// # Errors
    ///
    /// Returns an error when access is denied or the provider cannot perform the lookup.
    fn get(
        &self,
        id: BlobId,
    ) -> impl Future<Output = Result<Option<BlobMetadata>, Self::Error>> + Send;

    /// Reads a blob within the given byte limit.
    ///
    /// Rejects larger blobs before allocating their content. Providers may enforce a smaller limit.
    ///
    /// # Errors
    ///
    /// Returns [`NotFound`](BlobsErrorKind::NotFound) for an absent blob and
    /// [`TooLarge`](BlobsErrorKind::TooLarge) when the read exceeds a limit.
    /// Returns other errors when access or storage fails.
    fn read(
        &self,
        id: BlobId,
        limit: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send;

    /// Publishes a blob under the given ID.
    ///
    /// Repeating the same write succeeds. An existing ID must never be assigned different bytes
    /// or metadata. If the future is dropped, retry with the same ID.
    ///
    /// # Errors
    ///
    /// Returns [`Conflict`](BlobsErrorKind::Conflict) when the ID already names different content,
    /// [`InvalidData`](BlobsErrorKind::InvalidData) for invalid content, and
    /// [`TooLarge`](BlobsErrorKind::TooLarge) when a limit is exceeded.
    /// Returns other errors when access or storage fails.
    fn put(
        &self,
        id: BlobId,
        media_type: &str,
        content: &[u8],
    ) -> impl Future<Output = Result<BlobMetadata, Self::Error>> + Send;
}

/// The kind of error returned by a blob provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BlobsErrorKind {
    /// The blob does not exist.
    NotFound,
    /// The caller cannot access the blob or perform this operation.
    Denied,
    /// A size or resource limit was exceeded.
    TooLarge,
    /// The content is invalid or corrupt.
    InvalidData,
    /// The ID already names different content.
    Conflict,
    /// A storage failure not covered by another kind.
    Other,
}

/// A blob provider that denies all access.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoBlobs;

macro_rules! impl_blobs_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T: Blobs + ?Sized> Blobs for $t {
            type Error = T::Error;

            fn get(
                &self,
                id: BlobId,
            ) -> impl Future<Output = Result<Option<BlobMetadata>, Self::Error>> + Send {
                (**self).get(id)
            }

            fn read(
                &self,
                id: BlobId,
                limit: u64,
            ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send {
                (**self).read(id, limit)
            }

            fn put(
                &self,
                id: BlobId,
                media_type: &str,
                content: &[u8],
            ) -> impl Future<Output = Result<BlobMetadata, Self::Error>> + Send {
                (**self).put(id, media_type, content)
            }
        }
    };
}

impl_blobs_for_pointer!(&T);
impl_blobs_for_pointer!(&mut T);
impl_blobs_for_pointer!(Box<T>);
impl_blobs_for_pointer!(Rc<T>);
impl_blobs_for_pointer!(Arc<T>);

impl fmt::Display for BlobsErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("blob not found"),
            Self::Denied => f.write_str("blob access denied"),
            Self::TooLarge => f.write_str("blob limit exceeded"),
            Self::InvalidData => f.write_str("invalid blob data"),
            Self::Conflict => f.write_str("blob conflict"),
            Self::Other => f.write_str("blob storage failed"),
        }
    }
}

impl core::error::Error for BlobsErrorKind {}

impl fleet_core::Error<Self> for BlobsErrorKind {
    fn kind(&self) -> Self {
        *self
    }
}

impl Blobs for NoBlobs {
    type Error = BlobsErrorKind;

    fn get(
        &self,
        _id: BlobId,
    ) -> impl Future<Output = Result<Option<BlobMetadata>, Self::Error>> + Send {
        core::future::ready(Err(BlobsErrorKind::Denied))
    }

    fn read(
        &self,
        _id: BlobId,
        _limit: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        core::future::ready(Err(BlobsErrorKind::Denied))
    }

    fn put(
        &self,
        _id: BlobId,
        _media_type: &str,
        _content: &[u8],
    ) -> impl Future<Output = Result<BlobMetadata, Self::Error>> + Send {
        core::future::ready(Err(BlobsErrorKind::Denied))
    }
}
