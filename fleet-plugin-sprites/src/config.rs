use crate::{WorkspaceId, WorkspacesError, WorkspacesErrorKind};
use percent_encoding::{AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};

/// The home directory on each sprite.
pub const HOME: &str = "/home/sprite";
const REPOSITORY: &AsciiSet = &CONTROLS.add(b'%').add(b'/');

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprite: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
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
        if let Some("" | "." | "..") = config.repo.as_deref() {
            return Err(crate::error(
                WorkspacesErrorKind::InvalidConfiguration,
                "repo must be a git URL",
            ));
        }
        Ok(config)
    }

    /// Returns the name of the sprite hosting a workspace ID.
    pub fn sprite(&self, id: &WorkspaceId) -> String {
        self.sprite
            .clone()
            .unwrap_or_else(|| format!("fleet-{}", crate::hex(id)))
    }

    /// Returns the workspace directory of a workspace ID.
    pub fn directory(&self, id: &WorkspaceId) -> String {
        if self.repo.is_some() {
            format!("{HOME}/worktrees/{}", crate::hex(id))
        } else {
            HOME.to_owned()
        }
    }
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
    fn parse_accepts_optional_fields() {
        let config = Config::parse("{}").unwrap();
        assert_eq!(
            config,
            Config {
                sprite: None,
                setup: None,
                repo: None,
            },
        );

        let parameters = json!({
            "sprite": "dev-1",
            "setup": "apt-get install -y ripgrep",
            "repo": "https://github.com/halo-org/fleet.git",
        })
        .to_string();
        let config = Config::parse(&parameters).unwrap();
        assert_eq!(config.sprite.as_deref(), Some("dev-1"));
        assert_eq!(config.setup.as_deref(), Some("apt-get install -y ripgrep"));
        assert_eq!(
            config.repo.as_deref(),
            Some("https://github.com/halo-org/fleet.git"),
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
            "repo": "..",
        });

        for parameters in [unknown, sprite, repo] {
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
    fn directory_is_a_worktree_with_a_repo() {
        let config = Config::parse("{}").unwrap();
        assert_eq!(config.directory(&ID), "/home/sprite");

        let config = Config::parse(r#"{"repo": "https://github.com/halo-org/fleet"}"#).unwrap();
        assert_eq!(
            config.directory(&ID),
            "/home/sprite/worktrees/0123456789abcdef0000000000000001",
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
