use crate::{
    AgentId, Agents, Configured,
    bindings::{
        exports::fleet::providers::workspace_resolvers::{
            Guest, WorkspaceResolution, WorkspaceSource,
        },
        fleet::providers::{
            agents,
            types::{Workspace, WorkspacesError, WorkspacesErrorKind},
        },
    },
};
use fleet_kit::{AgentParent, Extension as _};
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    directory: Option<String>,
}

impl Guest for Agents {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Workspace>, WorkspacesError> {
        Ok((name == "parent").then(|| Workspace {
            name,
            extensions: "{}".into(),
        }))
    }

    async fn resolve(
        subject: AgentId,
        workspace: Configured,
    ) -> Result<WorkspaceResolution, WorkspacesError> {
        if workspace.name != "parent" {
            return Err(WorkspacesError {
                kind: WorkspacesErrorKind::NotFound,
                message: "workspace resolver not found".into(),
            });
        }
        let config: Config = serde_json::from_str(&workspace.config).map_err(invalid)?;
        let agent = agents::get(subject)
            .await
            .map_err(|error| WorkspacesError {
                kind: WorkspacesErrorKind::Other,
                message: error.message,
            })?;
        let extensions = serde_json::from_str(&agent.extensions).map_err(invalid)?;
        let parent = AgentParent::get(&extensions)
            .map_err(invalid)?
            .ok_or_else(|| invalid("agent has no parent"))?;
        let id = u128::from(parent.agent);
        Ok(WorkspaceResolution {
            source: WorkspaceSource::Agent(AgentId {
                high: u64::try_from(id >> 64).expect("high bits fit"),
                low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit"),
            }),
            directory: config.directory,
        })
    }
}

fn invalid(error: impl core::fmt::Display) -> WorkspacesError {
    WorkspacesError {
        kind: WorkspacesErrorKind::InvalidConfiguration,
        message: error.to_string(),
    }
}
