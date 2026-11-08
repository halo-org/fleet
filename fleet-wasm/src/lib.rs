//! WebAssembly providers for [`fleet_core`].
//!
//! Components implement the contracts in `wit/`. Compiled components can be
//! reused across providers. Each call uses fresh guest state and pooled resources.
//! Registries route `namespace:item` names to configured providers. Provider config
//! is available through the `configuration` import and is separate from call config.
//! WASI 0.3 capabilities are configured per provider. WASI operations require
//! a Tokio runtime with I/O and time enabled.
//! Clocks and randomness are available. Environment variables and standard
//! streams are not inherited, and raw sockets are unavailable.
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
mod wasm_error;
mod wasm_models;
mod wasm_prompts;
mod wasm_registry;
mod wasm_state;
mod wasm_tools;

pub use self::{
    wasm::{Wasm, WasmComponent},
    wasm_capabilities::{WasmCapabilities, WasmMount},
    wasm_error::WasmError,
    wasm_registry::{WasmProvider, WasmRegistry},
};

pub(crate) use self::{
    wasm_models::WasmModels, wasm_prompts::WasmPrompts, wasm_state::WasmState,
    wasm_tools::WasmTools,
};
