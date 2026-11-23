use crate::{Authentication, Config, Fleet, PluginRoles, PluginToolCalls, TaskHandles};
use core::{error::Error, fmt, time::Duration};
use fleet_core::JsonObject;
use fleet_wasm::{
    Wasm, WasmAgents, WasmBlobs, WasmCapabilities, WasmCredentials, WasmError, WasmMount,
    WasmOrigin, WasmProvider, WasmRegistry, WasmRoles, WasmToolCalls, WasmWorkspaceAccess,
    WasmWorkspaceHost,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
};

// === Configuration ===

/// A plugin source and its approved capabilities.
#[derive(Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum PluginConfig {
    /// A local component.
    Local {
        /// The component path.
        path: PathBuf,
        /// The approved capabilities.
        #[serde(default)]
        capabilities: PluginCapabilities,
    },
    /// A remote component pinned in the lockfile.
    Remote {
        /// The component URL, or an OCI reference prefixed with `oci://`.
        url: String,
        /// The approved capabilities.
        #[serde(default)]
        capabilities: PluginCapabilities,
    },
}

/// A configured plugin provider.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// The plugin name.
    pub plugin: String,
    /// The configuration passed to the plugin.
    #[serde(default)]
    pub config: JsonObject,
}

/// Access approved for a plugin.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "capabilities are independently approved permissions"
)]
pub struct PluginCapabilities {
    #[serde(default)]
    blobs: bool,
    #[serde(default)]
    http: Vec<String>,
    #[serde(default)]
    filesystem: Vec<PluginMount>,
    #[serde(default)]
    process: bool,
    #[serde(default)]
    workers: bool,
    #[serde(default)]
    credentials: bool,
    #[serde(default)]
    workspaces: Vec<String>,
    #[serde(default)]
    roles: RoleCapability,
    #[serde(default)]
    agents: bool,
    #[serde(default)]
    tool_calls: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RoleCapability {
    Enabled(bool),
    Permission(RolePermission),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RolePermission {
    Read,
}

impl Default for RoleCapability {
    fn default() -> Self {
        Self::Enabled(false)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginMount {
    host_path: PathBuf,
    guest_path: String,
    read_only: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Lockfile {
    plugins: BTreeMap<String, LockedPlugin>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LockedPlugin {
    url: String,
    sha256: String,
}

#[derive(Debug)]
struct LoadError {
    target: String,
    source: WasmError,
}

// === Loading ===

/// Loads the configured providers.
pub async fn load(
    config: &Config,
    directory: &Path,
    cache: Option<&Path>,
    authentication: &Authentication,
    fleet: &Fleet,
) -> Result<Arc<WasmRegistry>, Box<dyn Error + Send + Sync>> {
    let order = provider_order(config)?;
    let wasm = Wasm::new()?;
    let lockfile = Lockfile::load(directory)?;
    let mut components = BTreeMap::new();

    for (name, plugin) in &config.plugins {
        let bytes = match plugin {
            PluginConfig::Local { path, .. } => {
                let path = directory.join(path);
                fs::read(&path).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "could not read plugin {name} from {}: {error}",
                            path.display()
                        ),
                    )
                })?
            }
            PluginConfig::Remote { url, .. } => {
                let cache = cache.ok_or("remote plugins need a cache directory")?;
                let locked = lockfile.plugin(name, url)?;
                let path = cache.join(format!("{}.wasm", locked.sha256));
                let bytes = match fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        let bytes = download(&locked.url).await?;
                        locked.verify(&bytes)?;
                        write_file(&path, &bytes)?;
                        bytes
                    }
                    Err(error) => return Err(error.into()),
                };
                locked.verify(&bytes)?;
                bytes
            }
        };
        let component = wasm.compile(&bytes).map_err(|source| LoadError {
            target: format!("plugin {name}"),
            source,
        })?;
        components.insert(name, component);
    }

