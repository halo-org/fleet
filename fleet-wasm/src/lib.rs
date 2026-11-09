//! WebAssembly providers for [`fleet_core`] and [`fleet_kit`].
//!
//! Components implement the contracts in `wit/`. Compiled components can be
//! reused across providers. Each call uses fresh guest state and pooled resources.
//! Registries route `namespace:item` names to configured providers. Provider config
//! is available through the `configuration` import and is separate from call config.
//! WASI 0.3 capabilities are configured per provider. WASI operations require
//! a Tokio runtime with I/O and time enabled, as do host processes.
//! Clocks and randomness are available. The guest does not inherit environment
//! variables or standard streams, and raw sockets are unavailable.
//! The process capability runs host programs outside the WASI sandbox.
//! Workspace imports delegate to a [`fleet_kit::Workspaces`] implementation
//! through [`WasmWorkspaceHost`].
//!
//! ```no_run
//! use fleet_core::JsonObject;
//! use fleet_wasm::{Wasm, WasmCapabilities, WasmError, WasmMount, WasmRegistry};
//!
//! # fn example(bytes: &[u8]) -> Result<(), WasmError> {
//! let wasm = Wasm::new()?;
//! let component = wasm.compile(bytes)?;
//! let provider = wasm.provider(
//!     &component,
//!     &JsonObject::new(),
//!     WasmCapabilities {
//!         filesystem: vec![WasmMount {
//!             host_path: "./workspace".into(),
//!             guest_path: "/workspace".into(),
//!             read_only: true,
//!         }],
//!         ..WasmCapabilities::default()
//!     },
//! )?;
//! let mut registry = WasmRegistry::new();
//! registry.insert("example", provider)?;
//! # Ok(())
//! # }
//! ```

mod bindings;
mod convert;
mod wasm;
mod wasm_capabilities;
mod wasm_credentials;
mod wasm_error;
mod wasm_models;
mod wasm_process;
mod wasm_prompts;
mod wasm_registry;
mod wasm_state;
mod wasm_tools;
mod wasm_workspace_host;
mod wasm_workspaces;

pub use self::{
    wasm::{Wasm, WasmComponent},
    wasm_capabilities::{WasmCapabilities, WasmMount},
    wasm_credentials::WasmCredentials,
    wasm_error::WasmError,
    wasm_registry::{WasmProvider, WasmRegistry},
    wasm_workspace_host::WasmWorkspaceHost,
};

pub(crate) use self::{
    wasm_models::WasmModels, wasm_prompts::WasmPrompts, wasm_state::WasmState,
    wasm_tools::WasmTools, wasm_workspaces::WasmWorkspaces,
};
