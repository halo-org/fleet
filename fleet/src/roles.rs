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
        let (model, tools, system_prompt) = match name.as_ref() {
            "test" => (
                "example:test",
                BTreeMap::from([(
                    "echo".into(),
                    Configured {
                        name: "example:echo".into(),
                        config: JsonObject::new(),
                    },
                )]),
                "A deterministic test provider.",
            ),
            "sol" => (
                "openai:gpt-6.1-sol",
                BTreeMap::new(),
                "You are a helpful assistant.",
            ),
            _ => return future::ready(Ok(None)),
        };
        let role = Role {
            name: name.clone(),
            parameters: BTreeMap::new(),
            model: Configured {
                name: model.into(),
                config: JsonObject::new(),
            },
            tools,
            system_prompt: system_prompt.into(),
            compaction: None,
            extensions: JsonObject::new(),
        };

        future::ready(Ok(Some(role)))
    }
}
