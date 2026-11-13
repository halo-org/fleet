use crate::{
    Store, StoreError,
    records::Request,
    role_records::{self, RoleRecord},
};
use fleet_core::{Role, RoleName};
use fleet_kit::{RoleStore, RoleStoreErrorKind, RoleVersion, StoredRole};
use redb::{ReadableDatabase as _, ReadableTable as _};

impl Store {
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
        Ok(RoleStore::role(self, name).await?.map(|stored| stored.role))
    }
}

impl RoleStore for Store {
    type Error = StoreError;

    async fn roles(&self) -> Result<Vec<StoredRole>, StoreError> {
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

    async fn role(&self, name: &RoleName) -> Result<Option<StoredRole>, StoreError> {
        let name = name.clone();
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let roles = transaction.open_table(role_records::ROLES)?;
                Ok(role_records::get(&roles, name.as_ref())?.map(Into::into))
            })
            .await
    }

    async fn put_role(
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
}

impl fleet_core::Error<RoleStoreErrorKind> for StoreError {
    fn kind(&self) -> RoleStoreErrorKind {
        match self {
            Self::PreconditionFailed => RoleStoreErrorKind::PreconditionFailed,
            Self::IdempotencyConflict => RoleStoreErrorKind::IdempotencyConflict,
            _ => RoleStoreErrorKind::Other,
        }
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
