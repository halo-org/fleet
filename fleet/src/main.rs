//! An AI agent harness.

mod api;
mod api_types;
mod directories;
mod error_handler;
mod fleet;
mod idle_handler;
mod roles;
mod server;
mod signals;
mod subscription;

pub(crate) use self::{
    api_types::{
        ApiAgent, ApiAgentActivity, ApiAgentUpdates, ApiEvent, ApiMessage, ApiMessageRecord,
        ApiPreview, ApiPreviewKind, ApiPreviewSelection, ApiToolCallRecord, CreateAgentRequest,
        ErrorDetails, ErrorResponse, EventsQuery, InterruptAgentRequest, SinceQuery,
    },
    error_handler::ErrorHandler,
    fleet::{Fleet, FleetError},
    idle_handler::IdleHandler,
    roles::Roles,
};

use core::{
    error::Error,
    net::{Ipv4Addr, SocketAddr},
    pin::pin,
    time::Duration,
};
use fleet_core::JsonObject;
use fleet_wasm::{Wasm, WasmCapabilities, WasmMount, WasmRegistry};
use futures_util::future;
use std::{fs, io, path::Path, process::ExitCode};

fn main() -> ExitCode {
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can build tokio runtime");

    let idle_timeout = None;
    let config_dir = match directories::create_config_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Could not create config directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let providers_path = config_dir.join("providers.wasm");
    let roles = Roles;
    let providers = match load_providers(&providers_path, &config_dir) {
        Ok(providers) => providers,
        Err(error) => {
            eprintln!("Could not load {}: {error}", providers_path.display());
            let mut source = error.source();
            while let Some(error) = source {
                eprintln!("  {error}");
                source = error.source();
            }
            return ExitCode::FAILURE;
        }
    };

    let data_dir = match directories::create_data_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Could not create data directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let database_path = data_dir.join("fleet.db");
    let store = match fleet_store::Store::open(&database_path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("Could not open {}: {error}", database_path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleet = Fleet::new(store, idle_timeout);

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let shutdown_timeout = Some(Duration::from_secs(10));

    let agents = fleet.run(&roles, &providers, &providers);
    match tokio_runtime.block_on(run(fleet.clone(), agents, address, shutdown_timeout)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("HTTP server failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn load_providers(path: &Path, config_dir: &Path) -> Result<WasmRegistry, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    let wasm = Wasm::new()?;
    let component = wasm.compile(&bytes)?;
    let capabilities = WasmCapabilities {
        filesystem: vec![WasmMount {
            host_path: config_dir.into(),
            guest_path: "/config".into(),
            read_only: true,
        }],
        ..WasmCapabilities::default()
    };
    let provider = wasm.provider(&component, &JsonObject::new(), capabilities)?;
    let mut providers = WasmRegistry::new();
    providers.insert("example", provider)?;
    Ok(providers)
}

async fn run(
    fleet: Fleet,
    agents: impl Future<Output = ()> + Send,
    address: SocketAddr,
    shutdown_timeout: Option<Duration>,
) -> io::Result<()> {
    let shutdown = async {
        future::select(
            pin!(signals::wait_for_shutdown()),
            pin!(fleet.wait_for_shutdown()),
        )
        .await;
        fleet.shutdown();
    };

    let server = async {
        let result = server::run(shutdown, fleet.clone(), address, shutdown_timeout).await;
        fleet.shutdown();
        result
    };

    let (result, ()) = future::join(server, agents).await;

    result
}
