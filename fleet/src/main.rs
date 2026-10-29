//! An AI agent harness.

mod server;
mod shutdown;

use core::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use std::process::ExitCode;

fn main() -> ExitCode {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can build tokio runtime");

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let shutdown_timeout = Some(Duration::from_secs(10));

    match runtime.block_on(server::run(address, shutdown_timeout)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("HTTP server failed: {error}");
            ExitCode::FAILURE
        }
    }
}
