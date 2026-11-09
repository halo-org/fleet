use crate::AuthenticationError;
use redb::{ReadableDatabase as _, TableDefinition};
use serde::{Serialize, de::DeserializeOwned};
use std::{fs, path::Path, sync::Arc};

const RECORDS: TableDefinition<&str, &[u8]> = TableDefinition::new("credentials");

pub struct CredentialStore(Arc<redb::Database>);

impl CredentialStore {
    pub fn open(directory: &Path) -> Result<Self, AuthenticationError> {
        fs::create_dir_all(directory).map_err(|error| {
            AuthenticationError::source("could not create credential directory", error)
        })?;
        let path = directory.join("credentials.db");
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
                AuthenticationError::source("could not protect credential directory", error)
            })?;
            options.mode(0o600);
        }
        options.open(&path).map_err(|error| {
            AuthenticationError::source("could not create credential database", error)
        })?;
        let database = redb::Database::create(path).map_err(|error| {
            AuthenticationError::source("could not open credential database", error)
        })?;
        let transaction = database.begin_write().map_err(|error| {
            AuthenticationError::source("could not initialize credential database", error)
        })?;
        transaction.open_table(RECORDS).map_err(|error| {
            AuthenticationError::source("could not initialize credential database", error)
        })?;
        transaction.commit().map_err(|error| {
            AuthenticationError::source("could not initialize credential database", error)
        })?;
        Ok(Self(Arc::new(database)))
    }

    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, AuthenticationError> {
        let transaction = self
            .0
            .begin_read()
            .map_err(|error| AuthenticationError::source("could not read credentials", error))?;
        let table = transaction
            .open_table(RECORDS)
            .map_err(|error| AuthenticationError::source("could not read credentials", error))?;
        table
            .get(key)
            .map_err(|error| AuthenticationError::source("could not read credentials", error))?
            .map(|record| serde_json::from_slice(record.value()))
            .transpose()
            .map_err(|error| AuthenticationError::source("could not decode credentials", error))
    }

    pub async fn put(
        &self,
        key: String,
        value: &(impl Serialize + Sync),
    ) -> Result<(), AuthenticationError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| AuthenticationError::source("could not encode credentials", error))?;
        let database = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let transaction = database.begin_write().map_err(|error| {
                AuthenticationError::source("could not save credentials", error)
            })?;
            transaction
                .open_table(RECORDS)
                .map_err(|error| AuthenticationError::source("could not save credentials", error))?
                .insert(key.as_str(), bytes.as_slice())
                .map_err(|error| {
                    AuthenticationError::source("could not save credentials", error)
                })?;
            transaction
                .commit()
                .map_err(|error| AuthenticationError::source("could not save credentials", error))
        })
        .await
        .expect("credential writer completes")
    }
}
