use core::iter;
use fleet_core::{
    Configured, JsonObject, JsonValue, Role, RoleCompaction, RoleName, RoleParameter,
    RoleParameterTarget,
};
use fleet_kit::{Extension as _, RoleWorkspace};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

/// The extension key of a display title.
pub const TITLE: &str = "fleet.title";
/// The extension key of a description.
pub const DESCRIPTION: &str = "fleet.description";
/// The extension key that hides a role from selection.
pub const HIDDEN: &str = "fleet.hidden";
/// The extension key of a parameter's JSON Schema.
pub const SCHEMA: &str = "fleet.schema";

/// A role in the shape edited through the API, without its name.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDefinition {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    hidden: bool,
    model: Selection,
    #[serde(default)]
    tools: BTreeMap<String, Selection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<RoleWorkspace>,
    #[serde(default)]
    parameters: BTreeMap<String, Parameter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction: Option<Compaction>,
    #[serde(default)]
    system_prompt: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    name: String,
    #[serde(default)]
    config: JsonObject,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameter {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<JsonValue>,
    target: Target,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Model { path: Vec<String> },
    Tool { tool: String, path: Vec<String> },
    Workspace { path: Vec<String> },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Compaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    threshold: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
}

impl RoleDefinition {
    /// Parses Markdown with YAML frontmatter. The body is the system prompt.
    pub fn from_markdown(source: &str) -> Result<Self, yaml_serde::Error> {
        let mut lines = source.split_inclusive('\n');
        if lines.next().map(str::trim_end) != Some("---") {
            return Err(serde::de::Error::custom(
                "expected YAML frontmatter between --- lines",
            ));
        }

        let mut frontmatter = String::new();
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                let mut definition: Self = yaml_serde::from_str(&frontmatter)?;
                if !definition.system_prompt.is_empty() {
                    return Err(serde::de::Error::custom(
                        "system_prompt belongs after the frontmatter",
                    ));
                }
                definition.system_prompt = lines.collect();
                return Ok(definition);
            }
            frontmatter.push_str(line);
        }

        Err(serde::de::Error::custom("missing closing --- line"))
    }

    /// Converts the definition to a role with the given name.
    pub fn into_role(self, name: RoleName) -> Result<Role, serde_json::Error> {
        let mut extensions = JsonObject::new();
        insert(&mut extensions, TITLE, self.title);
        insert(&mut extensions, DESCRIPTION, self.description);
        insert(&mut extensions, HIDDEN, self.hidden.then_some(true));
        if let Some(workspace) = self.workspace {
            workspace.insert(&mut extensions)?;
        }

        Ok(Role {
            name,
            parameters: self
                .parameters
                .into_iter()
                .map(|(name, parameter)| (name.into(), parameter.into()))
                .collect(),
            model: self.model.into(),
            tools: self
                .tools
                .into_iter()
                .map(|(alias, tool)| (alias.into(), tool.into()))
                .collect(),
            system_prompt: self.system_prompt,
            compaction: self.compaction.map(|compaction| RoleCompaction {
                threshold: compaction.threshold,
                instructions: compaction.instructions,
            }),
            extensions,
        })
    }
}

impl TryFrom<Role> for RoleDefinition {
    type Error = serde_json::Error;

    fn try_from(role: Role) -> Result<Self, Self::Error> {
        let mut extensions = role.extensions;
        let definition = Self {
            title: take(&mut extensions, TITLE)?,
            description: take(&mut extensions, DESCRIPTION)?,
            hidden: take(&mut extensions, HIDDEN)?.unwrap_or_default(),
            model: role.model.into(),
            tools: role
                .tools
                .into_iter()
                .map(|(alias, tool)| (alias.into(), tool.into()))
                .collect(),
            workspace: take(&mut extensions, RoleWorkspace::KEY)?,
            parameters: role
                .parameters
                .into_iter()
                .map(|(name, parameter)| Ok((name.into(), parameter.try_into()?)))
                .collect::<Result<_, Self::Error>>()?,
            compaction: role.compaction.map(|compaction| Compaction {
                threshold: compaction.threshold,
                instructions: compaction.instructions,
            }),
            system_prompt: role.system_prompt,
        };
        unsupported(&extensions)?;
        Ok(definition)
    }
}

impl<N: From<String>> From<Selection> for Configured<N> {
    fn from(selection: Selection) -> Self {
        Self {
            name: selection.name.into(),
            config: selection.config,
        }
    }
}

impl<N: Into<String>> From<Configured<N>> for Selection {
    fn from(configured: Configured<N>) -> Self {
        Self {
            name: configured.name.into(),
            config: configured.config,
        }
    }
}

