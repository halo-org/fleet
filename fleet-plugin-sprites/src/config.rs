use crate::{WorkspacesError, WorkspacesErrorKind};
use serde::{Deserialize, Serialize};

/// The workspace directory on each sprite.
pub const HOME: &str = "/home/sprite";

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
}

impl Config {
    pub fn parse(parameters: &str) -> Result<Self, WorkspacesError> {
        serde_json::from_str(parameters).map_err(|error| {
            crate::error(WorkspacesErrorKind::InvalidConfiguration, error.to_string())
        })
    }
}

pub fn path(path: &str) -> Result<String, WorkspacesError> {
    if path.contains('\0') {
        return Err(crate::error(
            WorkspacesErrorKind::InvalidConfiguration,
            "path must not contain null bytes",
        ));
    }
    Ok(if path.starts_with('/') {
        path.to_owned()
    } else if path.is_empty() {
        HOME.to_owned()
    } else {
        format!("{HOME}/{path}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_accepts_optional_setup() {
        let config = Config::parse("{}").unwrap();
        assert_eq!(config, Config { setup: None });

        let parameters = json!({
            "setup": "apt-get install -y ripgrep",
        })
        .to_string();
        let config = Config::parse(&parameters).unwrap();
        assert_eq!(config.setup.as_deref(), Some("apt-get install -y ripgrep"));
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let parameters = json!({
            "cwd": "/project",
        })
        .to_string();

        let error = Config::parse(&parameters).unwrap_err();

        assert!(matches!(
            error.kind,
            WorkspacesErrorKind::InvalidConfiguration
        ));
    }

    #[test]
    fn path_joins_relative_paths_to_home() {
        assert_eq!(path("").unwrap(), "/home/sprite");
        assert_eq!(path("src/main.rs").unwrap(), "/home/sprite/src/main.rs");
        assert_eq!(path("../file").unwrap(), "/home/sprite/../file");
        assert_eq!(path("/etc/hosts").unwrap(), "/etc/hosts");
    }

    #[test]
    fn path_rejects_null_bytes() {
        let error = path("file\0").unwrap_err();

        assert!(matches!(
            error.kind,
            WorkspacesErrorKind::InvalidConfiguration
        ));
    }
}
