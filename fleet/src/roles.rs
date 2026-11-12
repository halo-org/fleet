use core::iter;
use fleet_core::{
    Configured, JsonObject, JsonValue, Role, RoleCompaction, RoleName, RoleParameter,
    RoleParameterTarget,
};
use fleet_kit::{Extension as _, RoleWorkspace};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io, path::PathBuf};

/// The extension key of a display title.
pub const TITLE: &str = "fleet.title";
/// The extension key of a description.
pub const DESCRIPTION: &str = "fleet.description";
/// The extension key that hides a role from selection.
pub const HIDDEN: &str = "fleet.hidden";
/// The extension key of a parameter's JSON Schema.
pub const SCHEMA: &str = "fleet.schema";

/// Loads roles from Markdown files.
#[derive(Clone)]
pub struct Roles {
    directory: PathBuf,
}

/// A role in the shape of its file.
#[derive(Serialize)]
pub struct RoleDefinition {
    name: String,
    #[serde(flatten)]
    frontmatter: Frontmatter,
    system_prompt: String,
}

/// A role that could not be read.
#[derive(Debug)]
pub struct InvalidRole {
    /// The role name.
    pub name: RoleName,
    /// Why the role is invalid.
    pub error: io::Error,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
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
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    name: String,
    #[serde(default)]
    config: JsonObject,
}

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Model { path: Vec<String> },
    Tool { tool: String, path: Vec<String> },
    Workspace { path: Vec<String> },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Compaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    threshold: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
}

impl Roles {
    /// Creates a role loader for a directory.
    pub const fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Lists the available roles, including invalid ones.
    pub async fn list(&self) -> io::Result<Vec<Result<Role, InvalidRole>>> {
        let mut directory = match tokio::fs::read_dir(&self.directory).await {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut names = Vec::new();

        while let Some(entry) = directory.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "md")
                && entry.file_type().await?.is_file()
                && let Some(name) = path.file_stem().and_then(|name| name.to_str())
                && valid_name(name)
            {
                names.push(RoleName::from(name.to_owned()));
            }
        }

        names.sort_unstable();
        let mut roles = Vec::with_capacity(names.len());
        for name in names {
            match self
                .definition(&name)
                .await
                .and_then(|definition| definition.map(Role::try_from).transpose())
            {
                Ok(Some(role)) => roles.push(Ok(role)),
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    roles.push(Err(InvalidRole { name, error }));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(roles)
    }

    /// Returns the definition of a role.
    ///
    /// Invalid roles return [`InvalidData`](io::ErrorKind::InvalidData) errors.
    pub async fn definition(&self, name: &RoleName) -> io::Result<Option<RoleDefinition>> {
        if !valid_name(name.as_ref()) {
            return Ok(None);
        }

        match tokio::fs::read_to_string(self.path(name)).await {
            Ok(source) => RoleDefinition::parse(name.to_string(), &source).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn path(&self, name: &RoleName) -> PathBuf {
        self.directory.join(format!("{name}.md"))
    }
}

impl fleet_core::Roles for Roles {
    type Error = io::Error;

    async fn get(&self, name: &RoleName) -> Result<Option<Role>, Self::Error> {
        self.definition(name)
            .await
            .and_then(|definition| definition.map(Role::try_from).transpose())
            .map_err(|error| {
                if error.kind() == io::ErrorKind::InvalidData {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid role {}: {error}", self.path(name).display()),
                    )
                } else {
                    error
                }
            })
    }
}

impl RoleDefinition {
    fn parse(name: String, source: &str) -> io::Result<Self> {
        let mut lines = source.split_inclusive('\n');
        if lines.next().map(str::trim_end) != Some("---") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected YAML frontmatter between --- lines",
            ));
        }

        let mut frontmatter = String::new();
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                return Ok(Self {
                    name,
                    frontmatter: yaml_serde::from_str(&frontmatter)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                    system_prompt: lines.collect(),
                });
            }
            frontmatter.push_str(line);
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing closing --- line",
        ))
    }
}

impl TryFrom<RoleDefinition> for Role {
    type Error = io::Error;

