use crate::{WasmCapabilities, WasmError, WasmState, bindings::roles::RoleProviderPre};
use fleet_core::{Role, RoleName, Roles};
use wasmtime::{
    Store,
    component::{Component, Linker},
};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;

/// A WebAssembly role provider.
pub struct WasmRoles {
    instance: RoleProviderPre<WasmState>,
    filesystem: WasiFilesystemCtx,
}

impl WasmRoles {
    pub(crate) fn new(
        component: &Component,
        linker: &Linker<WasmState>,
        capabilities: WasmCapabilities,
    ) -> Result<Self, WasmError> {
        let instance = linker
            .instantiate_pre(component)
            .map_err(WasmError::runtime)?;
        Ok(Self {
            instance: RoleProviderPre::new(instance).map_err(WasmError::runtime)?,
            filesystem: capabilities.open_filesystem()?,
        })
    }
}

impl Roles for WasmRoles {
    type Error = WasmError;

    async fn get(&self, name: &RoleName) -> Result<Option<Role>, Self::Error> {
        let mut store = Store::new(
            self.instance.engine(),
            WasmState::new(self.filesystem.clone()),
        );
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let role = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_roles()
                    .call_get(accessor, name.as_ref().into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::provider)?;

        role.map(TryInto::try_into).transpose()
    }
}
