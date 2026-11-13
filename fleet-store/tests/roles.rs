//! Tests for stored roles.

#![cfg(feature = "roles")]

#[cfg(test)]
mod tests {
    use core::assert_matches;
    use fleet_core::{Configured, JsonObject, Role, Roles as _};
    use fleet_kit::{RoleStore as _, StoredRole};
    use fleet_store::{Store, StoreError};
    use std::collections::BTreeMap;

    fn role(name: &str, system_prompt: &str) -> Role {
        Role {
            name: name.into(),
            parameters: BTreeMap::new(),
            model: Configured {
                name: "provider:model".into(),
                config: JsonObject::new(),
            },
            tools: BTreeMap::new(),
            system_prompt: system_prompt.into(),
            compaction: None,
            extensions: JsonObject::new(),
        }
    }

    #[test]
    fn writes_require_the_current_version() {
        let store = Store::memory().unwrap();
        let first = role("assistant", "Be helpful.");
        let second = role("assistant", "Be brief.");

        futures_executor::block_on(async {
            store.put_role("create", &first, None).await.unwrap();
            let created = store.role(&"assistant".into()).await.unwrap().unwrap();
            assert_eq!(created.role, first);

            let error = store.put_role("again", &second, None).await.unwrap_err();
            assert_matches!(error, StoreError::PreconditionFailed);

            store
                .put_role("edit", &second, Some(created.version))
                .await
                .unwrap();
            let edited = store.role(&"assistant".into()).await.unwrap().unwrap();
            assert_eq!(edited.role, second);
            assert_ne!(edited.version, created.version);
            assert_eq!(store.get(&"assistant".into()).await.unwrap(), Some(second));

            let error = store
                .delete_role("delete", &"assistant".into(), created.version)
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::PreconditionFailed);

            store
                .delete_role("delete", &"assistant".into(), edited.version)
                .await
                .unwrap();
            assert_eq!(store.role(&"assistant".into()).await.unwrap(), None);

            store.put_role("recreate", &first, None).await.unwrap();
            let recreated = store.role(&"assistant".into()).await.unwrap().unwrap();
            assert!(recreated.version > edited.version);
        });
    }

    #[test]
    fn retries_reuse_the_receipt() {
        let store = Store::memory().unwrap();
        let reviewer = role("reviewer", "Review changes.");
        let assistant = role("assistant", "Be helpful.");

        futures_executor::block_on(async {
            store.put_role("reviewer", &reviewer, None).await.unwrap();
            store.put_role("reviewer", &reviewer, None).await.unwrap();
            let error = store
                .put_role("reviewer", &assistant, None)
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::IdempotencyConflict);

            store.put_role("assistant", &assistant, None).await.unwrap();
            let version = store
                .role(&"assistant".into())
                .await
                .unwrap()
                .unwrap()
                .version;
            store
                .put_role("unchanged", &assistant, Some(version))
                .await
                .unwrap();

            let roles = store.roles().await.unwrap();
            assert_eq!(
                roles
                    .iter()
                    .map(|StoredRole { role, .. }| role.name.as_ref())
                    .collect::<Vec<_>>(),
                ["assistant", "reviewer"],
            );
            assert_eq!(roles[0].version, version);
        });
    }
}
