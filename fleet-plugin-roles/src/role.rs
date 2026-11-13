use crate::{Configured, roles};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// A role in the shape models read and write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    name: String,
    model: Selection,
    #[serde(default)]
    tools: BTreeMap<String, Selection>,
    #[serde(default)]
    system_prompt: String,
    #[serde(default)]
    parameters: BTreeMap<String, Parameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction: Option<Compaction>,
    #[serde(default)]
    extensions: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    name: String,
    #[serde(default)]
    config: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameter {
    target: Target,
    #[serde(default)]
    extensions: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Model { path: Vec<String> },
    Tool { tool: String, path: Vec<String> },
    Extensions { path: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Compaction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    threshold: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
}

impl Role {
    /// Returns the role name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Returns the JSON Schema of a role.
pub fn schema() -> Value {
    let selection = |description: &str| {
        json!({
            "type": "object",
            "description": description,
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The provider namespace and item, such as provider:item.",
                },
                "config": {
                    "type": "object",
                    "description": "Config passed to the provider.",
                },
            },
            "required": ["name"],
            "additionalProperties": false,
        })
    };
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "The role name. Use only ASCII letters, digits, - and _.",
            },
            "model": selection("The model and its config."),
            "tools": {
                "type": "object",
                "description": "Tools by the name the model sees.",
                "additionalProperties": selection("The tool and its config."),
            },
            "system_prompt": {
                "type": "string",
            },
            "parameters": {
                "type": "object",
                "description": "Values that can be set when an agent is created, by parameter name.",
                "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "object",
                            "description": "The config or extension value the parameter replaces.",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": ["model", "tool", "extensions"],
                                },
                                "tool": {
                                    "type": "string",
                                    "description": "The tool name in tools. Required for tool targets.",
                                },
                                "path": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "The object keys leading to the value.",
                                },
                            },
                            "required": ["type", "path"],
                            "additionalProperties": false,
                        },
                        "extensions": {
                            "type": "object",
                            "description": "Parameter metadata such as fleet.title, fleet.description and fleet.schema.",
                        },
                    },
                    "required": ["target"],
                    "additionalProperties": false,
                },
            },
            "compaction": {
                "type": "object",
                "properties": {
                    "threshold": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The input token count that triggers compaction.",
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Custom summarization instructions.",
                    },
                },
                "additionalProperties": false,
            },
            "extensions": {
                "type": "object",
                "description": "Role metadata such as fleet.title, fleet.description, fleet.hidden and fleet.workspace.",
            },
        },
        "required": ["name", "model"],
        "additionalProperties": false,
    })
}

/// Formats a role version as a decimal string.
pub fn version(version: roles::RoleVersion) -> String {
    ((u128::from(version.high) << 64) | u128::from(version.low)).to_string()
}

/// Parses a role version from a decimal string.
pub fn parse_version(value: &str) -> Option<roles::RoleVersion> {
    let version = value.parse::<u128>().ok()?;
    Some(roles::RoleVersion {
        high: u64::try_from(version >> 64).expect("high bits fit in u64"),
        low: u64::try_from(version & u128::from(u64::MAX)).expect("low bits fit in u64"),
    })
}

impl TryFrom<roles::Role> for Role {
    type Error = serde_json::Error;

    fn try_from(role: roles::Role) -> Result<Self, Self::Error> {
        Ok(Self {
            name: role.name,
            model: role.model.try_into()?,
            tools: role
                .tools
                .into_iter()
                .map(|tool| Ok((tool.alias, tool.tool.try_into()?)))
                .collect::<Result<_, Self::Error>>()?,
            system_prompt: role.system_prompt,
            parameters: role
                .parameters
                .into_iter()
                .map(|parameter| {
                    Ok((
                        parameter.name,
                        Parameter {
                            target: match parameter.target {
                                roles::RoleParameterTarget::Model(path) => Target::Model { path },
                                roles::RoleParameterTarget::Tool((tool, path)) => {
                                    Target::Tool { tool, path }
                                }
                                roles::RoleParameterTarget::Extensions(path) => {
                                    Target::Extensions { path }
                                }
                            },
                            extensions: serde_json::from_str(&parameter.extensions)?,
                        },
                    ))
                })
                .collect::<Result<_, Self::Error>>()?,
            compaction: role.compaction.map(|compaction| Compaction {
                threshold: compaction.threshold,
                instructions: compaction.instructions,
            }),
            extensions: serde_json::from_str(&role.extensions)?,
        })
    }
}

impl From<Role> for roles::Role {
    fn from(role: Role) -> Self {
        Self {
            name: role.name,
            parameters: role
                .parameters
                .into_iter()
                .map(|(name, parameter)| roles::RoleParameter {
                    name,
                    target: match parameter.target {
                        Target::Model { path } => roles::RoleParameterTarget::Model(path),
                        Target::Tool { tool, path } => {
                            roles::RoleParameterTarget::Tool((tool, path))
                        }
                        Target::Extensions { path } => roles::RoleParameterTarget::Extensions(path),
                    },
                    extensions: Value::Object(parameter.extensions).to_string(),
                })
                .collect(),
            model: role.model.into(),
            tools: role
                .tools
                .into_iter()
                .map(|(alias, tool)| roles::RoleTool {
                    alias,
                    tool: tool.into(),
                })
                .collect(),
            system_prompt: role.system_prompt,
            compaction: role.compaction.map(|compaction| roles::RoleCompaction {
                threshold: compaction.threshold,
                instructions: compaction.instructions,
            }),
            extensions: Value::Object(role.extensions).to_string(),
        }
    }
}

impl TryFrom<Configured> for Selection {
    type Error = serde_json::Error;

    fn try_from(configured: Configured) -> Result<Self, Self::Error> {
        Ok(Self {
            name: configured.name,
            config: serde_json::from_str(&configured.config)?,
        })
    }
}

impl From<Selection> for Configured {
    fn from(selection: Selection) -> Self {
        Self {
            name: selection.name,
            config: Value::Object(selection.config).to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip_through_the_host_representation() {
        let value = json!({
            "name": "reviewer",
            "model": {
                "name": "openai:gpt",
                "config": {
                    "effort": "high",
                },
            },
            "tools": {
                "bash": {
                    "name": "tools:bash",
                    "config": {},
                },
            },
            "system_prompt": "Review code.",
            "parameters": {
                "timeout": {
                    "target": {
                        "type": "tool",
                        "tool": "bash",
                        "path": ["timeout_ms"],
                    },
                    "extensions": {
                        "fleet.title": "Timeout",
                    },
                },
            },
            "compaction": {},
            "extensions": {
                "fleet.title": "Reviewer",
                "fleet.hidden": false,
            },
        });

        let role: Role = serde_json::from_value(value.clone()).unwrap();
        let role = Role::try_from(roles::Role::from(role)).unwrap();

        assert_eq!(serde_json::to_value(role).unwrap(), value);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let value = json!({
            "name": "reviewer",
            "model": {
                "name": "openai:gpt",
            },
            "title": "Reviewer",
        });

        let error = serde_json::from_value::<Role>(value).unwrap_err();

        assert!(error.to_string().starts_with("unknown field `title`"));
    }

    #[test]
    fn versions_round_trip_as_decimal_strings() {
        let value = u128::MAX.to_string();

        let parsed = parse_version(&value).unwrap();

        assert_eq!(version(parsed), value);
        assert!(parse_version("-1").is_none());
    }
}
