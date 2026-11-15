use crate::{Configured, JsonObject, ModelName, ParametersError, Role, ToolAlias, ToolName};

/// A parameterized view of a role.
#[derive(Debug)]
pub struct RoleInstance<'a> {
    definition: &'a Role,
    values: &'a JsonObject,
}

impl<'a> RoleInstance<'a> {
    /// Binds a role to parameter values.
    #[must_use]
    pub const fn new(definition: &'a Role, values: &'a JsonObject) -> Self {
        Self { definition, values }
    }

    /// Returns the model with its parameters applied.
    ///
    /// # Errors
    ///
    /// Returns an error if the model's parameters are invalid.
    pub fn model(&self) -> Result<Configured<ModelName>, ParametersError> {
        let mut model = self.definition.model.clone();
        model.config =
            crate::apply_model_parameters(model.config, &self.definition.parameters, self.values)?;
        Ok(model)
    }

    /// Returns a tool with its parameters applied, or `None` if the alias is not defined.
    ///
    /// # Errors
    ///
    /// Returns an error if the tool's parameters are invalid.
    pub fn tool(&self, alias: &ToolAlias) -> Result<Option<Configured<ToolName>>, ParametersError> {
        let Some(mut tool) = self.definition.tools.get(alias).cloned() else {
            return Ok(None);
        };
        tool.config = crate::apply_tool_parameters(
            alias,
            tool.config,
            &self.definition.parameters,
            self.values,
        )?;
        Ok(Some(tool))
    }

    /// Returns the role's extensions with their parameters applied.
    ///
    /// # Errors
    ///
    /// Returns an error if the extension parameters are invalid.
    pub fn extensions(&self) -> Result<JsonObject, ParametersError> {
        crate::apply_extension_parameters(
            self.definition.extensions.clone(),
            &self.definition.parameters,
            self.values,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JsonValue, RoleParameter, RoleParameterTarget};
    use alloc::{collections::BTreeMap, string::ToString as _, vec};
    use serde_json::json;

    fn object(value: JsonValue) -> JsonObject {
        match value {
            JsonValue::Object(object) => object,
            _ => panic!("expected an object"),
        }
    }

    fn definition() -> Role {
        Role {
            name: "assistant".into(),
            parameters: [
                ("model", RoleParameterTarget::Model(vec![])),
                ("search", RoleParameterTarget::Tool("search".into(), vec![])),
                ("extensions", RoleParameterTarget::Extensions(vec![])),
            ]
            .into_iter()
            .map(|(name, target)| {
                (
                    name.into(),
                    RoleParameter {
                        target,
                        extensions: JsonObject::new(),
                    },
                )
            })
            .collect(),
            model: Configured {
                name: "provider:model".into(),
                config: JsonObject::new(),
            },
            tools: BTreeMap::from([
                (
                    "search".into(),
                    Configured {
                        name: "provider:search".into(),
                        config: JsonObject::new(),
                    },
                ),
                (
                    "fetch".into(),
                    Configured {
                        name: "provider:fetch".into(),
                        config: JsonObject::new(),
                    },
                ),
            ]),
            system_prompt: "Be helpful.".into(),
            compaction: None,
            extensions: JsonObject::new(),
        }
    }

    #[test]
    fn configuration_applies_the_bound_values() {
        let definition = definition();
        let model_config = object(json!({
            "limit": 30,
        }));
        let tool_config = object(json!({
            "limit": 40,
        }));
        let extensions = object(json!({
            "directory": "/repo",
        }));
        let values = object(json!({
            "model": model_config,
            "search": tool_config,
            "extensions": extensions,
        }));
        let role = RoleInstance::new(&definition, &values);

        let model = role.model().unwrap();
        let tool = role.tool(&"search".into()).unwrap().unwrap();
        let resolved_extensions = role.extensions().unwrap();
        let unknown = role.tool(&"unknown".into()).unwrap();

        assert_eq!(model.name, definition.model.name);
        assert_eq!(model.config, model_config);
        assert_eq!(tool.name, ToolName::from("provider:search"));
        assert_eq!(tool.config, tool_config);
        assert_eq!(resolved_extensions, extensions);
        assert_eq!(unknown, None);
    }

    #[test]
    fn invalid_parameters_only_affect_their_target() {
        let definition = definition();
        let values = object(json!({
            "model": false,
            "search": false,
        }));
        let role = RoleInstance::new(&definition, &values);

        let extensions = role.extensions().unwrap();
        let fetch = role.tool(&"fetch".into()).unwrap().unwrap();
        let model_error = role.model().unwrap_err();
        let search_error = role.tool(&"search".into()).unwrap_err();

        assert_eq!(extensions, definition.extensions);
        assert_eq!(fetch.config, JsonObject::new());
        assert_eq!(
            model_error.to_string(),
            "parameter model must be an object when targeting the root",
        );
        assert_eq!(
            search_error.to_string(),
            "parameter search must be an object when targeting the root",
        );
    }
}
