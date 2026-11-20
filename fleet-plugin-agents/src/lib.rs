//! WebAssembly tools for scoped agent execution.

mod bindings;
mod config;
mod workspaces;

pub(crate) use self::{
    bindings::{
        exports::fleet::providers::tools::{Guest, ToolsError, ToolsErrorKind},
        fleet::providers::{
            agents::{self, AgentsError, EventDirection, EventPayload, EventQuery, TurnEnd},
            blobs::{self, BlobsErrorKind},
            roles,
            types::{
                AgentId, BlobId, Configured, ContentBlock, ModelOutput, Tool, ToolContext,
                ToolOutput,
            },
        },
    },
    config::Config,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

const IMAGE_COUNT_LIMIT: usize = 20;
const IMAGE_SIZE_LIMIT: u64 = 40 * 1024 * 1024;

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
    #[serde(default)]
    images: Vec<String>,
    title: Option<String>,
}

impl Guest for Agents {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Tool>, ToolsError> {
        Ok(match name.as_str() {
            "oneshot_agent" => Some(metadata(
                "oneshot_agent",
                "Delegate to agent",
                "Runs a task in a new agent and returns its final response. The agent finishes \
                 when this call ends. Images it mentions by reference are returned with the \
                 response.",
                false,
            )),
            "oneshot_agent_async" => Some(metadata(
                "oneshot_agent_async",
                "Delegate to agent in background",
                "Runs a task in a new agent and continues without waiting. Its final response \
                 arrives as this call's result. The agent finishes when this call ends. Images \
                 it mentions by reference are returned with the response.",
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
    let role = roles::get(input.role.clone())
        .await
        .map_err(|error| service_error("failed to read role", &error.message))?;
    let role = match role {
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
            }
        )));
    }
    let mut extensions = Map::new();
    if let Some(title) = input.title {
        extensions.insert("fleet.title".into(), title.into());
    }
    if input.images.len() > IMAGE_COUNT_LIMIT {
        return Err(Error::Input(format!(
            "Attach at most {IMAGE_COUNT_LIMIT} images."
        )));
    }
    let mut prompt = vec![ContentBlock::Text(input.prompt)];
    let mut size = 0_u64;
    for reference in &input.images {
        let image = match reference.parse() {
            Ok(id) => image_size(id).await?.map(|size| (id, size)),
            Err(_) => None,
        };
        let Some((id, image_size)) = image else {
            return Err(Error::Input(format!(
                "Image reference {reference} is not an available image."
            )));
        };
        size = size.saturating_add(image_size);
        prompt.push(ContentBlock::Image(blob_id(id)));
    }
    if size > IMAGE_SIZE_LIMIT {
        return Err(Error::Input("Attach at most 40 MiB of images.".into()));
    }
    let agent = agents::create(
        "agent".into(),
        Configured {
            name: role.name,
            config: Value::Object(input.parameters).to_string(),
        },
        Value::Object(extensions).to_string(),
    )
    .await
    .map_err(|error| agents_error("failed to create agent", &error))?;
    let message = agents::send("message".into(), agent, prompt)
        .await
        .map_err(|error| agents_error("failed to queue agent message", &error))?;
    response(agent, message).await
}

async fn response(agent: AgentId, message: agents::MessageId) -> Result<ToolOutput, Error> {
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
            return Ok(error_output("The agent was interrupted before finishing."));
        }
    };
    let mut content = images(&text).await?;
    content.insert(0, ContentBlock::Text(text));
    Ok(ToolOutput {
        content,
        structured_content: None,
        is_error: false,
    })
}

async fn images(text: &str) -> Result<Vec<ContentBlock>, Error> {
    let mut images = Vec::new();
    let mut seen = BTreeSet::new();
    let mut size = 0_u64;
    for reference in text.split(|c: char| !c.is_ascii_digit()) {
        let Ok(id) = reference.parse() else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        if let Some(image_size) = image_size(id).await? {
            size = size.saturating_add(image_size);
            if images.len() == IMAGE_COUNT_LIMIT || size > IMAGE_SIZE_LIMIT {
                break;
            }
            images.push(ContentBlock::Image(blob_id(id)));
        }
    }
    Ok(images)
}

async fn image_size(id: u128) -> Result<Option<u64>, Error> {
    match blobs::get(blob_id(id)).await {
        Ok(metadata) => Ok(metadata
            .filter(|metadata| metadata.image.is_some())
            .map(|metadata| metadata.size)),
        Err(error)
            if matches!(
                error.kind,
                BlobsErrorKind::NotFound | BlobsErrorKind::Denied
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(service_error("failed to read image", &error.message)),
    }
}

fn blob_id(id: u128) -> BlobId {
    BlobId {
        high: u64::try_from(id >> 64).expect("high bits fit in u64"),
        low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit in u64"),
    }
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

fn agents_error(context: &str, error: &AgentsError) -> Error {
    service_error(context, &error.message)
}

fn service_error(context: &str, error: &impl core::fmt::Display) -> Error {
    Error::Tools(ToolsError {
        kind: ToolsErrorKind::Other,
        message: format!("{context}: {error}"),
    })
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
                "images": {
                    "type": "array",
                    "items": {
                        "type": "string",
                    },
                    "maxItems": IMAGE_COUNT_LIMIT,
                    "description": "Image references to attach after the prompt.",
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
        output_schema: None,
        extensions: json!({
            "fleet.title": title,
        })
        .to_string(),
    }
}

#[cfg(target_arch = "wasm32")]
bindings::export!(Agents with_types_in bindings);
