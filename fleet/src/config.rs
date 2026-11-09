use crate::{PluginConfig, ProviderConfig};
use core::net::{Ipv4Addr, SocketAddr};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, io, path::Path};

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
}

impl Config {
    /// Loads the server configuration.
    pub fn load(directory: &Path) -> io::Result<Self> {
        let bytes = fs::read(directory.join("config.json"))?;
        let config: Self = serde_json::from_slice(&bytes)
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

        Ok(config)
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 1337))
}
