//! Stored roles, maintained with all feature sets.

use crate::{
    StoreError, data,
    records::{Records, Request},
};
use redb::{ReadableTable, Table, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};

pub const ROLES: TableDefinition<&str, &[u8]> = TableDefinition::new("roles");
pub const VERSION_SEQUENCE: u8 = 16;

#[derive(Serialize, Deserialize)]
pub struct RoleRecord {
    pub role: data::Role,
    pub version: u128,
}

pub fn get(
    roles: &impl ReadableTable<&'static str, &'static [u8]>,
    name: &str,
) -> Result<Option<RoleRecord>, StoreError> {
    roles
        .get(name)?
        .map(|value| serde_json::from_slice(value.value()).map_err(Into::into))
        .transpose()
}

/// Applies a role request, returning whether the request was a role request.
pub fn apply(
    transaction: &WriteTransaction,
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    request: &Request,
) -> Result<bool, StoreError> {
    match request {
        Request::PutRole { expected, role } => {
            put(
                records,
                &mut transaction.open_table(ROLES)?,
                *expected,
                role,
            )?;
        }
        Request::DeleteRole { name, expected } => {
            delete(&mut transaction.open_table(ROLES)?, name, *expected)?;
        }
        Request::CreateAgent(..)
        | Request::SendMessage(..)
        | Request::InterruptAgent(..)
        | Request::SetTitle { .. }
        | Request::SetPinned { .. }
        | Request::SetInbox { .. } => return Ok(false),
    }
    Ok(true)
}

fn put(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    roles: &mut Table<'_, &'static str, &'static [u8]>,
    expected: Option<u128>,
    role: &data::Role,
) -> Result<(), StoreError> {
    let current = get(roles, &role.name)?;
    if current.as_ref().map(|record| record.version) != expected {
        return Err(StoreError::PreconditionFailed);
    }
    if current.is_some_and(|record| record.role == *role) {
        return Ok(());
    }
    let version = records
        .get::<u128>(&[VERSION_SEQUENCE])?
        .unwrap_or(0)
        .checked_add(1)
        .expect("role versions do not overflow");
    records.put(&[VERSION_SEQUENCE], &version)?;
    let record = RoleRecord {
        role: role.clone(),
        version,
    };
    roles.insert(role.name.as_str(), serde_json::to_vec(&record)?.as_slice())?;
    Ok(())
}

fn delete(
    roles: &mut Table<'_, &'static str, &'static [u8]>,
    name: &str,
    expected: u128,
) -> Result<(), StoreError> {
    if get(roles, name)?.map(|record| record.version) != Some(expected) {
        return Err(StoreError::PreconditionFailed);
    }
    roles.remove(name)?;
    Ok(())
}
