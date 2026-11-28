use crate::{
    WasmCapabilities, WasmError, WasmProvider, WasmState,
    bindings::{
        agents, blobs, configuration, credentials, process, roles, tool_calls, workers,
        workspace_access,
    },
};
use fleet_core::JsonObject;
use std::sync::Mutex;
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy,
    component::{Component, HasSelf, Linker},
};

/// A host for WebAssembly providers.
pub struct Wasm {
    engine: Engine,
    linker: Linker<WasmState>,
    worker_engine: Mutex<Option<Engine>>,
}

/// A compiled WebAssembly component.
#[derive(Clone)]
pub struct WasmComponent {
    component: Component,
    worker: Option<Component>,
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
        let linker = Self::linker(&engine)?;

        Ok(Self {
            engine,
            linker,
            worker_engine: Mutex::new(None),
        })
    }

    /// Compiles a WebAssembly component and its worker variant, if exported.
    ///
    /// A core module is first encoded as a component from its embedded component type,
    /// such as the output of a `wit-bindgen` guest build.
    ///
    /// Only the worker variant uses fuel instrumentation. Both variants are shared
    /// by providers configured from this component.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are not a supported component or encodable module.
    pub fn compile(&self, bytes: &[u8]) -> Result<WasmComponent, WasmError> {
        let encoded;
        let bytes = if bytes.starts_with(b"\0asm\x01\0\0\0") {
            encoded = wit_component::ComponentEncoder::default()
                .module(bytes)
                .and_then(|encoder| encoder.validate(true).encode())
                .map_err(|error| WasmError::runtime(wasmtime::Error::from_anyhow(error)))?;
            encoded.as_slice()
        } else {
            bytes
        };
        let component = Component::new(&self.engine, bytes).map_err(WasmError::runtime)?;
        let worker = if component
            .get_export_index(None, "fleet:providers/worker@0.0.0")
            .is_some()
        {
            let engine = {
                let mut engine = self
                    .worker_engine
                    .lock()
                    .expect("worker engine lock is not poisoned");
                if engine.is_none() {
                    let mut config = self.engine.config().clone();
                    config.consume_fuel(true);
                    *engine = Some(Engine::new(&config).map_err(WasmError::runtime)?);
                }
                engine
                    .as_ref()
                    .expect("worker engine is initialized")
                    .clone()
            };
            Some(Component::new(&engine, bytes).map_err(WasmError::runtime)?)
        } else {
            None
        };
        Ok(WasmComponent { component, worker })
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
        let worker = if capabilities.workers {
            if component.worker.is_none()
                && component
                    .component
                    .component_type()
                    .imports(&self.engine)
                    .any(|(name, _)| name == "fleet:providers/workers@0.0.0")
            {
                return Err(WasmError::provider(
                    "component imports workers but does not export worker".into(),
                ));
            }
            component
                .worker
                .as_ref()
                .map(|worker| {
                    Self::linker(worker.engine())?
                        .instantiate_pre(worker)
                        .map_err(WasmError::runtime)
                })
                .transpose()?
        } else {
            None
        };
        WasmProvider::new(
            &component.component,
            &self.linker,
            worker,
            config,
            capabilities,
        )
    }

    fn linker(engine: &Engine) -> Result<Linker<WasmState>, WasmError> {
        let mut linker = Linker::new(engine);
        configuration::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::cli::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::clocks::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::random::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::filesystem::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        wasmtime_wasi_http::p3::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        workers::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        process::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        credentials::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        blobs::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        workspace_access::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        roles::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        agents::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        tool_calls::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        Ok(linker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_components_do_not_consume_fuel() {
        let wasm = Wasm::new().unwrap();
        let component = wasm.compile(b"\0asm\x0d\0\x01\0").unwrap();
        let store = wasmtime::Store::new(component.component.engine(), ());

        assert!(store.get_fuel().is_err());
        assert!(component.worker.is_none());
    }
}