    fn try_from(definition: RoleDefinition) -> Result<Self, Self::Error> {
        let RoleDefinition {
            name,
            frontmatter,
            system_prompt,
        } = definition;
        let mut extensions = JsonObject::new();
        insert(&mut extensions, TITLE, frontmatter.title);
        insert(&mut extensions, DESCRIPTION, frontmatter.description);
        insert(&mut extensions, HIDDEN, frontmatter.hidden.then_some(true));
        if let Some(workspace) = frontmatter.workspace {
            workspace.insert(&mut extensions)?;
        }

        Ok(Self {
            name: name.into(),
            parameters: frontmatter
                .parameters
                .into_iter()
                .map(|(name, parameter)| (name.into(), parameter.into()))
                .collect(),
            model: frontmatter.model.into(),
            tools: frontmatter
                .tools
                .into_iter()
                .map(|(alias, tool)| (alias.into(), tool.into()))
                .collect(),
            system_prompt,
            compaction: frontmatter.compaction.map(|compaction| RoleCompaction {
                threshold: compaction.threshold,
                instructions: compaction.instructions,
            }),
            extensions,
        })
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

fn insert(extensions: &mut JsonObject, key: &str, value: Option<impl Into<JsonValue>>) {
    if let Some(value) = value {
        extensions.insert(key.into(), value.into());
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_types::ApiRole;
    use serde_json::json;

    fn parse(name: &str, source: &str) -> io::Result<Role> {
        RoleDefinition::parse(name.into(), source)?.try_into()
    }

    #[test]
    fn role_metadata_exposes_parameters_and_configured_defaults() {
        let source = "\
---
title: Reviewer
description: Review changes.
hidden: true
model:
  name: provider:model
  config:
    effort: high
    token: private
tools:
  search:
    name: provider:search
    config:
      enabled: false
workspace:
  name: host:default
  config:
    cwd: /project
parameters:
  effort:
    title: Reasoning
    schema:
      type: string
      enum: [low, high]
    target: {type: model, path: [effort]}
  search:
    target: {type: tool, tool: search, path: [enabled]}
  cwd:
    target: {type: workspace, path: [config, cwd]}
  missing:
    target: {type: model, path: [missing]}
  token:
    schema:
      type: string
      writeOnly: true
    target: {type: model, path: [token]}
---
Private instructions.
";

        let role = parse("reviewer", source).unwrap();
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
    fn parse_preserves_prompt_and_configuration() {
        let source = "\
---
model:
  name: provider:model
  config:
    effort: high
tools:
  search:
    name: provider:search
workspace:
  name: sprites:default
  id: '7'
parameters:
  effort:
    target:
      type: model
      path: [effort]
  repos:
    target:
      type: workspace
      path: [config, repos]
---

Be helpful.

---
Keep this separator.
";

        let role = parse("assistant", source).unwrap();
        assert_eq!(role.name, "assistant".into());
        assert_eq!(role.model.name, "provider:model".into());
        assert_eq!(role.model.config["effort"], "high");
        assert_eq!(role.tools[&"search".into()].name, "provider:search".into());
        assert_eq!(
            role.parameters[&"effort".into()].target,
            RoleParameterTarget::Model(vec!["effort".into()]),
        );
        assert_eq!(
            role.parameters[&"repos".into()].target,
            RoleParameterTarget::Extensions(vec![
                "fleet.workspace".into(),
                "config".into(),
                "repos".into(),
            ]),
        );
        assert_eq!(
            serde_json::Value::from(role.extensions),
            json!({
                "fleet.workspace": {
                    "name": "sprites:default",
                    "config": {},
                    "id": "7",
                },
            }),
        );
        assert_eq!(
            role.system_prompt,
            "\nBe helpful.\n\n---\nKeep this separator.\n",
        );
    }

    #[test]
    fn definition_keeps_the_shape_of_the_file() {
        let source = "\
---
title: Reviewer
model:
  name: provider:model
  config:
    effort: high
tools:
  search:
    name: provider:search
workspace:
  name: host:default
  config:
    cwd: /project
  id: '7'
parameters:
  cwd:
    title: Directory
    target: {type: workspace, path: [config, cwd]}
  enabled:
    target: {type: tool, tool: search, path: [enabled]}
compaction:
  threshold: 1000
---
Review changes.
";

        let definition = RoleDefinition::parse("reviewer".into(), source).unwrap();

        assert_eq!(
            serde_json::to_value(definition).unwrap(),
            json!({
                "name": "reviewer",
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
            }),
        );
    }

    #[test]
    fn parse_rejects_missing_frontmatter_and_unknown_fields() {
        for source in [
            "Be helpful.",
            "---\nmodel:\n  name: provider:model\n",
            "---\nmodel:\n  name: provider:model\ntoolz: {}\n---\n",
            "---\nmodel:\n  name: provider:model\nextensions: {}\n---\n",
        ] {
            let error = parse("assistant", source).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn role_names_cannot_escape_the_directory() {
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
