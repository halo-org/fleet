//! Pending workspace releases, maintained with all feature sets.

use crate::{StoreError, records::Records};
use redb::Table;

pub const RELEASE: u8 = 21;
/// Marks databases whose settled root agents have been indexed.
pub const INDEXED: &[u8] = &[RELEASE];
/// An agent whose scope closed. Its release is due immediately.
#[cfg(feature = "queries")]
pub const ENDED: u8 = 0;
/// A settled root agent. Its release is due after the host's settled duration.
pub const SETTLED: u8 = 1;

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

pub fn cancel(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    kind: u8,
    since: u64,
    agent: u128,
) -> Result<(), StoreError> {
    records.remove(&key(kind, since, agent))
}
