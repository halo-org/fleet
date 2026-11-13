//! WebAssembly tools for reading and writing roles.

mod bindings;
mod config;
mod edit;
mod role;

pub(crate) use self::{
    bindings::{
        exports::fleet::providers::tools::{Guest, ToolsError, ToolsErrorKind},
        fleet::providers::{
            roles::{self, RolesError, RolesErrorKind},
            types::{Configured, ContentBlock, Tool, ToolContext, ToolOutput},
        },
    },
    config::Config,
    edit::TextEdit,
    role::Role,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

struct Roles;

enum Error {
    Input(String),
    Tools(ToolsError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the input is an empty object"
)]
struct ListInput {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    role: Role,
    version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    name: String,
    version: Option<String>,
    #[serde(default)]
    remove: Vec<String>,
    #[serde(default)]
    merge: Map<String, Value>,
    #[serde(default)]
    system_prompt_edits: Vec<TextEdit>,
}

#[derive(Serialize)]
struct ListedRole {
    name: String,
    version: String,
}

#[derive(Serialize)]
struct ReadOutput {
    version: String,
    role: Role,
}

impl Guest for Roles {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Tool>, ToolsError> {
        Ok(match name.as_str() {
            "list_roles" => Some(metadata(
                "list_roles",
                "List roles",
                "Lists the stored roles with their versions.",
                &json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
            )),
            "read_role" => Some(metadata(
                "read_role",
                "Read role",
                "Returns a role and its version.",
                &json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The role name.",
                        },
                    },
                    "required": ["name"],
                    "additionalProperties": false,
                }),
            )),
            "write_role" => Some(metadata(
                "write_role",
                "Write role",
                "Creates or replaces a role. Omit version to create a role. To replace a role, \
                 pass the version from read_role. Agents with the role use the change from \
                 their next turn.",
                &json!({
                    "type": "object",
                    "properties": {
                        "role": crate::role::schema(),
                        "version": {
                            "type": "string",
                            "description": "The version from read_role of the role being replaced.",
                        },
                    },
                    "required": ["role"],
                    "additionalProperties": false,
                }),
            )),
            "edit_role" => Some(metadata(
                "edit_role",
                "Edit role",
                "Edits part of a role. Removals apply first, then the merge, then the system \
                 prompt edits. Pass version to fail if the role changed since read_role. Agents \
                 with the role use the change from their next turn.",
                &crate::edit::schema(),
            )),
            _ => None,
        })
    }

    async fn execute(
        context: ToolContext,
        is_retry: bool,
        input: String,
    ) -> Result<ToolOutput, ToolsError> {
        let result = match context.tool.name.as_str() {
            "list_roles" => list(&input).await,
            "read_role" => read(&input).await,
            "write_role" => write(&context, &input).await,
            "edit_role" => edit(&context, is_retry, &input).await,
            _ => return Ok(error_output("The tool is not available.")),
        };
        match result {
            Ok(output) => Ok(output),
            Err(Error::Input(message)) => Ok(error_output(message)),
            Err(Error::Tools(error)) => Err(error),
        }
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn interrupted_output(
        context: ToolContext,
        _input: String,
    ) -> Result<ToolOutput, ToolsError> {
        Ok(error_output(match context.tool.name.as_str() {
            "write_role" => {
                "The tool call was interrupted. The role may already have been written."
            }
            "edit_role" => "The tool call was interrupted. The role may already have been edited.",
            _ => "The tool call was interrupted.",
        }))
    }
}

async fn list(input: &str) -> Result<ToolOutput, Error> {
    let ListInput {} = serde_json::from_str(input)?;
    let roles = roles::list()
        .await
        .map_err(|error| service_error("failed to list roles", &error))?;
    let roles: Vec<_> = roles
        .into_iter()
        .map(|stored| ListedRole {
            name: stored.role.name,
            version: crate::role::version(stored.version),
        })
        .collect();
    Ok(json_output(&roles))
}

async fn read(input: &str) -> Result<ToolOutput, Error> {
    let input: ReadInput = serde_json::from_str(input)?;
    let (role, version) = current(&input.name).await?;
    Ok(json_output(&ReadOutput {
        version: crate::role::version(version),
        role,
    }))
}

