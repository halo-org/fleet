use crate::{
    Store, StoreError,
    records::Request,
    role_records::{self, RoleRecord},
};
use fleet_core::{Role, RoleName};
use redb::{ReadableDatabase as _, ReadableTable as _};

/// The version of a stored role.
///
/// Versions are unique within a store, so a deleted and recreated role has a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleVersion(u128);

/// A role with its version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRole {
    /// The role.
    pub role: Role,
    /// The version of the role.
    pub version: RoleVersion,
}

impl Store {
    /// Lists the stored roles by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the roles cannot be read.
    pub async fn roles(&self) -> Result<Vec<StoredRole>, StoreError> {
        self.worker()
            .call(|worker| {
                let transaction = worker.database.begin_read()?;
                let roles = transaction.open_table(role_records::ROLES)?;
                roles
                    .iter()?
                    .map(|entry| {
                        let (_, value) = entry?;
                        Ok(serde_json::from_slice::<RoleRecord>(value.value())?.into())
                    })
                    .collect()
            })
            .await
    }

    /// Returns a stored role.
    ///
    /// # Errors
    ///
    /// Returns an error if the role cannot be read.
    pub async fn role(&self, name: &RoleName) -> Result<Option<StoredRole>, StoreError> {
        let name = name.clone();
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let roles = transaction.open_table(role_records::ROLES)?;
                Ok(role_records::get(&roles, name.as_ref())?.map(Into::into))
            })
            .await
    }

    /// Creates or replaces a role if its version matches.
    ///
    /// An `expected` version of `None` creates the role only if it does not exist.
    /// Storing an identical role keeps its version.
    ///
    /// # Errors
    ///
    /// Returns [`PreconditionFailed`](StoreError::PreconditionFailed) if the version does not
    /// match and [`IdempotencyConflict`](StoreError::IdempotencyConflict) if the key was used
    /// for a different request.
    pub async fn put_role(
        &self,
        idempotency_key: &str,
        role: &Role,
        expected: Option<RoleVersion>,
    ) -> Result<(), StoreError> {
        self.request(
            idempotency_key.into(),
            Request::PutRole {
                expected: expected.map(Into::into),
                role: role.clone().into(),
            },
        )
        .await
    }

    /// Deletes a role if its version matches.
    ///
    /// # Errors
    ///
    /// Returns [`PreconditionFailed`](StoreError::PreconditionFailed) if the role is missing or
    /// its version does not match, and
    /// [`IdempotencyConflict`](StoreError::IdempotencyConflict) if the key was used for a
    /// different request.
    pub async fn delete_role(
        &self,
        idempotency_key: &str,
        name: &RoleName,
        expected: RoleVersion,
    ) -> Result<(), StoreError> {
        self.request(
            idempotency_key.into(),
            Request::DeleteRole {
                name: name.to_string(),
                expected: expected.into(),
            },
        )
        .await
    }
}

impl fleet_core::Roles for Store {
    type Error = StoreError;

    async fn get(&self, name: &RoleName) -> Result<Option<Role>, StoreError> {
        Ok(self.role(name).await?.map(|stored| stored.role))
    }
}

impl From<u128> for RoleVersion {
    fn from(value: u128) -> Self {
        Self(value)
    }
}

impl From<RoleVersion> for u128 {
    fn from(value: RoleVersion) -> Self {
        value.0
    }
}

impl From<RoleRecord> for StoredRole {
    fn from(record: RoleRecord) -> Self {
        Self {
            role: record.role.into(),
            version: record.version.into(),
        }
    }
}
