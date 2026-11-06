use crate::{
    WasmCapabilities, WasmError, WasmModels, WasmPrompts, WasmRoles, WasmState, WasmTools,
};
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy,
    component::{Component, Linker},
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

    /// Creates a role provider from a compiled component.
    ///
    /// # Errors
    ///
    /// Returns an error if the component belongs to another host, has unsupported
    /// imports, does not export the role interface or a directory cannot be opened.
    pub fn roles(
        &self,
        component: &WasmComponent,
        capabilities: WasmCapabilities,
    ) -> Result<WasmRoles, WasmError> {
        let linker = self.linker(&capabilities)?;
        WasmRoles::new(&component.component, &linker, capabilities)
    }

    /// Creates a model provider from a compiled component.
    ///
    /// # Errors
    ///
    /// Returns an error if the component belongs to another host, has unsupported
    /// imports, does not export the model interface or a directory cannot be opened.
    pub fn models(
        &self,
        component: &WasmComponent,
        capabilities: WasmCapabilities,
    ) -> Result<WasmModels, WasmError> {
        let linker = self.linker(&capabilities)?;
        WasmModels::new(&component.component, &linker, capabilities)
    }

    /// Creates a tool provider from a compiled component.
    ///
    /// # Errors
    ///
    /// Returns an error if the component belongs to another host, has unsupported
    /// imports, does not export the tool interface or a directory cannot be opened.
    pub fn tools(
        &self,
        component: &WasmComponent,
        capabilities: WasmCapabilities,
    ) -> Result<WasmTools, WasmError> {
        let linker = self.linker(&capabilities)?;
        WasmTools::new(&component.component, &linker, capabilities)
    }

    /// Creates a prompt provider from a compiled component.
    ///
    /// # Errors
    ///
    /// Returns an error if the component belongs to another host, has unsupported
    /// imports, does not export the prompt interface or a directory cannot be opened.
    pub fn prompts(
        &self,
        component: &WasmComponent,
        capabilities: WasmCapabilities,
    ) -> Result<WasmPrompts, WasmError> {
        let linker = self.linker(&capabilities)?;
        WasmPrompts::new(&component.component, &linker, capabilities)
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
        Ok(linker)
    }
}
