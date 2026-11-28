use crate::{
    WorkspaceId, WorkspacesError, WorkspacesErrorKind, bindings::fleet::host::configuration,
};
use percent_encoding::{AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The home directory on each sprite.
pub const HOME: &str = "/home/sprite";
const REPOSITORY: &AsciiSet = &CONTROLS.add(b'%').add(b'/');

/// The provider config.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// API key connections offered in addition to Sprites.
    #[serde(default)]
    pub connections: BTreeMap<String, ConnectionConfig>,
}

/// An API key connection declared in the provider config.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionConfig {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprite: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Repos>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, Variable>,
}

/// The repositories checked out in a workspace.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged, expecting = "a git URL or a list of git URLs")]
pub enum Repos {
    /// One repository whose worktree is the workspace directory.
    One(String),
    /// Repositories whose worktrees are named subdirectories of the workspace directory.
    Many(Vec<String>),
}

/// The value of an environment variable.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged, expecting = "a string or a connection reference")]
pub enum Variable {
    /// A literal value.
    Value(String),
    /// The credential of a connection declared in the provider config.
    Credential(Credential),
}

/// A reference to the credential of a connection.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub connection: String,
}

impl ProviderConfig {
    pub fn load() -> Result<Self, String> {
        Self::parse(&configuration::get())
    }

    fn parse(config: &str) -> Result<Self, String> {
        let config: Self =
            serde_json::from_str(config).map_err(|error| format!("invalid config: {error}"))?;
        if config.connections.contains_key(crate::sprites::CONNECTION) {
            return Err(format!(
                "invalid config: connection {} is built in",
                crate::sprites::CONNECTION,
            ));
        }
        Ok(config)
    }
}

