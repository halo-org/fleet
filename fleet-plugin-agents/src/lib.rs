//! WebAssembly tools that delegate tasks to new agents.

mod bindings;
mod config;
mod workspaces;

pub(crate) use self::{
    bindings::{
        exports::fleet::providers::tools::{Guest, ToolsError, ToolsErrorKind},
        fleet::providers::{
            agents::{self, AgentsError, EventDirection, EventPayload, EventQuery, TurnEnd},
            roles,
            types::{
                AgentId, Configured, ContentBlock, ModelOutput, Tool, ToolContext, ToolOutput,
            },
        },
    },
    config::Config,
};
use fleet_kit::{Extension as _, RoleWorkspace, WorkspaceId};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// The extension holding the workspace inherited from the parent agent.
const PARENT_WORKSPACE: &str = "fleet.parent_workspace";

struct Agents;

enum Error {
    Input(String),
    Tools(ToolsError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    role: String,
    #[serde(default)]
    parameters: Map<String, Value>,
    prompt: String,
    title: Option<String>,
}

impl Guest for Agents {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Tool>, ToolsError> {
        Ok(match name.as_str() {
            "oneshot_agent" => Some(metadata(
                "oneshot_agent",
                "Delegate to agent",
                "Runs a task with a new agent and returns its final response. The agent starts \
                 with only the prompt and its role. An unknown role returns the available roles.",
                false,
            )),
            "oneshot_agent_async" => Some(metadata(
                "oneshot_agent_async",
                "Delegate to agent in background",
                "Starts a task with a new agent and continues without waiting. The agent's final \
                 response arrives as this call's result. The agent starts with only the prompt and \
                 its role. An unknown role returns the available roles.",
                true,
            )),
            _ => None,
        })
    }

    async fn execute(
        context: ToolContext,
        _is_retry: bool,
        input: String,
    ) -> Result<ToolOutput, ToolsError> {
        if !matches!(
            context.tool.name.as_str(),
            "oneshot_agent" | "oneshot_agent_async"
        ) {
            return Ok(error_output("The tool is not available."));
        }
        match oneshot(&context, &input).await {
            Ok(output) => Ok(output),
            Err(Error::Input(message)) => Ok(error_output(message)),
            Err(Error::Tools(error)) => Err(error),
        }
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn interrupted_output(
        _context: ToolContext,
        _input: String,
    ) -> Result<ToolOutput, ToolsError> {
        Ok(error_output(
            "The tool call was interrupted. The agent was interrupted too.",
        ))
    }
}

async fn oneshot(context: &ToolContext, input: &str) -> Result<ToolOutput, Error> {
    let config = Config::load(context).map_err(Error::Tools)?;
    let input: Input = serde_json::from_str(input)?;
    let role = match roles::get(input.role.clone())
        .await
        .map_err(|error| service_error("failed to read role", &error.message))?
    {
        Some(stored) if config.allows(&stored.role.name) => stored.role,
        _ => return Err(Error::Input(unknown_role(&config, &input.role).await?)),
    };
    if let Some(name) = input.parameters.keys().find(|name| {
        !role
            .parameters
            .iter()
            .any(|parameter| parameter.name == **name)
    }) {
        let names: Vec<_> = role
            .parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        return Err(Error::Input(format!(
            "Role {} has no parameter {name}. Its parameters are: {}.",
            role.name,
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            },
        )));
    }

