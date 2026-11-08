use crate::bindings::configuration;
use std::sync::Arc;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, filesystem::WasiFilesystemCtx};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

pub struct WasmState {
    config: Arc<str>,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}

impl WasmState {
    pub fn new(config: Arc<str>, filesystem: WasiFilesystemCtx) -> Self {
        let mut wasi = WasiCtx::default();
        *wasi.filesystem() = filesystem;

        Self {
            config,
            wasi,
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        }
    }
}

impl configuration::Host for WasmState {
    fn get(&mut self) -> String {
        self.config.to_string()
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
