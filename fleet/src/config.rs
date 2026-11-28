use crate::{PluginConfig, ProviderConfig};
use core::net::{Ipv4Addr, SocketAddr};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, io, path::Path};
use url::Url;

/// The server configuration.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The address to listen on.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The installed plugins.
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginConfig>,
    /// The configured providers.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// How long a root agent stays settled before its workspace is released, in milliseconds.
    #[serde(default)]
    pub release_settled_workspaces_after_ms: Option<u64>,
    /// The URL at which browsers reach the HTTP API.
    #[serde(default)]
    pub public_url: Option<Url>,
    /// A shared OAuth client metadata document for a public URL on 127.0.0.1.
    #[serde(default)]
    pub oauth_client_metadata_url: Option<Url>,
}

impl Config {
    /// Loads the server configuration.
    pub fn load(directory: &Path) -> io::Result<Self> {
        Self::parse(&fs::read(directory.join("config.json"))?)
    }

    /// Returns the URL at which browsers reach the HTTP API, without a trailing `/`.
    ///
    /// Defaults to the listening port on 127.0.0.1, unless that port is 0.
    pub fn public_url(&self) -> Option<String> {
        match &self.public_url {
            Some(url) => Some(url.as_str().trim_end_matches('/').to_owned()),
            None if self.listen.port() == 0 => None,
            None => Some(format!("http://127.0.0.1:{}", self.listen.port())),
        }
    }

    fn parse(bytes: &[u8]) -> io::Result<Self> {
        let config: Self = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

        for name in config.plugins.keys().chain(config.providers.keys()) {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid plugin or provider name: {name}"),
                ));
            }
        }

        for (name, provider) in &config.providers {
            if !config.plugins.contains_key(&provider.plugin) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "provider {name} refers to unknown plugin {}",
                        provider.plugin,
                    ),
                ));
            }
        }

        if let Some(url) = &config.public_url
            && (!matches!(url.scheme(), "http" | "https") || !plain(url))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "public_url must use HTTP or HTTPS without a query, fragment or credentials",
            ));
        }
        if let Some(url) = &config.oauth_client_metadata_url
            && (url.scheme() != "https" || url.path() == "/" || !plain(url))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oauth_client_metadata_url must use HTTPS with a path and no query, fragment or credentials",
            ));
        }

        Ok(config)
    }
}

/// Returns whether a URL has a host and no query, fragment or credentials.
fn plain(url: &Url) -> bool {
    url.host().is_some()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn default_listen() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 1337))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(settings: &serde_json::Value) -> io::Result<Config> {
        Config::parse(settings.to_string().as_bytes())
    }

    #[test]
    fn public_urls_default_to_the_listening_port_on_loopback() {
        let config = parse(&json!({ "listen": "0.0.0.0:8080" })).unwrap();
        assert_eq!(config.public_url().unwrap(), "http://127.0.0.1:8080");
        let config = parse(&json!({ "listen": "127.0.0.1:0" })).unwrap();
        assert_eq!(config.public_url(), None);

        for (url, expected) in [
            ("https://fleet.example.com", "https://fleet.example.com"),
            ("https://fleet.example.com/", "https://fleet.example.com"),
            (
                "https://fleet.example.com/api/",
                "https://fleet.example.com/api",
            ),
            ("http://127.0.0.1:1337", "http://127.0.0.1:1337"),
        ] {
            let config = parse(&json!({ "listen": "127.0.0.1:0", "public_url": url })).unwrap();
            assert_eq!(config.public_url().unwrap(), expected);
        }
        for url in [
            "ftp://fleet.example.com",
            "https://fleet.example.com/?next=/",
            "https://fleet.example.com/#top",
            "https://user@fleet.example.com",
            "file:///fleet",
            "fleet.example.com",
        ] {
            let error = parse(&json!({ "public_url": url })).err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{url}");
        }
    }

    #[test]
    fn shared_client_metadata_documents_use_https_with_a_path() {
        let url = "https://fleet.example.org/oauth/client.json";
        let config = parse(&json!({ "oauth_client_metadata_url": url })).unwrap();
        assert_eq!(config.oauth_client_metadata_url.unwrap().as_str(), url);

        for url in [
            "http://fleet.example.org/oauth/client.json",
            "https://fleet.example.org",
            "https://fleet.example.org/",
            "https://fleet.example.org/client.json?version=1",
            "https://fleet.example.org/client.json#top",
            "https://user:secret@fleet.example.org/client.json",
        ] {
            let error = parse(&json!({ "oauth_client_metadata_url": url }))
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{url}");
        }
    }
}
