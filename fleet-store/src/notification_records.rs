//! Pending notifications and notification devices, maintained with all feature sets.

use crate::{
    StoreError,
    records::{self, Records},
};
use redb::Table;
use serde::{Deserialize, Serialize};

pub const NOTIFICATION: u8 = 18;
#[cfg(feature = "notifications")]
pub const DEVICE: u8 = 19;

/// The latest undelivered notification for an agent.
#[derive(Serialize, Deserialize)]
pub struct NotificationRecord {
    pub agent: u128,
    pub cursor: u128,
    pub lease_until: u64,
    pub claim: u64,
}

/// Records that an agent finished a turn at `cursor`, replacing an older notification.
pub fn record(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    agent: u128,
    cursor: u128,
) -> Result<(), StoreError> {
    records.put(
        &records::key(NOTIFICATION, agent, &[]),
        &NotificationRecord {
            agent,
            cursor,
            lease_until: 0,
            claim: 0,
        },
    )
}
