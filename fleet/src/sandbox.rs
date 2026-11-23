use crate::{Authentication, Config, Fleet, RoleDefinition, SandboxOAuth};
use core::{
    error::Error,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};
use fleet_store::Store;
use fleet_wasm::WasmRegistry;
use std::{ffi::OsStr, fs, io, path::Path, sync::Arc};
use url::Url;

/// Opens a sandbox from its config and directory.
///
/// Stores are kept in memory. Plugins may use only capabilities within Fleet, and
/// sign-in reaches only the sandbox's OAuth provider. Roles are seeded from the
/// directory's `roles` subdirectory.
pub async fn open(
    config: Config,
    directory: &Path,
) -> Result<(Fleet, Arc<WasmRegistry>), Box<dyn Error + Send + Sync>> {
    for (name, plugin) in &config.plugins {
        if let Some(access) = plugin.outside_access() {
            return Err(
                format!("plugin {name} uses {access}, which the sandbox does not allow").into(),
            );
        }
    }
    let provider = SandboxOAuth::new(oauth_base(config.listen));
    let fleet = Fleet::new(Store::memory()?, None);
    let authentication =
        Authentication::sandbox(provider.clone(), fleet.idle_handler().clone()).await?;
    let providers = crate::plugins::load(&config, directory, None, &authentication, &fleet).await?;
    let fleet = fleet
        .with_authentication(authentication)
        .with_configuration(config)
        .with_sandbox(provider);
    seed_roles(&fleet, &directory.join("roles")).await?;
    Ok((fleet, providers))
}

fn oauth_base(listen: SocketAddr) -> Url {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    let address = SocketAddr::new(ip, listen.port());
    format!("http://{address}/sandbox/oauth")
        .parse()
        .expect("a socket address forms a URL")
}

async fn seed_roles(fleet: &Fleet, directory: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    for path in paths {
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .filter(|name| crate::roles::valid_name(name))
            .ok_or_else(|| format!("invalid role name: {}", path.display()))?;
        let definition: RoleDefinition = serde_json::from_slice(&fs::read(&path)?)
            .map_err(|error| format!("could not load {}: {error}", path.display()))?;
        let role = definition
            .into_role(name.into())
            .map_err(|error| format!("could not load {}: {error}", path.display()))?;
        fleet
            .put_role(format!("sandbox:{name}"), role, None)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
