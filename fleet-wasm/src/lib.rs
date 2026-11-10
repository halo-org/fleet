#![expect(
    clippy::significant_drop_tightening,
    reason = "provider and connection ownership lasts through operation and stream cleanup"
)]

//! WebAssembly providers for [`fleet_core`] and [`fleet_kit`].
//!
//! Components implement the contracts in `wit/`. Compiled components can be
//! reused across providers. Ordinary calls use fresh guest state and pooled resources.
//! Registries route `namespace:item` names to configured providers. Provider config
//! is available through the `configuration` import and is separate from call config.
//! WASI 0.3 capabilities are configured per provider. WASI operations require
//! a Tokio runtime with I/O and time enabled, as do host processes.
//! Clocks and randomness are available. The guest does not inherit environment
//! variables or standard streams, and raw sockets are unavailable.
//! The process capability runs host programs outside the WASI sandbox.
//! Workspace imports delegate to a [`fleet_kit::Workspaces`] implementation
//! through [`WasmWorkspaceHost`]. Blob imports delegate through [`WasmBlobs`]. Model calls can
//! read only their input's blob references and cannot write. Tool access is supplied by an
//! agent-scoped factory. A plugin must declare the `fleet:providers/blobs` import to use it.
//! Metadata lookups receive no blob capability.
//!
//! # Private workers
//!
//! With [`WasmCapabilities::workers`] enabled, a component can import
//! `fleet:providers/workers` and export `fleet:providers/worker` alongside its ordinary
//! provider interfaces. `connect` starts or reuses a worker instance by a private key.
//! Clones of a configured provider share workers; separately configured providers do not.
//! Other plugins continue to use the ordinary provider interfaces, not worker identities.
//!
//! Each connection carries bounded, bidirectional byte streams. The component defines
//! framing, replies and cancellation. Dropping either connection endpoint signals its peer;
//! finishing a stream closes only that direction. Transport completion is not an operation
//! acknowledgement, and disconnection does not prove that external work was rolled back.
//! Check stream results and use an application protocol to determine operation success.
//!
//! Workers run independently on Tokio between calls. They receive the provider's static
//! capabilities, but no blob access or permission to start nested workers. Forward any
//! request-authorized blob data through a connection instead of passing ambient authority.
//! Workers retain memory and resources until they return, fail, retire or are stopped.
//! `listener.try-retire` atomically stops admission when no connections remain open.
//! There is no host idle timeout, persistence or automatic replay.
//!
//! Call [`WasmRegistry::shutdown`] or [`WasmProvider::shutdown`] before releasing providers
//! for graceful cleanup. Shutdown stops admission and gives workers five seconds to return
//! before dropping their instances. Worker execution uses a separately compiled, fuel-enabled
//! variant so CPU-bound workers yield periodically. Ordinary calls, including calls to a
//! worker-capable component, run without fuel instrumentation. The extra worker compilation
//! is shared by providers using the same compiled component.
//! Without explicit shutdown, workers are aborted when their provider and all in-flight
//! calls are dropped. SDK adapters that launch subprocesses should use `process.spawn-group`
//! so forced worker shutdown also terminates descendants remaining in the process group.
//! Ordinary `process.spawn` still terminates only its direct child. Group cleanup is supported
//! on Linux and macOS and errors on other platforms. Descendants that leave the group are not
//! contained; adapters must prevent daemonization or use a stronger external supervisor.
//! The WIT contract documents transport and resource limits.
//!
//! # Configuration
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
mod wasm_blobs;
mod wasm_capabilities;
mod wasm_credentials;
mod wasm_error;
mod wasm_models;
mod wasm_process;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod wasm_process_group;
mod wasm_prompts;
mod wasm_registry;
mod wasm_state;
mod wasm_tools;
mod wasm_worker_connection;
mod wasm_workers;
mod wasm_workspace_host;
mod wasm_workspaces;

pub use self::{
    wasm::{Wasm, WasmComponent},
    wasm_blobs::WasmBlobs,
    wasm_capabilities::{WasmCapabilities, WasmMount},
    wasm_credentials::WasmCredentials,
    wasm_error::WasmError,
    wasm_registry::{WasmProvider, WasmRegistry},
    wasm_workspace_host::WasmWorkspaceHost,
};

pub(crate) use self::{
    wasm_models::WasmModels, wasm_prompts::WasmPrompts, wasm_state::WasmState,
    wasm_tools::WasmTools, wasm_workers::WasmWorkers, wasm_workspaces::WasmWorkspaces,
};
