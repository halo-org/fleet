use crate::{
    WasmCredentials, WasmWorkspaceHost,
    bindings::{configuration, credentials},
};
use std::sync::Arc;
use wasmtime::component::{Accessor, HasSelf, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, filesystem::WasiFilesystemCtx};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

pub struct WasmState {
    config: Arc<str>,
    credentials: Option<WasmCredentials>,
    workspace_host: Option<WasmWorkspaceHost>,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}

impl WasmState {
    pub fn new(
        config: Arc<str>,
        filesystem: WasiFilesystemCtx,
        credentials: Option<WasmCredentials>,
        workspace_host: Option<WasmWorkspaceHost>,
    ) -> Self {
        let mut wasi = WasiCtx::default();
        *wasi.filesystem() = filesystem;

        Self {
            config,
            credentials,
            workspace_host,
            wasi,
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        }
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
            hooks: wasmtime_wasi_http::default_hooks(),
        }
    }
}