    let task_handles = Arc::new(TaskHandles::new());
    let mut loaded = BTreeMap::<&str, WasmProvider>::new();
    for name in order {
        let provider = config.providers.get(name).expect("provider exists");
        let plugin = config
            .plugins
            .get(&provider.plugin)
            .ok_or_else(|| io::Error::other(format!("unknown plugin: {}", provider.plugin)))?;
        let component = components.get(&provider.plugin).ok_or_else(|| {
            io::Error::other(format!("plugin was not loaded: {}", provider.plugin))
        })?;
        let capabilities = plugin.capabilities();
        let mut workspaces = WasmRegistry::new();
        for dependency in &capabilities.workspaces {
            let provider = loaded
                .get(dependency.as_str())
                .expect("dependency is loaded");
            workspaces.insert(dependency, provider.clone())?;
        }
        let capabilities = capabilities_for(
            capabilities,
            workspaces,
            &task_handles,
            fleet,
            name,
            directory,
            authentication,
        )
        .map_err(|source| LoadError {
            target: format!("plugin {}", provider.plugin),
            source,
        })?;
        let provider = wasm
            .provider(component, &provider.config, capabilities)
            .map_err(|source| LoadError {
                target: format!("provider {name}"),
                source,
            })?;
        authentication.register(name, provider.connections(authentication.host_id()).await?)?;
        loaded.insert(name, provider);
    }

    let mut providers = WasmRegistry::new();
    for (name, provider) in loaded {
        providers.insert(name, provider)?;
    }
    let providers = Arc::new(providers);
    task_handles.bind(&providers);
    Ok(providers)
}

fn capabilities_for(
    capabilities: &PluginCapabilities,
    workspaces: WasmRegistry,
    task_handles: &Arc<TaskHandles<WasmRegistry>>,
    fleet: &Fleet,
    name: &str,
    directory: &Path,
    authentication: &Authentication,
) -> Result<WasmCapabilities, WasmError> {
    let http = capabilities
        .http
        .iter()
        .map(|origin| origin.parse::<WasmOrigin>())
        .collect::<Result<_, _>>()?;
    let workspace_access = (!capabilities.workspaces.is_empty()).then(|| {
        let store = fleet.blob_store().clone();
        let locks = fleet.workspace_locks().clone();
        let workspaces = workspaces.clone();
        WasmWorkspaceAccess::new(move |agent| {
            fleet_workspaces::AgentWorkspaces::new(
                agent,
                store.agent_access(agent),
                store.clone(),
                locks.wrap(workspaces.clone()),
            )
        })
    });
    let blob_store = fleet.blob_store().clone();
    Ok(WasmCapabilities {
        blobs: capabilities.blobs.then(|| {
            WasmBlobs::new(blob_store.clone())
                .with_agents(move |id, call| blob_store.agent_blobs(id).for_call(call))
        }),
        http,
        filesystem: capabilities
            .filesystem
            .iter()
            .map(|mount| WasmMount {
                host_path: directory.join(&mount.host_path),
                guest_path: mount.guest_path.clone(),
                read_only: mount.read_only,
            })
            .collect(),
        process: capabilities.process,
        workers: capabilities.workers,
        credentials: capabilities
            .credentials
            .then(|| WasmCredentials::new(authentication.scope(name))),
        workspaces: (!capabilities.workspaces.is_empty())
            .then(|| WasmWorkspaceHost::new(workspaces)),
        workspace_access,
        roles: match capabilities.roles {
            RoleCapability::Enabled(false) => None,
            RoleCapability::Enabled(true) => Some(roles(fleet, name)),
            RoleCapability::Permission(RolePermission::Read) => {
                Some(roles(fleet, name).read_only())
            }
        },
        agents: capabilities.agents.then(|| agents(fleet)),
        tool_calls: capabilities
            .tool_calls
            .then(|| tool_calls(fleet, task_handles)),
    })
}

