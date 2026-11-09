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
        if !is_absolute(&config.cwd) || config.cwd.contains('\0') {
            return Err(crate::error(
                WorkspacesErrorKind::InvalidConfiguration,
                "cwd must be an absolute host path without null bytes",
            ));
        }
        while config.cwd.len() > 1 && config.cwd.ends_with('/') && !config.cwd.ends_with(":/") {
            config.cwd.pop();
        }
        Ok(config)
    }

    pub fn path(&self, path: &str) -> Result<String, WorkspacesError> {
        if path.contains('\0') {
            return Err(crate::error(
                WorkspacesErrorKind::InvalidConfiguration,
                "path must not contain null bytes",
            ));
        }
        let path = path.replace('\\', "/");
        Ok(if is_absolute(&path) {
            path
        } else if path.is_empty() {
            self.cwd.clone()
        } else {
            format!("{}/{path}", self.cwd.trim_end_matches('/'))
        })
    }
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
        || (path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && path.as_bytes().get(1..3) == Some(b":/"))
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
        for path in ["project", "C:project", "", "/project\0"] {
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
    fn path_joins_relative_paths_without_resolving_parents() {
        let config = Config {
            cwd: "/project".into(),
        };

        let path = config.path("link/../file").unwrap();
        assert_eq!(path, "/project/link/../file");

        let path = config.path("../other/file").unwrap();
        assert_eq!(path, "/project/../other/file");
    }

    #[test]
    fn path_preserves_absolute_paths() {
        let config = Config {
            cwd: "/project".into(),
        };

        let path = config.path("/other/file").unwrap();
        assert_eq!(path, "/other/file");

        let path = config.path("C:\\other\\file").unwrap();
        assert_eq!(path, "C:/other/file");
    }
}