impl From<Parameter> for RoleParameter {
    fn from(parameter: Parameter) -> Self {
        let mut extensions = JsonObject::new();
        insert(&mut extensions, TITLE, parameter.title);
        insert(&mut extensions, DESCRIPTION, parameter.description);
        insert(&mut extensions, SCHEMA, parameter.schema);
        Self {
            target: match parameter.target {
                Target::Model { path } => RoleParameterTarget::Model(path),
                Target::Tool { tool, path } => RoleParameterTarget::Tool(tool.into(), path),
                Target::Workspace { path } => RoleParameterTarget::Extensions(
                    iter::once(RoleWorkspace::KEY.into()).chain(path).collect(),
                ),
            },
            extensions,
        }
    }
}

impl TryFrom<RoleParameter> for Parameter {
    type Error = serde_json::Error;

    fn try_from(parameter: RoleParameter) -> Result<Self, Self::Error> {
        let mut extensions = parameter.extensions;
        let target = match parameter.target {
            RoleParameterTarget::Model(path) => Target::Model { path },
            RoleParameterTarget::Tool(tool, path) => Target::Tool {
                tool: tool.into(),
                path,
            },
            RoleParameterTarget::Extensions(path) => match path.split_first() {
                Some((key, path)) if key == RoleWorkspace::KEY => Target::Workspace {
                    path: path.to_vec(),
                },
                _ => {
                    return Err(serde::de::Error::custom(
                        "parameter targets an unsupported extension",
                    ));
                }
            },
        };
        let parameter = Self {
            title: take(&mut extensions, TITLE)?,
            description: take(&mut extensions, DESCRIPTION)?,
            schema: take(&mut extensions, SCHEMA)?,
            target,
        };
        unsupported(&extensions)?;
        Ok(parameter)
    }
}

/// Returns whether a role name is valid.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn insert(extensions: &mut JsonObject, key: &str, value: Option<impl Into<JsonValue>>) {
    if let Some(value) = value {
        extensions.insert(key.into(), value.into());
    }
}

fn take<T: DeserializeOwned>(
    extensions: &mut JsonObject,
    key: &str,
) -> Result<Option<T>, serde_json::Error> {
    extensions
        .remove(key)
        .map_or(Ok(None), Option::<T>::deserialize)
}

