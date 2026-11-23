//! An AI agent harness.

mod admin_auth;
mod api;
mod api_types;
mod authentication;
mod authentication_api;
mod authentication_error;
mod blob_api;
mod config;
mod credential_store;
mod directories;
mod error_chain;
mod error_handler;
mod fleet;
mod idle_handler;
mod notification_api;
mod notifications;
mod oauth;
mod oauth_callback;
mod oci;
mod plugin_roles;
mod plugin_tool_calls;
mod plugins;
mod roles;
mod sandbox;
mod sandbox_oauth;
mod server;
mod signals;
mod subscription;
mod web_push;
mod workspace_locks;

pub(crate) use self::{
    admin_auth::AdminToken,
    api_types::{
        ApiAgent, ApiAgentActivity, ApiAgentUpdates, ApiEvent, ApiMessage, ApiMessageRecord,
        ApiPreview, ApiPreviewKind, ApiPreviewSelection, ApiProvider, ApiRole, ApiRoleDefinition,
        ApiToolCallRecord, CreateAgentRequest, ErrorDetails, ErrorResponse, EventsQuery,
        InterruptAgentRequest, InterruptToolCallsRequest, SinceQuery,
    },
    authentication::Authentication,
    authentication_error::AuthenticationError,
    config::Config,
    credential_store::CredentialStore,
    error_chain::ErrorChain,
    error_handler::ErrorHandler,
    fleet::{Fleet, FleetError},
    idle_handler::IdleHandler,
    notifications::Notifications,
    plugin_roles::PluginRoles,
    plugin_tool_calls::{PluginToolCalls, TaskHandles},
    plugins::{PluginConfig, ProviderConfig},
    roles::RoleDefinition,
    sandbox_oauth::SandboxOAuth,
    workspace_locks::WorkspaceLocks,
};

use core::{error::Error, net::SocketAddr, pin::pin, time::Duration};
use fleet_wasm::WasmRegistry;
use futures_util::future;
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

const ADMIN_TOKEN_VARIABLE: &str = "FLEET_ADMIN_TOKEN";
const USAGE: &str = "usage: fleet [lock | sandbox <directory>]";

enum Command {
    Serve,
    Lock,
    Sandbox(PathBuf),
}

fn main() -> ExitCode {
    let admin_token = std::env::var_os(ADMIN_TOKEN_VARIABLE);
    // SAFETY: No other threads have started. Removal keeps the token from child processes.
    unsafe {
        std::env::remove_var(ADMIN_TOKEN_VARIABLE);
    }

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("can install TLS provider");

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can build tokio runtime");

    match tokio_runtime.block_on(start(admin_token)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Fleet failed: {}", ErrorChain(error.as_ref()));
            ExitCode::FAILURE
        }
    }
}

async fn start(admin_token: Option<OsString>) -> Result<(), Box<dyn Error + Send + Sync>> {
    let command = {
        let mut arguments = std::env::args_os().skip(1);
        let command = match arguments.next() {
            None => Command::Serve,
            Some(command) if command == "lock" => Command::Lock,
            Some(command) if command == "sandbox" => {
                Command::Sandbox(arguments.next().ok_or(USAGE)?.into())
            }
            _ => return Err(USAGE.into()),
        };
        if arguments.next().is_some() {
            return Err(USAGE.into());
        }
        command
    };
    if let Command::Sandbox(directory) = command {
        return sandbox(&directory, admin_token).await;
    }

    let config_dir = directories::create_config_dir()?;
    let config = load_config(&config_dir)?;
    let cache_dir = directories::create_cache_dir()?;
    if matches!(command, Command::Lock) {
        crate::plugins::lock(&config, &config_dir, &cache_dir).await?;
        eprintln!("Updated {}", config_dir.join("plugins.lock").display());
        return Ok(());
    }

    let admin_token = admin_token.ok_or_else(|| format!("{ADMIN_TOKEN_VARIABLE} must be set"))?;
    let admin_token = AdminToken::new(&admin_token)?;

    let data_dir = directories::create_data_dir()?;
    let store = fleet_store::Store::open(data_dir.join("fleet.db"))?;
    let fleet = Fleet::new(store.clone(), None);
    let authentication = Authentication::open(
        &data_dir.join("authentication"),
        fleet.idle_handler().clone(),
    )
    .await?;
    let notifications = Notifications::open(&data_dir.join("notifications")).await?;
    let providers = crate::plugins::load(
        &config,
        &config_dir,
        Some(&cache_dir),
        &authentication,
        &fleet,
    )
    .await?;
    let address = config.listen;
    let fleet = fleet
        .with_authentication(authentication)
        .with_notifications(notifications)
        .with_configuration(config);
    serve(admin_token, fleet, providers, address).await
}

async fn sandbox(
    directory: &Path,
    admin_token: Option<OsString>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let config = load_config(directory)?;
    let address = config.listen;
    let admin_token = if let Some(token) = admin_token {
        AdminToken::new(&token)?
    } else {
        let token = crate::oauth::random_value()?;
        eprintln!("Admin token: {token}");
        AdminToken::new(token.as_ref())?
    };
    let (fleet, providers) = crate::sandbox::open(config, directory).await?;
    serve(admin_token, fleet, providers, address).await
}

fn load_config(directory: &Path) -> io::Result<Config> {
    Config::load(directory).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not load {}: {error}",
                directory.join("config.json").display(),
            ),
        )
    })
}

async fn serve(
    admin_token: AdminToken,
    fleet: Fleet,
    providers: Arc<WasmRegistry>,
    address: SocketAddr,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let agents = fleet.run(&*providers, &*providers, &*providers);
    let result = run(
        admin_token,
        fleet.clone(),
        agents,
        address,
        Some(Duration::from_secs(10)),
    )
    .await;
    providers.shutdown().await;
    drop(providers);
    result?;
    Ok(())
}

async fn run(
    admin_token: AdminToken,
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
        let result = server::run(
            shutdown,
            admin_token,
            fleet.clone(),
            address,
            shutdown_timeout,
        )
        .await;
        fleet.shutdown();
        result
    };

    let (result, ()) = future::join(server, agents).await;

    result
}
