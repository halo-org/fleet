use crate::{
    Store, StoreError,
    blob_records::{self, Metadata},
    records::{self, Records},
};
use fleet_core::{AgentId, BlobId};
use fleet_kit::{BlobMetadata, Blobs, BlobsErrorKind, ImageDimensions};
use image::{ImageFormat, ImageReader, Limits};
use redb::{ReadableDatabase as _, ReadableTable as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::io::Cursor;

#[cfg(test)]
mod tests;

/// The maximum size of a blob accepted by this store.
pub const MAX_BLOB_BYTES: u64 = 10 * 1024 * 1024;
/// The maximum size of an individual resumable upload chunk.
pub const MAX_UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;
const UPLOAD_LIFETIME: u64 = 24 * 60 * 60 * 1000;
const MAX_UPLOADS: usize = 64;
const MAX_STAGING_BYTES: u64 = 256 * 1024 * 1024;

/// A resumable upload. Its ID is not a readable blob until completion succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    /// The caller-assigned upload identity, retained across retries.
    pub id: u128,
    /// The declared media type.
    pub media_type: String,
    /// The declared total length.
    pub size: u64,
    /// The number of durably accepted bytes.
    pub offset: u64,
    /// When this session and its completion receipt expire, in Unix milliseconds.
    pub expires_at: u64,
    /// The published blob, if completion succeeded.
    pub blob: Option<BlobId>,
}

#[derive(Clone, Serialize, Deserialize)]
struct UploadRecord {
    media_type: String,
    size: u64,
    offset: u64,
    expires_at: u64,
    complete: bool,
    generation: u128,
}

impl UploadRecord {
    fn public(&self, id: u128) -> Upload {
        Upload {
            id,
            media_type: self.media_type.clone(),
            size: self.size,
            offset: self.offset,
            expires_at: self.expires_at,
            blob: self.complete.then(|| id.into()),
        }
    }
}

fn inspect(media_type: &str, content: &[u8]) -> Result<Metadata, StoreError> {
    let size = u64::try_from(content.len()).map_err(|_| StoreError::BlobTooLarge)?;
    if size == 0 || size > MAX_BLOB_BYTES {
        return Err(StoreError::BlobTooLarge);
    }
    if media_type.is_empty()
        || media_type.len() > 100
        || !media_type.is_ascii()
        || media_type.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(StoreError::InvalidBlob);
    }
    let dimensions = if media_type.starts_with("image/") {
        let format = match media_type {
            "image/png" => ImageFormat::Png,
            "image/jpeg" => ImageFormat::Jpeg,
            "image/gif" => ImageFormat::Gif,
            "image/webp" => ImageFormat::WebP,
            _ => return Err(StoreError::InvalidBlob),
        };
        if image::guess_format(content).ok() != Some(format) {
            return Err(StoreError::InvalidBlob);
        }
        let dimensions = ImageReader::with_format(Cursor::new(content), format)
            .into_dimensions()
            .map_err(|_| StoreError::InvalidBlob)?;
        if dimensions.0 == 0
            || dimensions.1 == 0
            || dimensions.0 > 8192
            || dimensions.1 > 8192
            || u64::from(dimensions.0).saturating_mul(u64::from(dimensions.1)) > 32_000_000
        {
            return Err(StoreError::BlobTooLarge);
        }
        let mut reader = ImageReader::with_format(Cursor::new(content), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let decoded = reader.decode().map_err(|_| StoreError::InvalidBlob)?;
        if u64::from(decoded.width()).saturating_mul(u64::from(decoded.height())) > 32_000_000 {
            return Err(StoreError::BlobTooLarge);
        }
        Some((decoded.width(), decoded.height()))
    } else {
        None
    };
    Ok(Metadata {
        media_type: media_type.into(),
        size,
        image: dimensions,
        digest: Sha256::digest(content).into(),
    })
}

fn metadata(value: &Metadata) -> BlobMetadata {
    BlobMetadata {
        media_type: value.media_type.clone(),
        size: value.size,
        image: value
            .image
            .map(|(width, height)| ImageDimensions { width, height }),
    }
}

fn publish(
    transaction: &redb::WriteTransaction,
    id: u128,
    info: &Metadata,
    content: &[u8],
) -> Result<(), StoreError> {
    let mut records = Records(transaction.open_table(records::RECORDS)?);
    let key = records::key(blob_records::METADATA, id, &[]);
    if let Some(previous) = records.get::<Metadata>(&key)? {
        if previous != *info {
            return Err(StoreError::BlobConflict);
        }
        let table = transaction.open_table(blob_records::BYTES)?;
        if table.get(id)?.is_none_or(|value| value.value() != content) {
            return Err(StoreError::InvalidBlob);
        }
    } else {
        transaction
            .open_table(blob_records::BYTES)?
            .insert(id, content)?;
        records.put(&key, info)?;
    }
    Ok(())
}