fn unsupported(extensions: &JsonObject) -> Result<(), serde_json::Error> {
    extensions.keys().next().map_or(Ok(()), |key| {
        Err(serde::de::Error::custom(format_args!(
            "unsupported extension {key}"
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_types::ApiRole;
    use serde_json::{Value, json};

    fn role(name: &str, definition: Value) -> Role {
        serde_json::from_value::<RoleDefinition>(definition)
            .unwrap()
            .into_role(name.into())
            .unwrap()
    }

    #[test]
    fn role_metadata_exposes_parameters_and_configured_defaults() {
        let role = role(
            "reviewer",
            json!({
                "title": "Reviewer",
                "description": "Review changes.",
                "hidden": true,
                "model": {
                    "name": "provider:model",
                    "config": {
                        "effort": "high",
                        "token": "private",
                    },
                },
                "tools": {
                    "search": {
                        "name": "provider:search",
                        "config": {
                            "enabled": false,
                        },
                    },
                },
                "workspace": {
                    "name": "host:default",
                    "config": {
                        "cwd": "/project",
                    },
                },
                "parameters": {
                    "effort": {
                        "title": "Reasoning",
                        "schema": {
                            "type": "string",
                            "enum": ["low", "high"],
                        },
                        "target": {"type": "model", "path": ["effort"]},
                    },
                    "search": {
                        "target": {"type": "tool", "tool": "search", "path": ["enabled"]},
                    },
                    "cwd": {
                        "target": {"type": "workspace", "path": ["config", "cwd"]},
                    },
                    "missing": {
                        "target": {"type": "model", "path": ["missing"]},
                    },
                    "token": {
                        "schema": {
                            "type": "string",
                            "writeOnly": true,
                        },
                        "target": {"type": "model", "path": ["token"]},
                    },
                },
                "system_prompt": "Private instructions.",
            }),
        );
        let role = ApiRole::try_from(role).unwrap();
        let value = serde_json::to_value(role).unwrap();

        assert_eq!(value["title"], "Reviewer");
        assert_eq!(value["description"], "Review changes.");
        assert_eq!(value["hidden"], true);
        assert_eq!(
            value["parameters"]["effort"],
            json!({
                "title": "Reasoning",
                "schema": {
                    "type": "string",
                    "enum": ["low", "high"],
                },
                "default": "high",
            }),
        );
        assert_eq!(value["parameters"]["search"]["default"], false);
        assert_eq!(value["parameters"]["cwd"]["default"], "/project");
        assert!(value["parameters"]["missing"].get("default").is_none());
        assert!(value["parameters"]["token"].get("default").is_none());
        assert!(value.get("system_prompt").is_none());
        assert!(value.get("model").is_none());
    }

    #[test]
    fn definitions_round_trip_through_roles() {
        let definition = json!({
            "title": "Reviewer",
            "hidden": false,
            "model": {
                "name": "provider:model",
                "config": {
                    "effort": "high",
                },
            },
            "tools": {
                "search": {
                    "name": "provider:search",
                    "config": {},
                },
            },
            "workspace": {
                "name": "host:default",
                "config": {
                    "cwd": "/project",
                },
                "id": "7",
            },
            "parameters": {
                "cwd": {
                    "title": "Directory",
                    "target": {
                        "type": "workspace",
                        "path": ["config", "cwd"],
                    },
                },
                "enabled": {
                    "target": {
                        "type": "tool",
                        "tool": "search",
                        "path": ["enabled"],
                    },
                },
            },
            "compaction": {
                "threshold": 1000,
            },
            "system_prompt": "Review changes.\n",
        });

        let role = role("reviewer", definition.clone());
        assert_eq!(
            role.parameters[&"cwd".into()].target,
            RoleParameterTarget::Extensions(vec![
                "fleet.workspace".into(),
                "config".into(),
                "cwd".into(),
            ]),
        );
        let role = RoleDefinition::try_from(role).unwrap();
        assert_eq!(serde_json::to_value(role).unwrap(), definition);
    }

    #[test]
    fn definitions_reject_unknown_fields() {
        for definition in [
            json!({}),
            json!({"model": {"name": "provider:model"}, "name": "assistant"}),
            json!({"model": {"name": "provider:model"}, "extensions": {}}),
        ] {
            serde_json::from_value::<RoleDefinition>(definition).unwrap_err();
        }
    }

    #[test]
    fn markdown_bodies_are_system_prompts() {
        let source = "\
---
title: Reviewer
model:
  name: provider:model
  config:
    effort: high
workspace:
  name: sprites:default
  id: '7'
parameters:
  repos:
    target: {type: workspace, path: [config, repos]}
---

Be helpful.

---
Keep this separator.
";

        let definition = RoleDefinition::from_markdown(source).unwrap();
        assert_eq!(
            serde_json::to_value(definition).unwrap(),
            json!({
                "title": "Reviewer",
                "hidden": false,
                "model": {
                    "name": "provider:model",
                    "config": {
                        "effort": "high",
                    },
                },
                "tools": {},
                "workspace": {
                    "name": "sprites:default",
                    "config": {},
                    "id": "7",
                },
                "parameters": {
                    "repos": {
                        "target": {
                            "type": "workspace",
                            "path": ["config", "repos"],
                        },
                    },
                },
                "system_prompt": "\nBe helpful.\n\n---\nKeep this separator.\n",
            }),
        );
    }

    #[test]
    fn markdown_rejects_missing_frontmatter_and_unknown_fields() {
        for source in [
            "Be helpful.",
            "---\nmodel:\n  name: provider:model\n",
            "---\nmodel:\n  name: provider:model\ntoolz: {}\n---\n",
            "---\nmodel:\n  name: provider:model\nsystem_prompt: Be helpful.\n---\n",
        ] {
            RoleDefinition::from_markdown(source).unwrap_err();
        }
    }

    #[test]
    fn unrepresentable_roles_are_errors() {
        let mut extended = role("assistant", json!({"model": {"name": "provider:model"}}));
        extended.extensions.insert("other".into(), true.into());
        let mut targeted = role("assistant", json!({"model": {"name": "provider:model"}}));
        targeted.parameters.insert(
            "other".into(),
            RoleParameter {
                target: RoleParameterTarget::Extensions(vec!["other".into()]),
                extensions: JsonObject::new(),
            },
        );

        for role in [extended, targeted] {
            RoleDefinition::try_from(role).unwrap_err();
        }
    }

    #[test]
    fn role_names_are_path_segments() {
        for name in [
            "",
            ".",
            "..",
            "../assistant",
            "nested/assistant",
            "nested\\assistant",
        ] {
            assert!(!valid_name(name));
        }
        assert!(valid_name("code-review_2"));
    }
}
