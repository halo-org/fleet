//! Pending workspace releases, maintained with all feature sets.

use crate::{StoreError, data, records::Records};
use redb::Table;
use serde::{Deserialize, Serialize};

pub const RELEASE: u8 = 21;
/// Marks databases whose settled root agents have been indexed.
pub const INDEXED: &[u8] = &[RELEASE];
/// An agent whose scope closed. Its release is due immediately.
pub const ENDED: u8 = 0;
/// A settled root agent. Its release is due after the host's settled duration.
pub const SETTLED: u8 = 1;
/// A deleted agent. Its release is due immediately and uses the role the agent had.
pub const DELETED: u8 = 2;

/// A scheduled release with its time, agent, generation and the role of a deleted agent.
#[derive(Serialize, Deserialize)]
pub struct Release(
    pub u64,
    pub u128,
    pub u128,
    #[serde(default)] pub Option<data::Configured>,
);

/// Returns the key of a release scheduled at `since`, ordered by time within its kind.
pub fn key(kind: u8, since: u64, agent: u128) -> Vec<u8> {
    [
        &[RELEASE, kind],
        since.to_be_bytes().as_slice(),
        agent.to_be_bytes().as_slice(),
    ]
    .concat()
}

/// Schedules a release, replacing one with the same key.
///
/// The generation tells a replacement apart from the release it replaces.
pub fn schedule(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    kind: u8,
    since: u64,
    agent: u128,
    generation: u128,
) -> Result<(), StoreError> {
    records.put(&key(kind, since, agent), &(since, agent, generation))
}

/// Schedules the release of a deleted agent's workspace under the role it had.
pub fn schedule_deleted(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    since: u64,
    agent: u128,
    generation: u128,
    role: data::Configured,
) -> Result<(), StoreError> {
    records.put(
        &key(DELETED, since, agent),
        &Release(since, agent, generation, Some(role)),
    )
}

pub fn cancel(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    kind: u8,
    since: u64,
    agent: u128,
) -> Result<(), StoreError> {
    records.remove(&key(kind, since, agent))
}
