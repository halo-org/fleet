use crate::{ToolContext, ToolsError, ToolsErrorKind, bindings::fleet::providers::configuration};
use serde::Deserialize;

/// The provider or tool config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    roles: Option<Vec<String>>,
}

impl Config {
    pub fn load(context: &ToolContext) -> Result<Self, ToolsError> {
        Self::parse(&configuration::get(), &context.tool.config)
    }

    fn parse(provider: &str, tool: &str) -> Result<Self, ToolsError> {
        let provider: Self = serde_json::from_str(provider).map_err(error)?;
        let tool: Self = serde_json::from_str(tool).map_err(error)?;
        Ok(Self {
            roles: tool.roles.or(provider.roles),
        })
    }

    /// Returns whether agents may be created with the role.
    pub fn allows(&self, name: &str) -> bool {
        self.roles.as_ref().is_none_or(|patterns| {
            patterns.iter().any(|pattern| {
                pattern
                    .strip_suffix('*')
                    .map_or_else(|| pattern == name, |prefix| name.starts_with(prefix))
            })
        })
    }
}

fn error(error: impl core::fmt::Display) -> ToolsError {
    ToolsError {
        kind: ToolsErrorKind::Other,
        message: format!("invalid config: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_roles_override_provider_roles() {
        let config = Config::parse(r#"{ "roles": ["reviewer"] }"#, "{}").unwrap();

        assert!(config.allows("reviewer"));
        assert!(!config.allows("assistant"));

        let config =
            Config::parse(r#"{ "roles": ["reviewer"] }"#, r#"{ "roles": ["work-*"] }"#).unwrap();

        assert!(config.allows("work-review"));
        assert!(!config.allows("reviewer"));
        assert!(Config::parse("{}", "{}").unwrap().allows("assistant"));
    }
}
