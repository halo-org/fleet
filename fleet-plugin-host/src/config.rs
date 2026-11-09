use crate::{WorkspacesError, WorkspacesErrorKind};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub cwd: String,
}

impl Config {
    pub fn parse(parameters: &str) -> Result<Self, WorkspacesError> {
        let mut config: Self = serde_json::from_str(parameters).map_err(|error| {
            crate::error(WorkspacesErrorKind::InvalidConfiguration, error.to_string())
        })?;
        config.cwd = config.cwd.replace('\\', "/");
        let absolute =
            config.cwd.starts_with('/') || config.cwd.as_bytes().get(1..3) == Some(b":/");
        if !absolute
            || config.cwd.contains('\0')
            || config.cwd.split('/').any(|part| matches!(part, "." | ".."))
        {
            return Err(crate::error(
                WorkspacesErrorKind::InvalidConfiguration,
                "cwd must be an absolute host path without . or .. components",
            ));
        }
        while config.cwd.len() > 1 && config.cwd.ends_with('/') && !config.cwd.ends_with(":/") {
            config.cwd.pop();
        }
        Ok(config)
    }
}

pub fn relative_path(path: &str) -> Result<&str, WorkspacesError> {
    if path.starts_with('/')
        || path.contains(['\\', ':', '\0'])
        || path.split('/').any(|part| part == "..")
    {
        return Err(crate::error(
            WorkspacesErrorKind::InvalidConfiguration,
            "path must be relative to the workspace without .. components",
        ));
    }
    Ok(if path.is_empty() { "." } else { path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_requires_a_directory() {
        let error = Config::parse("{}").unwrap_err();

        assert!(matches!(
            error.kind,
            WorkspacesErrorKind::InvalidConfiguration
        ));
    }

    #[test]
    fn parse_normalizes_directory() {
        let parameters = json!({
            "cwd": "/project/src/",
        })
        .to_string();

        let config = Config::parse(&parameters).unwrap();

        assert_eq!(config.cwd, "/project/src");
    }

    #[test]
    fn parse_preserves_windows_root() {
        let config = json!({
            "cwd": "C:\\",
        })
        .to_string();

        let config = Config::parse(&config).unwrap();

        assert_eq!(config.cwd, "C:/");
    }

    #[test]
    fn parse_rejects_ambiguous_directories() {
        for path in ["project", "/project/../other", "/project/./src", ""] {
            let config = json!({
                "cwd": path,
            })
            .to_string();

            let error = Config::parse(&config).unwrap_err();

            assert!(matches!(
                error.kind,
                WorkspacesErrorKind::InvalidConfiguration
            ));
        }
    }

    #[test]
    fn relative_path_rejects_parent_and_rooted_paths() {
        for path in ["../file", "/file", "dir/../../file", "C:/file", "dir\\file"] {
            let error = relative_path(path).unwrap_err();

            assert!(matches!(
                error.kind,
                WorkspacesErrorKind::InvalidConfiguration
            ));
        }
    }
}
