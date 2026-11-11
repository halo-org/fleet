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
mod oauth;
mod oauth_callback;
mod plugins;
mod roles;
mod server;
mod signals;
mod subscription;

pub(crate) use self::{
    admin_auth::AdminToken,
    api_types::{
        ApiAgent, ApiAgentActivity, ApiAgentUpdates, ApiEvent, ApiMessage, ApiMessageRecord,
        ApiPreview, ApiPreviewKind, ApiPreviewSelection, ApiProvider, ApiRole, ApiToolCallRecord,
        CreateAgentRequest, ErrorDetails, ErrorResponse, EventsQuery, InterruptAgentRequest,
        SinceQuery,
    },
    authentication::Authentication,
    authentication_error::AuthenticationError,
    config::Config,
    credential_store::CredentialStore,
    error_chain::ErrorChain,
    error_handler::ErrorHandler,
    fleet::{Fleet, FleetError},
    idle_handler::IdleHandler,
    plugins::{PluginConfig, ProviderConfig},
    roles::Roles,
};

use core::{error::Error, net::SocketAddr, pin::pin, time::Duration};
use futures_util::future;
use std::{ffi::OsString, io, process::ExitCode};

const ADMIN_TOKEN_VARIABLE: &str = "FLEET_ADMIN_TOKEN";

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
    let lock = {
        let mut arguments = std::env::args_os().skip(1);
        let lock = match arguments.next() {
            None => false,
            Some(command) if command == "lock" => true,
            _ => return Err("usage: fleet [lock]".into()),
        };
        if arguments.next().is_some() {
            return Err("usage: fleet [lock]".into());
        }
        lock
    };

    let config_dir = directories::create_config_dir()?;
    let config = Config::load(&config_dir).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not load {}: {error}",
                config_dir.join("config.json").display(),
            ),
        )
    })?;
    let cache_dir = directories::create_cache_dir()?;
    if lock {
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
    let providers =
        crate::plugins::load(&config, &config_dir, &cache_dir, &authentication, &store).await?;
    let roles = Roles::new(config_dir.join("roles"));
    let address = config.listen;
    let fleet = fleet
        .with_authentication(authentication)
        .with_configuration(config, roles.clone());
    let agents = fleet.run(&roles, &providers, &providers);

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
