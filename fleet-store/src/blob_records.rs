use crate::{
    StoreError, data,
    records::{self, Records},
};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

pub const BYTES: TableDefinition<u128, &[u8]> = TableDefinition::new("blob_bytes");
pub const UPLOADS: TableDefinition<u128, &[u8]> = TableDefinition::new("uploads");
pub const CHUNKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("upload_chunks");
pub const METADATA: u8 = 13;
pub const GRANT: u8 = 14;
#[cfg(feature = "blobs")]
pub const UPLOAD_SEQUENCE: u8 = 15;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub media_type: String,
    pub size: u64,
    pub image: Option<(u32, u32)>,
    pub digest: [u8; 32],
}

pub fn content_ids(content: &[data::ContentBlock]) -> impl Iterator<Item = u128> + '_ {
    content.iter().filter_map(|block| match block {
        data::ContentBlock::Text(_) => None,
        data::ContentBlock::Image(id) => Some(*id),
    })
}

pub fn validate_input<T: ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    input: &[data::ModelInput],
) -> Result<Vec<u128>, StoreError> {
    let mut ids = Vec::new();
    for item in input {
        match item {
            data::ModelInput::User(content) => ids.extend(validate(records, content_ids(content))?),
            data::ModelInput::ToolResult(result) => {
                ids.extend(validate(records, content_ids(&result.output.content))?);
            }
            data::ModelInput::Assistant(..) | data::ModelInput::Compaction(..) => {}
        }
    }
    Ok(ids)
}

pub fn validate<T: ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    ids: impl Iterator<Item = u128>,
) -> Result<Vec<u128>, StoreError> {
    let mut result = Vec::new();
    let mut size = 0_u64;
    for id in ids {
        let metadata = records
            .get::<Metadata>(&records::key(METADATA, id, &[]))?
            .ok_or(StoreError::BlobNotFound)?;
        if metadata.image.is_none() {
            return Err(StoreError::InvalidBlob);
        }
        size = size
            .checked_add(metadata.size)
            .ok_or(StoreError::BlobTooLarge)?;
        if result.len() >= 20 || size > 40 * 1024 * 1024 {
            return Err(StoreError::BlobTooLarge);
        }
        result.push(id);
    }
    Ok(result)
}

pub fn grant(
    records: &mut Records<redb::Table<'_, &[u8], &[u8]>>,
    agent_id: u128,
    ids: &[u128],
) -> Result<(), StoreError> {
    for id in ids {
        records.put(&records::key(GRANT, agent_id, &id.to_be_bytes()), &true)?;
    }
    Ok(())
}
