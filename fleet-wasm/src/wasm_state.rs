use crate::{
    WasmBlobs, WasmCredentials, WasmOrigins, WasmWorkers, WasmWorkspaceHost,
    bindings::{configuration, credentials},
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
    pub credentials: Option<WasmCredentials>,
    pub workspace_host: Option<WasmWorkspaceHost>,
}

pub struct WasmState {
    blobs: Option<WasmBlobs>,
    config: Arc<str>,
    credentials: Option<WasmCredentials>,
    workspace_host: Option<WasmWorkspaceHost>,
    workers: Option<WasmWorkers>,
    limits: StoreLimits,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    origins: WasmOrigins,
    table: ResourceTable,
}

impl WasmState {
    pub fn new(environment: WasmEnvironment, workers: Option<WasmWorkers>) -> Self {
        let WasmEnvironment {
            config,
            filesystem,
            origins,
            credentials,
            workspace_host,
        } = environment;
        let mut wasi = WasiCtx::default();
        *wasi.filesystem() = filesystem;

        Self {
            blobs: None,
            config,
            credentials,
            workspace_host,
            workers,
            limits: StoreLimits::default(),
            wasi,
            http: WasiHttpCtx::new(),
            origins,
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

    pub fn workspace_host(&self) -> Option<WasmWorkspaceHost> {
        self.workspace_host.clone()
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
        let credentials = store
            .with(|mut access| access.get().credentials.clone())
            .ok_or("credentials are not available")?;
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

    async fn refresh(
        store: &Accessor<WasmState, Self>,
        name: String,
        rejected: String,
    ) -> Result<bool, String> {
        let credentials = store
            .with(|mut access| access.get().credentials.clone())
            .ok_or("credentials are not available")?;
        credentials.refresh(&name, &rejected).await
    }
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