impl Config {
    pub fn parse(parameters: &str) -> Result<Self, WorkspacesError> {
        let config: Self = serde_json::from_str(parameters).map_err(|error| {
            crate::error(WorkspacesErrorKind::InvalidConfiguration, error.to_string())
        })?;
        if let Some(sprite) = &config.sprite
            && (sprite.is_empty()
                || !sprite
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
        {
            return Err(crate::error(
                WorkspacesErrorKind::InvalidConfiguration,
                "sprite names may only contain lowercase letters, digits and hyphens",
            ));
        }
        match &config.repos {
            None => {}
            Some(Repos::One(url)) => {
                if let "" | "." | ".." = url.as_str() {
                    return Err(crate::error(
                        WorkspacesErrorKind::InvalidConfiguration,
                        "repos must contain git URLs",
                    ));
                }
            }
            Some(Repos::Many(urls)) => {
                if urls.is_empty() {
                    return Err(crate::error(
                        WorkspacesErrorKind::InvalidConfiguration,
                        "repos must not be empty",
                    ));
                }
                let mut names = BTreeSet::new();
                for url in urls {
                    let name = crate::config::name(url);
                    if let "" | "." | ".." = name {
                        return Err(crate::error(
                            WorkspacesErrorKind::InvalidConfiguration,
                            format!("repos must end with a repository name: {url}"),
                        ));
                    }
                    if !names.insert(name) {
                        return Err(crate::error(
                            WorkspacesErrorKind::InvalidConfiguration,
                            format!("repos must have distinct names: {name}"),
                        ));
                    }
                }
            }
        }
        for (name, variable) in &config.env {
            if !crate::exec::valid_name(name) {
                return Err(crate::error(
                    WorkspacesErrorKind::InvalidConfiguration,
                    format!("env names must be shell variable names: {name}"),
                ));
            }
            if let Variable::Value(value) = variable
                && value.contains('\0')
            {
                return Err(crate::error(
                    WorkspacesErrorKind::InvalidConfiguration,
                    format!("env values must not contain null bytes: {name}"),
                ));
            }
        }
        Ok(config)
    }

    /// Checks that env only refers to connections declared in the provider config.
    pub fn check(&self, provider: &ProviderConfig) -> Result<(), WorkspacesError> {
        for (name, variable) in &self.env {
            if let Variable::Credential(Credential { connection }) = variable
                && !provider.connections.contains_key(connection)
            {
                return Err(crate::error(
                    WorkspacesErrorKind::InvalidConfiguration,
                    format!("env {name} refers to undeclared connection {connection}"),
                ));
            }
        }
        Ok(())
    }

    /// Returns the name of the sprite hosting a workspace ID.
    pub fn sprite(&self, id: &WorkspaceId) -> String {
        self.sprite
            .clone()
            .unwrap_or_else(|| format!("fleet-{}", crate::hex(id)))
    }

    /// Returns the workspace directory of a workspace ID.
    pub fn directory(&self, id: &WorkspaceId) -> String {
        if self.repos.is_some() {
            format!("{HOME}/worktrees/{}", crate::hex(id))
        } else {
            HOME.to_owned()
        }
    }

    /// Returns the repository URLs and worktree directories of a workspace ID.
    pub fn worktrees(&self, id: &WorkspaceId) -> Vec<(&str, String)> {
        let directory = self.directory(id);
        match &self.repos {
            None => Vec::new(),
            Some(Repos::One(url)) => vec![(url.as_str(), directory)],
            Some(Repos::Many(urls)) => urls
                .iter()
                .map(|url| {
                    (
                        url.as_str(),
                        format!("{directory}/{}", crate::config::name(url)),
                    )
                })
                .collect(),
        }
    }
}

/// Returns the last path segment of a repository URL without `.git`.
fn name(url: &str) -> &str {
    let path = url.trim_end_matches('/');
    let name = path.rsplit(['/', ':']).next().unwrap_or(path);
    name.strip_suffix(".git").unwrap_or(name)
}

/// Returns the bare clone directory of a repository URL.
pub fn repository(url: &str) -> String {
    format!(
        "{HOME}/repos/{}",
        percent_encoding::utf8_percent_encode(url, REPOSITORY),
    )
}

pub fn path(directory: &str, path: &str) -> Result<String, WorkspacesError> {
    if path.contains('\0') {
        return Err(crate::error(
            WorkspacesErrorKind::InvalidConfiguration,
            "path must not contain null bytes",
        ));
    }
    Ok(if path.starts_with('/') {
        path.to_owned()
    } else if path.is_empty() {
        directory.to_owned()
    } else {
        format!("{directory}/{path}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: WorkspaceId = WorkspaceId {
        high: 0x0123_4567_89ab_cdef,
        low: 1,
    };

    #[test]
    fn provider_config_declares_connections() {
        let config = ProviderConfig::parse("{}").unwrap();
        assert!(config.connections.is_empty());

        let parameters = json!({
            "connections": {
                "github": {
                    "title": "GitHub",
                },
                "npm": {
                    "title": "npm",
                    "description": "Publishes packages.",
                },
            },
        })
        .to_string();
        let config = ProviderConfig::parse(&parameters).unwrap();
        assert_eq!(
            config.connections,
            BTreeMap::from([
                (
                    "github".into(),
                    ConnectionConfig {
                        title: "GitHub".into(),
                        description: None,
                    },
                ),
                (
                    "npm".into(),
                    ConnectionConfig {
                        title: "npm".into(),
                        description: Some("Publishes packages.".into()),
                    },
                ),
            ]),
        );
    }

    #[test]
    fn provider_config_rejects_invalid_connections() {
        let sprites = json!({
            "connections": {
                "sprites": {
                    "title": "Sprites",
                },
            },
        });
        let untitled = json!({
            "connections": {
                "github": {},
            },
        });
        let unknown = json!({
            "connection": {},
        });

        for config in [sprites, untitled, unknown] {
            ProviderConfig::parse(&config.to_string()).unwrap_err();
        }
    }

    #[test]
    fn parse_accepts_optional_fields() {
        let config = Config::parse("{}").unwrap();
        assert_eq!(
            config,
            Config {
                sprite: None,
                setup: None,
                repos: None,
                env: BTreeMap::new(),
            },
        );

        let parameters = json!({
            "sprite": "dev-1",
            "setup": "apt-get install -y ripgrep",
            "repos": "https://github.com/halo-org/fleet.git",
        })
        .to_string();
        let config = Config::parse(&parameters).unwrap();
        assert_eq!(config.sprite.as_deref(), Some("dev-1"));
        assert_eq!(config.setup.as_deref(), Some("apt-get install -y ripgrep"));
        assert_eq!(
            config.repos,
            Some(Repos::One("https://github.com/halo-org/fleet.git".into())),
        );

        let parameters = json!({
            "repos": ["https://github.com/halo-org/fleet.git", "git@github.com:a/b"],
        })
        .to_string();
        let config = Config::parse(&parameters).unwrap();
        assert_eq!(
            config.repos,
            Some(Repos::Many(vec![
                "https://github.com/halo-org/fleet.git".into(),
                "git@github.com:a/b".into(),
            ])),
        );

        let parameters = json!({
            "env": {
                "GH_TOKEN": {"connection": "github"},
                "_NAME": "'$(a\nb)\"",
            },
        })
        .to_string();
        let config = Config::parse(&parameters).unwrap();
        assert_eq!(
            config.env,
            BTreeMap::from([
                (
                    "GH_TOKEN".into(),
                    Variable::Credential(Credential {
                        connection: "github".into(),
                    }),
                ),
                ("_NAME".into(), Variable::Value("'$(a\nb)\"".into())),
            ]),
        );
    }

    #[test]
    fn config_stores_connection_references() {
        let parameters = json!({
            "env": {
                "GH_TOKEN": {"connection": "github"},
                "NAME": "value",
            },
        });
        let config = Config::parse(&parameters.to_string()).unwrap();

        let stored = serde_json::to_string_pretty(&config).unwrap();

        assert_eq!(
            stored,
            r#"{
  "env": {
    "GH_TOKEN": {
      "connection": "github"
    },
    "NAME": "value"
  }
}"#,
        );
        assert_eq!(Config::parse(&stored).unwrap(), config);
    }

    #[test]
    fn check_rejects_undeclared_connections() {
        let provider =
            ProviderConfig::parse(r#"{"connections": {"github": {"title": "GitHub"}}}"#).unwrap();
        let declared = Config::parse(r#"{"env": {"GH_TOKEN": {"connection": "github"}}}"#).unwrap();
        let undeclared = Config::parse(r#"{"env": {"TOKEN": {"connection": "gitlab"}}}"#).unwrap();

        declared.check(&provider).unwrap();
        let error = undeclared.check(&provider).unwrap_err();

        assert!(matches!(
            error.kind,
            WorkspacesErrorKind::InvalidConfiguration
        ));
        assert_eq!(
            error.message,
            "env TOKEN refers to undeclared connection gitlab"
        );
    }

    #[test]
    fn parse_rejects_invalid_fields() {
        let unknown = json!({
            "cwd": "/project",
        });
        let sprite = json!({
            "sprite": "Dev/1",
        });
        let repo = json!({
            "repo": "https://github.com/halo-org/fleet.git",
        });
        let dots = json!({
            "repos": "..",
        });
        let empty = json!({
            "repos": [],
        });
        let unnamed = json!({
            "repos": ["https://github.com/a/.git"],
        });
        let duplicate = json!({
            "repos": ["https://github.com/a/fleet.git", "https://github.com/b/fleet"],
        });
        let name = json!({
            "env": {"GIT-TOKEN": "a"},
        });
        let digit = json!({
            "env": {"1TOKEN": "a"},
        });
        let nul = json!({
            "env": {"TOKEN": "a\0"},
        });
        let number = json!({
            "env": {"TOKEN": 1},
        });
        let reference = json!({
            "env": {"TOKEN": {"connection": "github", "field": "token"}},
        });

        for parameters in [
            unknown, sprite, repo, dots, empty, unnamed, duplicate, name, digit, nul, number,
            reference,
        ] {
            let error = Config::parse(&parameters.to_string()).unwrap_err();

            assert!(matches!(
                error.kind,
                WorkspacesErrorKind::InvalidConfiguration
            ));
        }
    }

    #[test]
    fn sprite_defaults_to_one_per_workspace_id() {
        let config = Config::parse("{}").unwrap();
        assert_eq!(config.sprite(&ID), "fleet-0123456789abcdef0000000000000001");

        let config = Config::parse(r#"{"sprite": "dev"}"#).unwrap();
        assert_eq!(config.sprite(&ID), "dev");
    }

    #[test]
    fn directory_is_the_worktree_of_one_repo() {
        let config = Config::parse("{}").unwrap();
        assert_eq!(config.directory(&ID), "/home/sprite");
        assert_eq!(config.worktrees(&ID), []);

        let config = Config::parse(r#"{"repos": "https://github.com/halo-org/fleet"}"#).unwrap();
        assert_eq!(
            config.directory(&ID),
            "/home/sprite/worktrees/0123456789abcdef0000000000000001",
        );
        assert_eq!(
            config.worktrees(&ID),
            [(
                "https://github.com/halo-org/fleet",
                "/home/sprite/worktrees/0123456789abcdef0000000000000001".to_owned(),
            )],
        );
    }

    #[test]
    fn directory_contains_a_worktree_of_each_listed_repo() {
        let parameters = json!({
            "repos": ["https://github.com/halo-org/fleet.git", "git@github.com:halo-org/fleet-web/"],
        })
        .to_string();
        let config = Config::parse(&parameters).unwrap();

        assert_eq!(
            config.directory(&ID),
            "/home/sprite/worktrees/0123456789abcdef0000000000000001",
        );
        assert_eq!(
            config.worktrees(&ID),
            [
                (
                    "https://github.com/halo-org/fleet.git",
                    "/home/sprite/worktrees/0123456789abcdef0000000000000001/fleet".to_owned(),
                ),
                (
                    "git@github.com:halo-org/fleet-web/",
                    "/home/sprite/worktrees/0123456789abcdef0000000000000001/fleet-web".to_owned(),
                ),
            ],
        );
    }

    #[test]
    fn repository_encodes_the_url_in_one_directory() {
        assert_eq!(
            repository("https://github.com/halo-org/fleet.git"),
            "/home/sprite/repos/https:%2F%2Fgithub.com%2Fhalo-org%2Ffleet.git",
        );
        assert_eq!(
            repository("git@github.com:a/b%2F"),
            "/home/sprite/repos/git@github.com:a%2Fb%252F",
        );
    }

    #[test]
    fn path_joins_relative_paths_to_the_directory() {
        assert_eq!(path("/home/sprite", "").unwrap(), "/home/sprite");
        assert_eq!(
            path("/home/sprite", "src/main.rs").unwrap(),
            "/home/sprite/src/main.rs",
        );
        assert_eq!(
            path("/home/sprite", "../file").unwrap(),
            "/home/sprite/../file"
        );
        assert_eq!(path("/home/sprite", "/etc/hosts").unwrap(), "/etc/hosts");
    }

    #[test]
    fn path_rejects_null_bytes() {
        let error = path("/home/sprite", "file\0").unwrap_err();

        assert!(matches!(
            error.kind,
            WorkspacesErrorKind::InvalidConfiguration
        ));
    }
}
