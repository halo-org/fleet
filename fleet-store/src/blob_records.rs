use crate::{
    StoreError, data,
    records::{self, Records},
};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const BYTES: TableDefinition<u128, &[u8]> = TableDefinition::new("blob_bytes");
pub const UPLOADS: TableDefinition<u128, &[u8]> = TableDefinition::new("uploads");
pub const CHUNKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("upload_chunks");
pub const METADATA: u8 = 13;
pub const GRANT: u8 = 14;
#[cfg(feature = "blobs")]
pub const UPLOAD_SEQUENCE: u8 = 15;
/// An agent holding a blob, keyed by the blob.
pub const HOLDER: u8 = 23;

#[derive(Deserialize)]
struct UploadState {
    complete: bool,
}

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
        records.put(&records::key(HOLDER, *id, &agent_id.to_be_bytes()), &())?;
    }
    Ok(())
}

/// Indexes the holders of blobs granted before format 6.
pub fn index_holders(
    records: &mut Records<redb::Table<'_, &[u8], &[u8]>>,
) -> Result<(), StoreError> {
    let mut grants = Vec::new();
    for entry in records
        .0
        .range([GRANT].as_slice()..[GRANT + 1].as_slice())?
    {
        if let Some(grant) = grant_key(entry?.0.value()) {
            grants.push(grant);
        }
    }
    for (agent, id) in grants {
        records.put(&records::key(HOLDER, id, &agent.to_be_bytes()), &())?;
    }
    Ok(())
}

/// Removes the agents' grants and the blobs no other agent holds.
pub fn revoke(
    transaction: &redb::WriteTransaction,
    records: &mut Records<redb::Table<'_, &[u8], &[u8]>>,
    agents: &BTreeSet<u128>,
) -> Result<(), StoreError> {
    let mut revoked = BTreeSet::new();
    for &agent in agents {
        let prefix = records::key(GRANT, agent, &[]);
        let mut ids = Vec::new();
        for entry in records
            .0
            .range(prefix.as_slice()..records::prefix_end(&prefix).as_slice())?
        {
            if let Some((_, id)) = grant_key(entry?.0.value()) {
                ids.push(id);
            }
        }
        for id in ids {
            records.remove(&records::key(GRANT, agent, &id.to_be_bytes()))?;
            records.remove(&records::key(HOLDER, id, &agent.to_be_bytes()))?;
            revoked.insert(id);
        }
    }
    let mut bytes = transaction.open_table(BYTES)?;
    let mut uploads = transaction.open_table(UPLOADS)?;
    for id in revoked {
        let prefix = records::key(HOLDER, id, &[]);
        let end = records::prefix_end(&prefix);
        if records
            .0
            .range(prefix.as_slice()..end.as_slice())?
            .next()
            .transpose()?
            .is_some()
        {
            continue;
        }
        records.remove(&records::key(METADATA, id, &[]))?;
        bytes.remove(id)?;
        let upload = uploads
            .get(id)?
            .map(|upload| serde_json::from_slice::<UploadState>(upload.value()))
            .transpose()?;
        if upload.is_some_and(|upload| upload.complete) {
            uploads.remove(id)?;
        }
    }
    Ok(())
}

fn grant_key(key: &[u8]) -> Option<(u128, u128)> {
    let (agent, id) = key.strip_prefix(&[GRANT])?.split_first_chunk()?;
    Some((
        u128::from_be_bytes(*agent),
        u128::from_be_bytes(id.try_into().ok()?),
    ))
}

#[cfg(any(feature = "blobs", feature = "subagents"))]
pub fn readable<T: ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    agent: u128,
    call: Option<&str>,
    id: u128,
) -> Result<bool, StoreError> {
    let granted = |agent: u128| {
        records
            .get::<bool>(&records::key(GRANT, agent, &id.to_be_bytes()))
            .map(|grant| grant == Some(true))
    };
    if granted(agent)? {
        return Ok(true);
    }
    let Some(call) = call else {
        return Ok(false);
    };
    for child in crate::agents::children(records, agent)? {
        let child = child?;
        if granted(child)?
            && crate::metadata::agent_parent(&records.agent(child)?.agent.extensions)
                == Some((agent, call))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
