use crate::{ToolsError, ToolsErrorKind, bindings::fleet::providers::configuration};
use serde::Deserialize;

/// The provider config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    write: Option<Vec<String>>,
}

impl Config {
    pub fn load() -> Result<Self, ToolsError> {
        Self::parse(&configuration::get())
    }

    fn parse(config: &str) -> Result<Self, ToolsError> {
        serde_json::from_str(config).map_err(|error| ToolsError {
            kind: ToolsErrorKind::Other,
            message: format!("invalid provider config: {error}"),
        })
    }

    /// Returns whether the provider may write the role.
    pub fn writable(&self, name: &str) -> bool {
        self.write.as_ref().is_none_or(|patterns| {
            patterns.iter().any(|pattern| {
                pattern
                    .strip_suffix('*')
                    .map_or_else(|| pattern == name, |prefix| name.starts_with(prefix))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_patterns_match_names_and_prefixes() {
        let config = Config::parse(r#"{ "write": ["assistant", "drafts-*"] }"#).unwrap();

        assert!(config.writable("assistant"));
        assert!(config.writable("drafts-review"));
        assert!(!config.writable("assistant-2"));
        assert!(!config.writable("drafts"));
    }

    #[test]
    fn write_defaults_to_every_role() {
        assert!(Config::parse("{}").unwrap().writable("assistant"));
        assert!(
            !Config::parse(r#"{ "write": [] }"#)
                .unwrap()
                .writable("assistant")
        );
    }
}