impl PluginConfig {
    /// Returns access beyond Fleet that the plugin requires, if any.
    pub const fn outside_access(&self) -> Option<&'static str> {
        match self {
            Self::Remote { .. } => Some("a remote URL"),
            Self::Local { capabilities, .. } => {
                if !capabilities.http.is_empty() {
                    Some("HTTP")
                } else if !capabilities.filesystem.is_empty() {
                    Some("filesystem")
                } else if capabilities.process {
                    Some("process")
                } else {
                    None
                }
            }
        }
    }

    const fn capabilities(&self) -> &PluginCapabilities {
        match self {
            Self::Local { capabilities, .. } | Self::Remote { capabilities, .. } => capabilities,
        }
    }
}

fn roles(fleet: &Fleet, provider: &str) -> WasmRoles {
    let fleet = fleet.clone();
    let provider = provider.to_owned();
    WasmRoles::new(move |id| PluginRoles::new(fleet.clone(), &provider, id))
}

fn agents(fleet: &Fleet) -> WasmAgents {
    let store = fleet.blob_store().clone();
    WasmAgents::new(move |agent, call| {
        let access = store.agent_access(agent);
        match call {
            Some(call) => access.for_call(call),
            None => access,
        }
    })
}

fn tool_calls(fleet: &Fleet, task_handles: &Arc<TaskHandles<WasmRegistry>>) -> WasmToolCalls {
    let store = fleet.blob_store().clone();
    let task_handles = task_handles.clone();
    WasmToolCalls::new(move |agent, call| {
        PluginToolCalls::new(store.clone(), agent, call, task_handles.clone())
    })
}

fn provider_order(config: &Config) -> io::Result<Vec<&str>> {
    let mut dependencies = BTreeMap::new();
    for (name, provider) in &config.providers {
        let plugin = config
            .plugins
            .get(&provider.plugin)
            .ok_or_else(|| io::Error::other(format!("unknown plugin: {}", provider.plugin)))?;
        for dependency in &plugin.capabilities().workspaces {
            if !config.providers.contains_key(dependency) {
                return Err(io::Error::other(format!(
                    "unknown workspace provider: {dependency}",
                )));
            }
        }
        dependencies.insert(name.as_str(), &plugin.capabilities().workspaces);
    }
    let mut pending: BTreeSet<_> = dependencies.keys().copied().collect();
    let mut order = Vec::new();
    while !pending.is_empty() {
        let name = pending
            .iter()
            .copied()
            .find(|name| {
                dependencies
                    .get(name)
                    .expect("provider exists")
                    .iter()
                    .all(|dependency| !pending.contains(dependency.as_str()))
            })
            .ok_or_else(|| io::Error::other("workspace provider dependencies contain a cycle"))?;
        pending.remove(name);
        order.push(name);
    }
    Ok(order)
}

// === Locking ===

/// Resolves remote plugins and writes the lockfile.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the WASM compiler is reused across the plugin loop"
)]
pub async fn lock(
    config: &Config,
    directory: &Path,
    cache: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let wasm = Wasm::new()?;
    let mut lockfile = Lockfile::default();

    for (name, plugin) in &config.plugins {
        let PluginConfig::Remote { url, .. } = plugin else {
            continue;
        };
        let bytes = download(url).await?;
        wasm.compile(&bytes)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        write_file(&cache.join(format!("{sha256}.wasm")), &bytes)?;
        lockfile.plugins.insert(
            name.clone(),
            LockedPlugin {
                url: url.clone(),
                sha256,
            },
        );
    }

    let mut bytes = serde_json::to_vec_pretty(&lockfile)?;
    bytes.push(b'\n');
    write_file(&directory.join("plugins.lock"), &bytes)?;
    Ok(())
}

