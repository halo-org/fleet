//! An AI agent harness.

mod api;
mod api_types;
mod error_handler;
mod fleet;
mod idle_handler;
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
};

use core::{
    net::{Ipv4Addr, SocketAddr},
    pin::pin,
    time::Duration,
};
use futures_util::future;
use std::{io, process::ExitCode};

fn main() -> ExitCode {
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can build tokio runtime");

    let idle_timeout = None;
    let fleet = Fleet::new(idle_timeout);

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let shutdown_timeout = Some(Duration::from_secs(10));

    match tokio_runtime.block_on(run(fleet, address, shutdown_timeout)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("HTTP server failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(
    fleet: Fleet,
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

    let (result, ()) = future::join(server, fleet.run()).await;

    result
}