impl Blobs for Store {
    type Error = StoreError;

    async fn get(&self, id: BlobId) -> Result<Option<BlobMetadata>, StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                Ok(records
                    .get::<Metadata>(&records::key(blob_records::METADATA, id.into(), &[]))?
                    .as_ref()
                    .map(metadata))
            })
            .await
    }

    async fn read(&self, id: BlobId, limit: u64) -> Result<Vec<u8>, StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                let info = records
                    .get::<Metadata>(&records::key(blob_records::METADATA, id.into(), &[]))?
                    .ok_or(StoreError::BlobNotFound)?;
                if info.size > limit.min(MAX_BLOB_BYTES) {
                    return Err(StoreError::BlobTooLarge);
                }
                let table = transaction.open_table(blob_records::BYTES)?;
                let content = table.get(u128::from(id))?.ok_or(StoreError::InvalidBlob)?;
                if u64::try_from(content.value().len()).ok() != Some(info.size)
                    || <[u8; 32]>::from(Sha256::digest(content.value())) != info.digest
                {
                    return Err(StoreError::InvalidBlob);
                }
                Ok(content.value().to_vec())
            })
            .await
    }

    async fn put(
        &self,
        id: BlobId,
        media_type: &str,
        content: &[u8],
    ) -> Result<BlobMetadata, StoreError> {
        if content.is_empty()
            || u64::try_from(content.len()).map_err(|_| StoreError::BlobTooLarge)? > MAX_BLOB_BYTES
        {
            return Err(StoreError::BlobTooLarge);
        }
        let media_type = media_type.to_owned();
        let content = content.to_vec();
        self.worker()
            .call(move |worker| {
                let info = inspect(&media_type, &content)?;
                let transaction = worker.database.begin_write()?;
                publish(&transaction, id.into(), &info, &content)?;
                transaction.commit()?;
                Ok(metadata(&info))
            })
            .await
    }
}

/// Blob access restricted to an agent's published attachments and newly created blobs.
#[derive(Clone)]
pub struct AgentBlobs {
    store: Store,
    agent_id: AgentId,
}

impl AgentBlobs {
    async fn check(&self, id: BlobId) -> Result<(), StoreError> {
        let agent = self.agent_id;
        self.store
            .worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                if records.get::<bool>(&records::key(
                    blob_records::GRANT,
                    agent.into(),
                    &u128::from(id).to_be_bytes(),
                ))? != Some(true)
                {
                    return Err(StoreError::BlobDenied);
                }
                Ok(())
            })
            .await
    }
}

impl Blobs for AgentBlobs {
    type Error = StoreError;

    async fn get(&self, id: BlobId) -> Result<Option<BlobMetadata>, StoreError> {
        self.check(id).await?;
        self.store.get(id).await
    }

    async fn read(&self, id: BlobId, limit: u64) -> Result<Vec<u8>, StoreError> {
        self.check(id).await?;
        self.store.read(id, limit).await
    }

    async fn put(
        &self,
        id: BlobId,
        media_type: &str,
        content: &[u8],
    ) -> Result<BlobMetadata, StoreError> {
        if content.is_empty()
            || u64::try_from(content.len()).map_err(|_| StoreError::BlobTooLarge)? > MAX_BLOB_BYTES
        {
            return Err(StoreError::BlobTooLarge);
        }
        let media_type = media_type.to_owned();
        let content = content.to_vec();
        let agent_id = self.agent_id;
        self.store
            .worker()
            .call(move |worker| {
                let info = inspect(&media_type, &content)?;
                let transaction = worker.database.begin_write()?;
                {
                    let records = Records(transaction.open_table(records::RECORDS)?);
                    records.agent(agent_id.into())?;
                    if records
                        .get::<Metadata>(&records::key(blob_records::METADATA, id.into(), &[]))?
                        .is_some()
                        && records.get::<bool>(&records::key(
                            blob_records::GRANT,
                            agent_id.into(),
                            &u128::from(id).to_be_bytes(),
                        ))? != Some(true)
                    {
                        return Err(StoreError::BlobDenied);
                    }
                }
                publish(&transaction, id.into(), &info, &content)?;
                blob_records::grant(
                    &mut Records(transaction.open_table(records::RECORDS)?),
                    agent_id.into(),
                    &[id.into()],
                )?;
                transaction.commit()?;
                Ok(metadata(&info))
            })
            .await
    }
}

