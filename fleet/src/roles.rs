use core::{convert::Infallible, future};
use fleet_core::{Configured, JsonObject, Role, RoleName};
use std::collections::BTreeMap;

/// Provides built-in roles.
pub struct Roles;

impl fleet_core::Roles for Roles {
    type Error = Infallible;

    fn get(
        &self,
        name: &RoleName,
    ) -> impl Future<Output = Result<Option<Role>, Self::Error>> + Send {
        let role = (name.as_ref() == "test").then(|| Role {
            name: name.clone(),
            parameters: BTreeMap::new(),
            model: Configured {
                name: "test".into(),
                config: JsonObject::new(),
            },
            tools: BTreeMap::from([(
                "echo".into(),
                Configured {
                    name: "echo".into(),
                    config: JsonObject::new(),
                },
            )]),
            system_prompt: "A deterministic test provider.".into(),
            compaction: None,
            extensions: JsonObject::new(),
        });

        future::ready(Ok(role))
    }
}
