use crate::{
    WasmCapabilities, WasmError, WasmProvider, WasmState,
    bindings::workspace_host::fleet::providers::workspaces,
    bindings::{blobs, configuration, credentials, process, workers},
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
        let linker = Self::base_linker(&engine)?;

        Ok(Self {
            engine,
            linker,
            worker_engine: Mutex::new(None),
        })
    }

    /// Compiles a WebAssembly component and its worker variant, if exported.
    ///
    /// Only the worker variant uses fuel instrumentation. Both variants are shared
    /// by providers configured from this component.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are not a supported component.
    pub fn compile(&self, bytes: &[u8]) -> Result<WasmComponent, WasmError> {
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
        let linker = Self::linker(self.linker.clone(), &capabilities)?;
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
                    Self::linker(Self::base_linker(worker.engine())?, &capabilities)?
                        .instantiate_pre(worker)
                        .map_err(WasmError::runtime)
                })
                .transpose()?
        } else {
            None
        };
        WasmProvider::new(&component.component, &linker, worker, config, capabilities)
    }

    fn base_linker(engine: &Engine) -> Result<Linker<WasmState>, WasmError> {
        let mut linker = Linker::new(engine);
        configuration::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::cli::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::clocks::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        wasmtime_wasi::p3::random::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        Ok(linker)
    }

    fn linker(
        mut linker: Linker<WasmState>,
        capabilities: &WasmCapabilities,
    ) -> Result<Linker<WasmState>, WasmError> {
        if !capabilities.filesystem.is_empty() {
            wasmtime_wasi::p3::filesystem::add_to_linker(&mut linker)
                .map_err(WasmError::runtime)?;
        }
        if capabilities.http {
            wasmtime_wasi_http::p3::add_to_linker(&mut linker).map_err(WasmError::runtime)?;
        }
        if capabilities.workers {
            workers::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(WasmError::runtime)?;
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