fn upload(
    table: &impl redb::ReadableTable<u128, &'static [u8]>,
    id: u128,
) -> Result<UploadRecord, StoreError> {
    let value = table.get(id)?.ok_or(StoreError::UploadNotFound)?;
    let record: UploadRecord = serde_json::from_slice(value.value())?;
    if record.expires_at <= crate::agents::now_millis() {
        return Err(StoreError::UploadNotFound);
    }
    Ok(record)
}

fn chunk_key(id: u128, offset: u64) -> Vec<u8> {
    [id.to_be_bytes().as_slice(), offset.to_be_bytes().as_slice()].concat()
}

fn remove_chunks(table: &mut redb::Table<'_, &[u8], &[u8]>, id: u128) -> Result<(), StoreError> {
    let start = chunk_key(id, 0);
    let end = chunk_key(id, u64::MAX);
    let keys = table
        .range(start.as_slice()..=end.as_slice())?
        .map(|entry| entry.map(|(key, _)| key.value().to_vec()))
        .collect::<Result<Vec<_>, _>>()?;
    for key in keys {
        table.remove(key.as_slice())?;
    }
    Ok(())
}

impl Store {
    /// Restricts blob access to the given agent.
    #[must_use]
    pub fn agent_blobs(&self, agent_id: AgentId) -> AgentBlobs {
        AgentBlobs {
            store: self.clone(),
            agent_id,
        }
    }

    /// Creates a bounded upload session. Matching retries return the existing session.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported images, exhausted staging capacity, or a reused ID with
    /// different arguments. Expired sessions are reclaimed before capacity is checked.
    pub async fn create_upload(
        &self,
        id: u128,
        media_type: &str,
        size: u64,
    ) -> Result<Upload, StoreError> {
        if !matches!(
            media_type,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            return Err(StoreError::InvalidBlob);
        }
        if size == 0 || size > MAX_BLOB_BYTES {
            return Err(StoreError::BlobTooLarge);
        }
        let media_type = media_type.to_owned();
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let result = {
                    let mut table = transaction.open_table(blob_records::UPLOADS)?;
                    let mut chunks = transaction.open_table(blob_records::CHUNKS)?;
                    let now = crate::agents::now_millis();
                    let records = table
                        .iter()?
                        .map(|entry| {
                            let (key, value) = entry?;
                            Ok((
                                key.value(),
                                serde_json::from_slice::<UploadRecord>(value.value())?,
                            ))
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    let mut count = 0_usize;
                    let mut reserved = 0_u64;
                    for (key, record) in records {
                        if record.expires_at <= now {
                            table.remove(key)?;
                            remove_chunks(&mut chunks, key)?;
                        } else if !record.complete {
                            count = count.saturating_add(1);
                            reserved = reserved.saturating_add(record.size);
                        }
                    }
                    if let Some(value) = table.get(id)? {
                        let record: UploadRecord = serde_json::from_slice(value.value())?;
                        if record.size != size || record.media_type != media_type {
                            return Err(StoreError::UploadConflict);
                        }
                        record.public(id)
                    } else {
                        if count >= MAX_UPLOADS || reserved.saturating_add(size) > MAX_STAGING_BYTES
                        {
                            return Err(StoreError::BlobTooLarge);
                        }
                        let mut records = Records(transaction.open_table(records::RECORDS)?);
                        let generation = records
                            .get::<u128>(&[blob_records::UPLOAD_SEQUENCE])?
                            .unwrap_or(0)
                            .checked_add(1)
                            .ok_or(StoreError::UploadConflict)?;
                        records.put(&[blob_records::UPLOAD_SEQUENCE], &generation)?;
                        let record = UploadRecord {
                            media_type,
                            size,
                            offset: 0,
                            expires_at: now.saturating_add(UPLOAD_LIFETIME),
                            complete: false,
                            generation,
                        };
                        table.insert(id, serde_json::to_vec(&record)?.as_slice())?;
                        record.public(id)
                    }
                };
                transaction.commit()?;
                Ok(result)
            })
            .await
    }

