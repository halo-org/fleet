use crate::{
    WasmCapabilities, WasmError, WasmProvider, WasmState,
    bindings::workspace_host::fleet::providers::workspaces,
    bindings::{blobs, configuration, credentials, process},
};
use fleet_core::JsonObject;
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy,
    component::{Component, HasSelf, Linker},
};

/// A host for WebAssembly providers.
pub struct Wasm {
    engine: Engine,
    linker: Linker<WasmState>,
}

/// A compiled WebAssembly component.
#[derive(Clone)]
pub struct WasmComponent {
    component: Component,
}

impl Wasm {
    /// Creates a host with pooled resources.
    ///
    /// # Errors
    ///
    /// Returns an error if the host cannot be initialized.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config.wasm_component_model_async(true);
        config.allocation_strategy(InstanceAllocationStrategy::pooling());
        let engine = Engine::new(&config).map_err(WasmError::runtime)?;
        let mut linker = Linker::new(&engine);
        configuration::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::cli::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::clocks::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::random::add_to_linker(&mut linker).map_err(WasmError::runtime)?;

        Ok(Self { engine, linker })
    }

    /// Compiles a WebAssembly component.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are not a supported component.
    pub fn compile(&self, bytes: &[u8]) -> Result<WasmComponent, WasmError> {
        let component = Component::new(&self.engine, bytes).map_err(WasmError::runtime)?;
        Ok(WasmComponent { component })
    }

    /// Configures a provider from a compiled component.
    ///
    /// # Errors
    ///
    /// Returns an error if the component belongs to another host, has unsupported
    /// imports, has no supported provider interfaces or a directory cannot be opened.
    pub fn provider(
        &self,
        component: &WasmComponent,
        config: &JsonObject,
        capabilities: WasmCapabilities,
    ) -> Result<WasmProvider, WasmError> {
        let linker = self.linker(&capabilities)?;
        WasmProvider::new(&component.component, &linker, config, capabilities)
    }

    fn linker(&self, capabilities: &WasmCapabilities) -> Result<Linker<WasmState>, WasmError> {
        let mut linker = self.linker.clone();
        if !capabilities.filesystem.is_empty() {
            wasmtime_wasi::p3::filesystem::add_to_linker(&mut linker)
                .map_err(WasmError::runtime)?;
        }
        if capabilities.http {
            wasmtime_wasi_http::p3::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        }
        if capabilities.process {
            process::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(WasmError::runtime)?;
        }
        if capabilities.credentials.is_some() {
            credentials::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(WasmError::runtime)?;
        }
        if capabilities.blobs.is_some() {
            blobs::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(WasmError::runtime)?;
        }
        if capabilities.workspaces.is_some() {
            workspaces::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(WasmError::runtime)?;
        }
        Ok(linker)
    }
}
