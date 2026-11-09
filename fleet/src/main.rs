//! An AI agent harness.

mod api;
mod api_types;
mod authentication;
mod authentication_api;
mod authentication_error;
mod credential_store;
mod directories;
mod error_handler;
mod fleet;
mod idle_handler;
mod oauth;
mod oauth_callback;
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
    authentication::Authentication,
    authentication_error::AuthenticationError,
    credential_store::CredentialStore,
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
use fleet_wasm::{Wasm, WasmCapabilities, WasmCredentials, WasmMount, WasmRegistry};
use futures_util::future;
use std::{fs, io, path::Path, process::ExitCode};

fn main() -> ExitCode {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("can install TLS provider");

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
    let roles = Roles;
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
    let authentication = match tokio_runtime.block_on(Authentication::open(
        &data_dir.join("authentication"),
        fleet.idle_handler().clone(),
    )) {
        Ok(authentication) => authentication,
        Err(error) => {
            eprintln!("Could not open authentication: {error}");
            return ExitCode::FAILURE;
        }
    };
    let providers = match tokio_runtime.block_on(load_providers(&config_dir, &authentication)) {
        Ok(providers) => providers,
        Err(error) => {
            eprintln!(
                "Could not load providers from {}: {error}",
                config_dir.display()
            );
            let mut source = error.source();
            while let Some(error) = source {
                eprintln!("  {error}");
                source = error.source();
            }
            return ExitCode::FAILURE;
        }
    };
    let fleet = fleet.with_authentication(authentication);

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

async fn load_providers(
    config_dir: &Path,
    authentication: &Authentication,
) -> Result<WasmRegistry, Box<dyn Error>> {
    let wasm = Wasm::new()?;
    let mut providers = WasmRegistry::new();
    let example_capabilities = WasmCapabilities {
        filesystem: vec![WasmMount {
            host_path: config_dir.into(),
            guest_path: "/config".into(),
            read_only: true,
        }],
        ..WasmCapabilities::default()
    };
    let openai_capabilities = WasmCapabilities {
        http: true,
        ..WasmCapabilities::default()
    };
    for (namespace, file, mut capabilities) in [
        ("example", "providers", example_capabilities),
        ("openai", "openai", openai_capabilities),
    ] {
        let bytes = match fs::read(config_dir.join(format!("{file}.wasm"))) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let config = match fs::read(config_dir.join(format!("{file}.json"))) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => JsonObject::new(),
            Err(error) => return Err(error.into()),
        };
        let component = wasm.compile(&bytes)?;
        capabilities.credentials = Some(WasmCredentials::new(authentication.scope(namespace)));
        let provider = wasm.provider(&component, &config, capabilities)?;
        authentication.register(
            namespace,
            provider.connections(authentication.host_id()).await?,
        )?;
        providers.insert(namespace, provider)?;
    }
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