    /// Returns the durably accepted offset and completion state.
    ///
    /// # Errors
    ///
    /// Returns an error when the upload is absent, expired, or cannot be read.
    pub async fn upload(&self, id: u128) -> Result<Upload, StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                Ok(upload(&transaction.open_table(blob_records::UPLOADS)?, id)?.public(id))
            })
            .await
    }

    /// Appends a chunk at exactly the current offset, atomically advancing that offset.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent or completed upload, a mismatched offset, or a size limit.
    pub async fn append_upload(
        &self,
        id: u128,
        offset: u64,
        content: &[u8],
    ) -> Result<Upload, StoreError> {
        if content.is_empty() || content.len() > MAX_UPLOAD_CHUNK_BYTES {
            return Err(StoreError::BlobTooLarge);
        }
        let content = content.to_vec();
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let result = {
                    let mut table = transaction.open_table(blob_records::UPLOADS)?;
                    let mut record = upload(&table, id)?;
                    if record.complete || record.offset != offset {
                        return Err(StoreError::UploadConflict);
                    }
                    let end = offset
                        .checked_add(
                            u64::try_from(content.len()).map_err(|_| StoreError::BlobTooLarge)?,
                        )
                        .ok_or(StoreError::BlobTooLarge)?;
                    if end > record.size {
                        return Err(StoreError::BlobTooLarge);
                    }
                    transaction
                        .open_table(blob_records::CHUNKS)?
                        .insert(chunk_key(id, offset).as_slice(), content.as_slice())?;
                    record.offset = end;
                    table.insert(id, serde_json::to_vec(&record)?.as_slice())?;
                    record.public(id)
                };
                transaction.commit()?;
                Ok(result)
            })
            .await
    }

    /// Validates and publishes a complete image. Repeated completion returns the same blob ID.
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete, invalid, expired, or conflicting uploads.
    pub async fn complete_upload(&self, id: u128) -> Result<BlobId, StoreError> {
        let pending = self
            .worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let record = upload(&transaction.open_table(blob_records::UPLOADS)?, id)?;
                if record.complete {
                    return Ok(None);
                }
                if record.offset != record.size {
                    return Err(StoreError::UploadConflict);
                }
                let table = transaction.open_table(blob_records::CHUNKS)?;
                let start = chunk_key(id, 0);
                let end = chunk_key(id, u64::MAX);
                let mut bytes = Vec::new();
                for entry in table.range(start.as_slice()..=end.as_slice())? {
                    let (_, value) = entry?;
                    bytes.extend_from_slice(value.value());
                    if u64::try_from(bytes.len()).map_err(|_| StoreError::BlobTooLarge)?
                        > record.size
                    {
                        return Err(StoreError::InvalidBlob);
                    }
                }
                if u64::try_from(bytes.len()).ok() != Some(record.size) {
                    return Err(StoreError::InvalidBlob);
                }
                let info = inspect(&record.media_type, &bytes)?;
                Ok(Some((record, bytes, info)))
            })
            .await?;
        if let Some((snapshot, bytes, info)) = pending {
            self.worker()
                .call(move |worker| {
                    let transaction = worker.database.begin_write()?;
                    {
                        let mut table = transaction.open_table(blob_records::UPLOADS)?;
                        let mut record = upload(&table, id)?;
                        if record.generation != snapshot.generation || record.offset != record.size
                        {
                            return Err(StoreError::UploadConflict);
                        }
                        publish(&transaction, id, &info, &bytes)?;
                        record.complete = true;
                        table.insert(id, serde_json::to_vec(&record)?.as_slice())?;
                        remove_chunks(&mut transaction.open_table(blob_records::CHUNKS)?, id)?;
                    }
                    transaction.commit()?;
                    Ok(())
                })
                .await?;
        }
        Ok(id.into())
    }

    /// Discards an upload session and its staging bytes, but never its completed blob.
    ///
    /// # Errors
    ///
    /// Returns an error when storage fails. Absent sessions are already discarded.
    pub async fn delete_upload(&self, id: u128) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                transaction.open_table(blob_records::UPLOADS)?.remove(id)?;
                remove_chunks(&mut transaction.open_table(blob_records::CHUNKS)?, id)?;
                transaction.commit()?;
                Ok(())
            })
            .await
    }
}

impl fleet_core::Error<BlobsErrorKind> for StoreError {
    fn kind(&self) -> BlobsErrorKind {
        match self {
            Self::BlobNotFound => BlobsErrorKind::NotFound,
            Self::BlobDenied | Self::AgentNotFound => BlobsErrorKind::Denied,
            Self::BlobTooLarge => BlobsErrorKind::TooLarge,
            Self::InvalidBlob | Self::Encoding(_) => BlobsErrorKind::InvalidData,
            Self::BlobConflict => BlobsErrorKind::Conflict,
            _ => BlobsErrorKind::Other,
        }
    }
}
