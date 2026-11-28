use crate::{
    WasmAgents, WasmBlobs, WasmCapability, WasmCredentials, WasmOrigins, WasmProcessAccess,
    WasmToolCalls, WasmWorkers, WasmWorkspaceAccess,
    bindings::{configuration, credentials, process},
    wasm_roles::WasmAgentRoles,
};
use std::sync::Arc;
use wasmtime::{
    Engine, Store, StoreLimits,
    component::{Accessor, HasSelf, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, filesystem::WasiFilesystemCtx};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

/// Static capabilities shared by a provider's instances.
#[derive(Clone, Default)]
pub struct WasmEnvironment {
    pub config: Arc<str>,
    pub filesystem: WasiFilesystemCtx,
    pub origins: WasmOrigins,
    pub process: Arc<WasmProcessAccess>,
    pub credentials: Option<WasmCredentials>,
    pub agents: Option<WasmAgents>,
    pub workspace_access: Option<WasmWorkspaceAccess>,
    pub tool_calls: Option<WasmToolCalls>,
    pub granted: Arc<[WasmCapability]>,
}

pub struct WasmState {
    agents: Option<WasmAgents>,
    workspace_access: Option<WasmWorkspaceAccess>,
    tool_calls: Option<WasmToolCalls>,
    blobs: Option<WasmBlobs>,
    config: Arc<str>,
    credentials: Option<WasmCredentials>,
    roles: Option<WasmAgentRoles>,
    workers: Option<WasmWorkers>,
    limits: StoreLimits,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    origins: WasmOrigins,
    process: Arc<WasmProcessAccess>,
    granted: Arc<[WasmCapability]>,
    table: ResourceTable,
}

impl WasmState {
    pub fn new(environment: WasmEnvironment, workers: Option<WasmWorkers>) -> Self {
        let WasmEnvironment {
            config,
            filesystem,
            origins,
            process,
            credentials,
            agents,
            workspace_access,
            tool_calls,
            granted,
        } = environment;
        let mut wasi = WasiCtx::default();
        *wasi.filesystem() = filesystem;

        Self {
            agents,
            workspace_access,
            tool_calls,
            blobs: None,
            config,
            credentials,
            roles: None,
            workers,
            limits: StoreLimits::default(),
            wasi,
            http: WasiHttpCtx::new(),
            origins,
            process,
            granted,
            table: ResourceTable::new(),
        }
    }

    pub fn into_store(self, engine: &Engine) -> Store<Self> {
        Store::new(engine, self)
    }

    pub fn workers(&self) -> Option<WasmWorkers> {
        self.workers.clone()
    }

    pub const fn limits(&mut self) -> &mut StoreLimits {
        &mut self.limits
    }

    pub fn agents(&self) -> Option<WasmAgents> {
        self.agents.clone()
    }

    pub fn set_agents(&mut self, agents: Option<WasmAgents>) {
        self.agents = agents;
    }

    pub fn tool_calls(&self) -> Option<WasmToolCalls> {
        self.tool_calls.clone()
    }

    pub fn set_tool_calls(&mut self, tool_calls: Option<WasmToolCalls>) {
        self.tool_calls = tool_calls;
    }

    pub fn blobs(&self) -> Option<WasmBlobs> {
        self.blobs.clone()
    }

    pub fn set_blobs(&mut self, blobs: Option<WasmBlobs>) {
        self.blobs = blobs;
    }

    pub fn with_blobs(mut self, blobs: Option<WasmBlobs>) -> Self {
        self.blobs = blobs;
        self
    }

    pub fn roles(&self) -> Option<WasmAgentRoles> {
        self.roles.clone()
    }

    pub fn set_roles(&mut self, roles: Option<WasmAgentRoles>) {
        self.roles = roles;
    }

    pub fn workspace_access(&self) -> Option<WasmWorkspaceAccess> {
        self.workspace_access.clone()
    }

    pub fn set_workspace_access(&mut self, access: Option<WasmWorkspaceAccess>) {
        self.workspace_access = access;
    }

    pub fn require(&self, capability: WasmCapability) -> Result<(), String> {
        if self.granted.contains(&capability) {
            Ok(())
        } else {
            Err(capability.denial())
        }
    }

    pub fn require_command(&self, command: &process::Command) -> Result<(), String> {
        self.require(WasmCapability::Process)?;
        self.process.require(command)
    }

    pub fn unavailable(&self, capability: WasmCapability, message: &str) -> String {
        self.require(capability)
            .map_or_else(|denial| denial, |()| message.into())
    }

    pub const fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl configuration::Host for WasmState {
    fn get(&mut self) -> String {
        self.config.to_string()
    }
}

impl credentials::Host for WasmState {}

impl credentials::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn get(
        store: &Accessor<WasmState, Self>,
        name: String,
    ) -> Result<Option<credentials::Credential>, String> {
        let credentials = credentials(store)?;
        Ok(credentials
            .get(&name)
            .await?
            .map(|credential| match credential {
                fleet_kit::Credential::ApiKey(key) => credentials::Credential::ApiKey(key),
                fleet_kit::Credential::OAuth {
                    access_token,
                    id_token,
                } => credentials::Credential::Oauth(credentials::OauthCredential {
                    access_token,
                    id_token,
                }),
            }))
    }

    async fn reject(
        store: &Accessor<WasmState, Self>,
        name: String,
        token: Option<String>,
        challenge: Option<String>,
    ) -> Result<bool, String> {
        credentials(store)?
            .reject(&name, token.as_deref(), challenge.as_deref())
            .await
    }
}

fn credentials(store: &Accessor<WasmState, HasSelf<WasmState>>) -> Result<WasmCredentials, String> {
    store.with(|mut access| {
        let state = access.get();
        state.credentials.clone().ok_or_else(|| {
            state.unavailable(WasmCapability::Credentials, "credentials are not available")
        })
    })
}

impl WasiView for WasmState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for WasmState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.origins,
        }
    }
}
