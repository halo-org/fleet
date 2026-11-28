use crate::{ToolContext, ToolsError, ToolsErrorKind, bindings::fleet::host::configuration};
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub shell: Option<String>,
    pub timeout_ms: Option<u64>,
    pub output_limit: Option<u64>,
    pub read_limit: Option<u64>,
}

impl Config {
    pub fn load(context: &ToolContext) -> Result<Self, ToolsError> {
        Self::parse(&configuration::get(), &context.tool.config)
    }

    fn parse(provider: &str, tool: &str) -> Result<Self, ToolsError> {
        let provider: Self = serde_json::from_str(provider).map_err(error)?;
        let tool: Self = serde_json::from_str(tool).map_err(error)?;
        Ok(Self {
            shell: tool.shell.or(provider.shell),
            timeout_ms: tool.timeout_ms.or(provider.timeout_ms),
            output_limit: tool.output_limit.or(provider.output_limit),
            read_limit: tool.read_limit.or(provider.read_limit),
        })
    }
}

fn error(error: impl core::fmt::Display) -> ToolsError {
    ToolsError {
        kind: ToolsErrorKind::Other,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_preserves_settings_without_overrides() {
        let provider = json!({
            "shell": "/bin/bash",
            "timeout_ms": 1000,
            "output_limit": 512,
        })
        .to_string();
        let tool = json!({
            "timeout_ms": 2000,
        })
        .to_string();

        let config = Config::parse(&provider, &tool).unwrap();

        assert_eq!(config.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(config.timeout_ms, Some(2000));
        assert_eq!(config.output_limit, Some(512));
    }
}
