use fleet_core::{
    Configured, JsonObject, Role, RoleCompaction, RoleName, RoleParameter, RoleParameterTarget,
};
use serde::Deserialize;
use std::{collections::BTreeMap, io, path::PathBuf};

/// Loads roles from Markdown files.
#[derive(Clone)]
pub struct Roles {
    directory: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    model: Selection,
    #[serde(default)]
    tools: BTreeMap<String, Selection>,
    #[serde(default)]
    parameters: BTreeMap<String, Parameter>,
    compaction: Option<Compaction>,
    #[serde(default)]
    extensions: JsonObject,
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
    target: Target,
    #[serde(default)]
    extensions: JsonObject,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Model { path: Vec<String> },
    Tool { tool: String, path: Vec<String> },
    Extensions { path: Vec<String> },
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

    /// Lists the available role names.
    pub async fn list(&self) -> io::Result<Vec<String>> {
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
        Ok(names)
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
        Self {
            target: match parameter.target {
                Target::Model { path } => RoleParameterTarget::Model(path),
                Target::Tool { tool, path } => RoleParameterTarget::Tool(tool.into(), path),
                Target::Extensions { path } => RoleParameterTarget::Extensions(path),
            },
            extensions: parameter.extensions,
        }
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
                extensions: frontmatter.extensions,
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
parameters:
  effort:
    target:
      type: model
      path: [effort]
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
