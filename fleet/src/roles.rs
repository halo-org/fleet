use core::iter;
use fleet_core::{
    Configured, JsonObject, JsonValue, Role, RoleCompaction, RoleName, RoleParameter,
    RoleParameterTarget,
};
use fleet_kit::{Extension as _, RoleWorkspace};
use serde::Deserialize;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    title: Option<String>,
    description: Option<String>,
    #[serde(default)]
    hidden: bool,
    model: Selection,
    #[serde(default)]
    tools: BTreeMap<String, Selection>,
    workspace: Option<RoleWorkspace>,
    #[serde(default)]
    parameters: BTreeMap<String, Parameter>,
    compaction: Option<Compaction>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    name: String,
    #[serde(default)]
    config: JsonObject,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameter {
    title: Option<String>,
    description: Option<String>,
    schema: Option<JsonValue>,
    target: Target,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Model { path: Vec<String> },
    Tool { tool: String, path: Vec<String> },
    Workspace { path: Vec<String> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Compaction {
    threshold: Option<u32>,
    instructions: Option<String>,
}

impl Roles {
    /// Creates a role loader for a directory.
    pub const fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Lists the available roles.
    pub async fn list(&self) -> io::Result<Vec<Role>> {
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
                names.push(name.to_owned());
            }
        }

        names.sort_unstable();
        let mut roles = Vec::with_capacity(names.len());
        for name in names {
            if let Some(role) = fleet_core::Roles::get(self, &name.into()).await? {
                roles.push(role);
            }
        }
        Ok(roles)
    }
}

impl fleet_core::Roles for Roles {
    type Error = io::Error;

    async fn get(&self, name: &RoleName) -> Result<Option<Role>, Self::Error> {
        if !valid_name(name.as_ref()) {
            return Ok(None);
        }

        let path = self.directory.join(format!("{name}.md"));
        let source = match tokio::fs::read_to_string(&path).await {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };

        parse(name.clone(), &source).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid role {}: {error}", path.display()),
            )
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

fn parse(name: RoleName, source: &str) -> io::Result<Role> {
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
            let frontmatter: Frontmatter = yaml_serde::from_str(&frontmatter)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let mut extensions = JsonObject::new();
            insert(&mut extensions, TITLE, frontmatter.title);
            insert(&mut extensions, DESCRIPTION, frontmatter.description);
            insert(&mut extensions, HIDDEN, frontmatter.hidden.then_some(true));
            if let Some(workspace) = frontmatter.workspace {
                workspace.insert(&mut extensions)?;
            }

            return Ok(Role {
                name,
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
                system_prompt: lines.collect(),
                compaction: frontmatter.compaction.map(|compaction| RoleCompaction {
                    threshold: compaction.threshold,
                    instructions: compaction.instructions,
                }),
                extensions,
            });
        }
        frontmatter.push_str(line);
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "missing closing --- line",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiRole;
    use serde_json::json;

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

        let role = parse("reviewer".into(), source).unwrap();
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

        let role = parse("assistant".into(), source).unwrap();
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
    fn parse_rejects_missing_frontmatter_and_unknown_fields() {
        for source in [
            "Be helpful.",
            "---\nmodel:\n  name: provider:model\n",
            "---\nmodel:\n  name: provider:model\ntoolz: {}\n---\n",
            "---\nmodel:\n  name: provider:model\nextensions: {}\n---\n",
        ] {
            let error = parse("assistant".into(), source).unwrap_err();
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
