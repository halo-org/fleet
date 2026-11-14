use crate::{
    ToolContext, ToolsError, ToolsErrorKind, WorkspaceContext,
    bindings::fleet::providers::{configuration, types::Configured, workspaces::WorkspaceId},
};
use fleet_kit::{Extension as _, RoleWorkspace};
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

pub fn workspace(context: &ToolContext) -> Result<WorkspaceContext, ToolsError> {
    let extensions = serde_json::from_str(&context.role_extensions).map_err(error)?;
    let workspace = RoleWorkspace::get(&extensions)
        .map_err(error)?
        .ok_or_else(|| error("no workspace is configured in the role"))?;
    let id = workspace.id.map_or(
        WorkspaceId {
            high: context.agent_id.high,
            low: context.agent_id.low,
        },
        |id| {
            let id = u128::from(id);
            WorkspaceId {
                high: u64::try_from(id >> 64).expect("high bits fit in u64"),
                low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit in u64"),
            }
        },
    );
    Ok(WorkspaceContext {
        workspace: Configured {
            name: workspace.name.into(),
            config: serde_json::to_string(&workspace.config).map_err(error)?,
        },
        agent_id: context.agent_id,
        id,
    })
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
    use crate::bindings::fleet::providers::types::AgentId;
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

    #[test]
    fn workspace_uses_agent_id_unless_shared() {
        let mut extensions = json!({
            "fleet.workspace": {
                "name": "host:default",
                "config": {
                    "cwd": "/project",
                },
            },
        });
        let mut context = ToolContext {
            tool: Configured {
                name: "read".into(),
                config: "{}".into(),
            },
            agent_id: AgentId { high: 1, low: 2 },
            call_id: "call".into(),
            role_extensions: extensions.to_string(),
        };

        let environment = workspace(&context).unwrap();

        assert_eq!(environment.id.high, 1);
        assert_eq!(environment.id.low, 2);

        extensions["fleet.workspace"]["id"] = "55340232221128654852".into();
        context.role_extensions = extensions.to_string();
        let environment = workspace(&context).unwrap();

        assert_eq!(environment.id.high, 3);
        assert_eq!(environment.id.low, 4);
        assert_eq!(environment.agent_id.high, 1);
        assert_eq!(environment.agent_id.low, 2);
    }
}