    let mut extensions = Map::new();
    if let Some(title) = input.title {
        extensions.insert("fleet.title".into(), title.into());
    }
    if let Some(workspace) = inherited_workspace(context).await? {
        extensions.insert(
            PARENT_WORKSPACE.into(),
            serde_json::to_value(workspace).expect("workspaces are JSON"),
        );
    }
    let created = agents::create(
        Configured {
            name: role.name,
            config: Value::Object(input.parameters).to_string(),
        },
        Value::Object(extensions).to_string(),
        vec![ContentBlock::Text(input.prompt)],
    )
    .await
    .map_err(|error| agents_error("failed to create agent", &error))?;
    let id = decimal(created.agent);
    let end = agents::wait_for_turn_end(created.agent, created.message)
        .await
        .map_err(|error| agents_error("failed to wait for agent", &error))?;
    let text = match end.payload {
        TurnEnd::Completed => {
            let events = agents::events(
                created.agent,
                EventQuery {
                    from: Some(end.cursor),
                    until: None,
                    direction: EventDirection::Backward,
                    limit: 1,
                },
            )
            .await
            .map_err(|error| agents_error("failed to read agent response", &error))?;
            let text = events.into_iter().find_map(|event| match event.payload {
                EventPayload::ModelResponse(response) => Some(
                    response
                        .output
                        .into_iter()
                        .filter_map(|output| match output {
                            ModelOutput::Text(text) => Some(text),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            });
            text.filter(|text| !text.is_empty())
                .unwrap_or_else(|| "The agent finished without a text response.".into())
        }
        TurnEnd::Interrupted => {
            return Ok(ToolOutput {
                is_error: true,
                ..agent_output(&id, "The agent was interrupted before finishing.".into())
            });
        }
    };
    Ok(agent_output(&id, text))
}

async fn unknown_role(config: &Config, name: &str) -> Result<String, Error> {
    let roles = roles::list()
        .await
        .map_err(|error| service_error("failed to list roles", &error.message))?;
    let roles: Vec<_> = roles
        .into_iter()
        .filter(|stored| config.allows(&stored.role.name))
        .map(|stored| {
            let extensions: Map<String, Value> =
                serde_json::from_str(&stored.role.extensions).unwrap_or_default();
            let parameters: Vec<_> = stored
                .role
                .parameters
                .into_iter()
                .map(|parameter| parameter.name)
                .collect();
            json!({
                "name": stored.role.name,
                "description": extensions.get("fleet.description"),
                "parameters": parameters,
            })
        })
        .collect();
    Ok(format!(
        "Role {name} is not available. Available roles:\n{}",
        serde_json::to_string_pretty(&roles).expect("roles are JSON"),
    ))
}

/// Returns the workspace a new agent shares with the calling agent.
///
/// A workspace from this plugin resolves to the one the calling agent inherited.
async fn inherited_workspace(context: &ToolContext) -> Result<Option<RoleWorkspace>, Error> {
    let extensions = serde_json::from_str(&context.role_extensions)
        .map_err(|error| service_error("invalid role extensions", &error))?;
    let Some(workspace) = RoleWorkspace::get(&extensions)
        .map_err(|error| service_error("invalid role workspace", &error))?
    else {
        return Ok(None);
    };
    if crate::bindings::fleet::providers::workspaces::get(workspace.name.to_string())
        .await
        .map_err(|error| service_error("failed to read workspace", &error.message))?
        .is_some()
    {
        let id = workspace
            .id
            .unwrap_or_else(|| WorkspaceId::from(u128::from(context.agent_id)));
        return Ok(Some(RoleWorkspace {
            id: Some(id),
            ..workspace
        }));
    }
    if !workspace.name.as_ref().ends_with(":parent") {
        return Ok(None);
    }
    crate::workspaces::parent(context.agent_id)
        .await
        .map_err(|error| service_error("failed to read parent workspace", &error.message))
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Input(error.to_string())
    }
}

impl From<AgentId> for u128 {
    fn from(id: AgentId) -> Self {
        (Self::from(id.high) << 64) | Self::from(id.low)
    }
}

fn decimal(id: AgentId) -> String {
    u128::from(id).to_string()
}

fn agents_error(context: &str, error: &AgentsError) -> Error {
    service_error(context, &error.message)
}

fn service_error(context: &str, error: &impl core::fmt::Display) -> Error {
    Error::Tools(ToolsError {
        kind: ToolsErrorKind::Other,
        message: format!("{context}: {error}"),
    })
}

fn agent_output(id: &str, text: String) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(text)],
        structured_content: Some(json!({ "agent": id }).to_string()),
        is_error: false,
    }
}

fn error_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(text.into())],
        structured_content: None,
        is_error: true,
    }
}

fn metadata(name: &str, title: &str, description: &str, is_async: bool) -> Tool {
    Tool {
        name: name.into(),
        description: Some(description.into()),
        is_async,
        input_schema: json!({
            "type": "object",
            "properties": {
                "role": {
                    "type": "string",
                    "description": "The role of the new agent.",
                },
                "parameters": {
                    "type": "object",
                    "description": "Values for the role's parameters.",
                },
                "prompt": {
                    "type": "string",
                    "description": "The complete task. The agent cannot see this conversation.",
                },
                "title": {
                    "type": "string",
                    "description": "A short title for the agent.",
                },
            },
            "required": ["role", "prompt"],
            "additionalProperties": false,
        })
        .to_string(),
        output_schema: Some(
            json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "The ID of the agent.",
                    },
                },
                "required": ["agent"],
                "additionalProperties": false,
            })
            .to_string(),
        ),
        extensions: json!({
            "fleet.title": title,
        })
        .to_string(),
    }
}

#[cfg(target_arch = "wasm32")]
bindings::export!(Agents with_types_in bindings);
