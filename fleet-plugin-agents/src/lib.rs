//! WebAssembly tools for scoped agent execution.

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
use serde::Deserialize;
use serde_json::{Map, Value, json};

struct Agents;

enum Error {
    Input(String),
    Tools(ToolsError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    role: Option<String>,
    agent: Option<String>,
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
                "Queues a task in a new or existing agent and returns the first final response after \
                 that message is consumed. Supply a role to create or an agent ID to resume. \
                 Remaining background work is stopped when this call ends.",
                false,
            )),
            "oneshot_agent_async" => Some(metadata(
                "oneshot_agent_async",
                "Delegate to agent in background",
                "Queues a task in a new or existing agent and continues without waiting. The first \
                 final response after the message is consumed arrives as this call's result. \
                 Remaining background work is stopped when this call ends.",
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
            "The tool call was interrupted. Its remaining agent work is being stopped.",
        ))
    }
}

async fn oneshot(context: &ToolContext, input: &str) -> Result<ToolOutput, Error> {
    let config = Config::load(context).map_err(Error::Tools)?;
    let input: Input = serde_json::from_str(input)?;
    let agent =
        match (&input.role, &input.agent) {
            (Some(name), None) => {
                let role = roles::get(name.clone())
                    .await
                    .map_err(|error| service_error("failed to read role", &error.message))?;
                let role = match role {
                    Some(stored) if config.allows(&stored.role.name) => stored.role,
                    _ => return Err(Error::Input(unknown_role(&config, name).await?)),
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
                        }
                    )));
                }
                let mut extensions = Map::new();
                if let Some(title) = input.title {
                    extensions.insert("fleet.title".into(), title.into());
                }
                agents::create(
                    "agent".into(),
                    Configured {
                        name: role.name,
                        config: Value::Object(input.parameters).to_string(),
                    },
                    Value::Object(extensions).to_string(),
                )
                .await
                .map_err(|error| agents_error("failed to create agent", &error))?
            }
            (None, Some(id)) if input.parameters.is_empty() && input.title.is_none() => {
                let id = id
                    .parse::<u128>()
                    .map_err(|_| Error::Input("agent must be a decimal ID".into()))?;
                let id = AgentId {
                    high: u64::try_from(id >> 64).expect("high bits fit"),
                    low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit"),
                };
                let agent = agents::get(id)
                    .await
                    .map_err(|error| agents_error("failed to read agent", &error))?;
                if !config.allows(&agent.role.name) {
                    return Err(Error::Input(unknown_role(&config, &agent.role.name).await?));
                }
                id
            }
            _ => return Err(Error::Input(
                "Supply either role (with optional parameters and title) or an existing agent ID."
                    .into(),
            )),
        };
    let message = agents::send(
        "message".into(),
        agent,
        vec![ContentBlock::Text(input.prompt)],
    )
    .await
    .map_err(|error| agents_error("failed to queue agent message", &error))?;
    response(agent, message).await
}

async fn response(agent: AgentId, message: agents::MessageId) -> Result<ToolOutput, Error> {
    let id = decimal(agent);
    let end = agents::wait_for_turn_end(agent, message)
        .await
        .map_err(|error| agents_error("failed to wait for agent", &error))?;
    let text = match end.payload {
        TurnEnd::Completed => {
            let events = agents::events(
                agent,
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
                "agent": {
                    "type": "string",
                    "description": "An existing child agent ID to resume instead of creating one.",
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
            "required": ["prompt"],
            "oneOf": [{ "required": ["role"] }, { "required": ["agent"] }],
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
