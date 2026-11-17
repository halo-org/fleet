use crate::bindings::fleet::providers::configuration;
use serde::Deserialize;

/// The provider config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The base URL of an OAuth provider offered for sign-in.
    pub oauth: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        Self::parse(&configuration::get())
    }

    fn parse(config: &str) -> Result<Self, String> {
        serde_json::from_str(config).map_err(|error| format!("invalid config: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_an_optional_oauth_provider() {
        assert_eq!(Config::parse("{}").unwrap().oauth, None);
        assert_eq!(
            Config::parse(r#"{ "oauth": "http://127.0.0.1:1337/sandbox/oauth" }"#)
                .unwrap()
                .oauth
                .as_deref(),
            Some("http://127.0.0.1:1337/sandbox/oauth")
        );
        assert!(Config::parse(r#"{ "oath": "" }"#).is_err());
    }
}