impl Lockfile {
    fn load(directory: &Path) -> io::Result<Self> {
        match fs::read(directory.join("plugins.lock")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    fn plugin(&self, name: &str, url: &str) -> io::Result<&LockedPlugin> {
        let plugin = self
            .plugins
            .get(name)
            .filter(|plugin| plugin.url == url)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "plugin {name} is not locked for {url}; run fleet lock",
                ))
            })?;
        if plugin.sha256.len() != 64
            || !plugin
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid SHA-256 for plugin {name}"),
            ));
        }
        Ok(plugin)
    }
}

impl LockedPlugin {
    fn verify(&self, bytes: &[u8]) -> io::Result<()> {
        if format!("{:x}", Sha256::digest(bytes)) != self.sha256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("plugin hash does not match the lockfile for {}", self.url),
            ));
        }
        Ok(())
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not load {}", self.target)
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

// === Files ===

async fn download(url: &str) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    if let Some(reference) = url.strip_prefix("oci://") {
        return crate::oci::pull(&client, reference).await;
    }
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec())
}

fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut random = [0; 8];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let temporary = path.with_extension(format!("{:016x}.tmp", u64::from_ne_bytes(random)));
    let result = (|| {
        let mut file = fs::File::create_new(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        drop(fs::remove_file(&temporary));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_order_loads_workspaces_first() {
        let config: Config = serde_json::from_value(json!({
            "plugins": {
                "tools": {
                    "path": "tools.wasm",
                    "capabilities": {
                        "workspaces": ["workspace"],
                    },
                },
                "host": {
                    "path": "host.wasm",
                },
            },
            "providers": {
                "tools": {
                    "plugin": "tools",
                },
                "workspace": {
                    "plugin": "host",
                },
            },
        }))
        .unwrap();

        let order = provider_order(&config).unwrap();

        assert_eq!(order, ["workspace", "tools"]);
    }

    #[test]
    fn provider_order_rejects_missing_and_cyclic_dependencies() {
        for dependency in ["missing", "tools"] {
            let config: Config = serde_json::from_value(json!({
                "plugins": {
                    "tools": {
                        "path": "tools.wasm",
                        "capabilities": {
                            "workspaces": [dependency],
                        },
                    },
                },
                "providers": {
                    "tools": {
                        "plugin": "tools",
                    },
                },
            }))
            .unwrap();

            let error = provider_order(&config).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::Other);
        }
    }

    #[test]
    fn roles_can_be_read_only() {
        let plugin: PluginConfig = serde_json::from_value(json!({
            "path": "agents.wasm",
            "capabilities": { "roles": "read", "agents": true },
        }))
        .unwrap();
        assert!(matches!(
            plugin.capabilities().roles,
            RoleCapability::Permission(RolePermission::Read)
        ));
        let error = serde_json::from_value::<PluginConfig>(json!({
            "path": "agents.wasm",
            "capabilities": { "roles": "write" },
        }))
        .unwrap_err();
        assert!(error.is_data());
    }

    #[test]
    fn plugin_config_requires_one_source() {
        for value in [
            json!({}),
            json!({
                "path": "plugin.wasm",
                "url": "https://example.com/plugin.wasm",
            }),
            json!({
                "path": "plugin.wasm",
                "capabilities": {
                    "htpt": true,
                },
            }),
        ] {
            let error = serde_json::from_value::<PluginConfig>(value).unwrap_err();
            assert!(error.is_data());
        }
    }

    #[test]
    fn lockfile_requires_matching_source_and_digest() {
        let bytes = b"plugin";
        let lockfile = Lockfile {
            plugins: BTreeMap::from([(
                "example".into(),
                LockedPlugin {
                    url: "https://example.com/plugin.wasm".into(),
                    sha256: format!("{:x}", Sha256::digest(bytes)),
                },
            )]),
        };

        let plugin = lockfile
            .plugin("example", "https://example.com/plugin.wasm")
            .unwrap();
        plugin.verify(bytes).unwrap();
        let error = plugin.verify(b"changed").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let error = lockfile
            .plugin("example", "https://example.com/other.wasm")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);

        let error = lockfile
            .plugin("missing", "https://example.com/plugin.wasm")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }
}