async fn write(context: &ToolContext, input: &str) -> Result<ToolOutput, Error> {
    let config = Config::load().map_err(Error::Tools)?;
    let input: WriteInput = serde_json::from_str(input)?;
    let name = input.role.name().to_owned();
    if !config.writable(&name) {
        return Err(Error::Input(format!("Writing role {name} is not allowed.")));
    }
    let expected = input.version.as_deref().map(parse_version).transpose()?;
    match roles::put(context.call_id.clone(), input.role.clone().into(), expected).await {
        Ok(()) => Ok(written_output(&input.role).await),
        Err(error) => Err(match error.kind {
            RolesErrorKind::PreconditionFailed if input.version.is_some() => Error::Input(format!(
                "Role {name} changed or was deleted since it was read. Read it again and \
                     reapply the changes."
            )),
            RolesErrorKind::PreconditionFailed => Error::Input(format!(
                "Role {name} already exists. Read it to get its version."
            )),
            RolesErrorKind::InvalidRole => Error::Input(format!("Invalid role: {}", error.message)),
            RolesErrorKind::IdempotencyConflict | RolesErrorKind::Other => {
                service_error("failed to write role", &error)
            }
        }),
    }
}

async fn edit(context: &ToolContext, is_retry: bool, input: &str) -> Result<ToolOutput, Error> {
    let config = Config::load().map_err(Error::Tools)?;
    let input: EditInput = serde_json::from_str(input)?;
    let name = input.name;
    if !config.writable(&name) {
        return Err(Error::Input(format!("Writing role {name} is not allowed.")));
    }
    let expected = input.version.as_deref().map(parse_version).transpose()?;
    let retry_note = if is_retry {
        " An earlier attempt of this call may already have edited it. Read it to check."
    } else {
        ""
    };
    let (role, version) = current(&name).await?;
    if expected.is_some_and(|expected| (expected.high, expected.low) != (version.high, version.low))
    {
        return Err(Error::Input(format!(
            "Role {name} changed since it was read. Read it again and reapply the changes.\
             {retry_note}"
        )));
    }
    let edited = crate::edit::apply(
        &role,
        &input.remove,
        &input.merge,
        &input.system_prompt_edits,
    )
    .map_err(|message| Error::Input(format!("{message}{retry_note}")))?;
    match roles::put(
        context.call_id.clone(),
        edited.clone().into(),
        Some(version),
    )
    .await
    {
        Ok(()) => Ok(written_output(&edited).await),
        Err(error) => match error.kind {
            RolesErrorKind::PreconditionFailed => Err(Error::Input(format!(
                "Role {name} changed during the edit. Read it again and reapply the changes."
            ))),
            RolesErrorKind::IdempotencyConflict => Ok(success_output(format!(
                "An earlier attempt of this call already edited role {name}. Read it to get its \
                 version."
            ))),
            RolesErrorKind::InvalidRole => {
                Err(Error::Input(format!("Invalid role: {}", error.message)))
            }
            RolesErrorKind::Other => Err(service_error("failed to write role", &error)),
        },
    }
}

async fn current(name: &str) -> Result<(Role, roles::RoleVersion), Error> {
    let stored = roles::get(name.to_owned())
        .await
        .map_err(|error| service_error("failed to read role", &error))?
        .ok_or_else(|| Error::Input(format!("Role {name} does not exist.")))?;
    let role = Role::try_from(stored.role).map_err(|error| {
        Error::Tools(ToolsError {
            kind: ToolsErrorKind::Other,
            message: format!("host returned an invalid role: {error}"),
        })
    })?;
    Ok((role, stored.version))
}

async fn written_output(role: &Role) -> ToolOutput {
    let name = role.name();
    let stored = roles::get(name.to_owned()).await.map(|stored| {
        stored.and_then(|stored| Some((Role::try_from(stored.role).ok()?, stored.version)))
    });
    success_output(match stored {
        Ok(Some((stored, version))) if stored == *role => format!(
            "Role {name} was written. Its version is {}.",
            crate::role::version(version)
        ),
        Ok(_) => format!("Role {name} was written but has changed since. Read it again."),
        Err(_) => format!("Role {name} was written. Read it again to get its version."),
    })
}

fn parse_version(version: &str) -> Result<roles::RoleVersion, Error> {
    crate::role::parse_version(version)
        .ok_or_else(|| Error::Input("The version must come from read_role.".into()))
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Input(error.to_string())
    }
}

fn service_error(context: &str, error: &RolesError) -> Error {
    Error::Tools(ToolsError {
        kind: ToolsErrorKind::Other,
        message: format!("{context}: {}", error.message),
    })
}

fn json_output(value: &impl Serialize) -> ToolOutput {
    success_output(serde_json::to_string_pretty(value).expect("outputs can be serialized"))
}

fn success_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(text.into())],
        structured_content: None,
        is_error: false,
    }
}

fn error_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        is_error: true,
        ..success_output(text)
    }
}

fn metadata(name: &str, title: &str, description: &str, schema: &Value) -> Tool {
    Tool {
        name: name.into(),
        description: Some(description.into()),
        is_async: false,
        input_schema: schema.to_string(),
        output_schema: None,
        extensions: json!({
            "fleet.title": title,
        })
        .to_string(),
    }
}

#[cfg(target_arch = "wasm32")]
bindings::export!(Roles with_types_in bindings);
