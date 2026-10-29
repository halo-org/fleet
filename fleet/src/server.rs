use core::{convert::Infallible, error::Error, net::SocketAddr, pin::pin, time::Duration};
use futures_util::future::{self, Either};
use http_body_util::combinators::UnsyncBoxBody;
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Incoming},
    service,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::graceful::GracefulShutdown,
};
use std::io;

type ResponseBody = UnsyncBoxBody<Bytes, Box<dyn Error + Send + Sync>>;

/// Runs the HTTP server.
pub async fn run(address: SocketAddr, shutdown_timeout: Option<Duration>) -> io::Result<()> {
    let connections = GracefulShutdown::new();

    let result = {
        let shutdown = pin!(crate::shutdown::wait());
        let server = pin!(serve(address, &connections));

        match future::select(shutdown, server).await {
            Either::Left(((), _)) => Ok(()),
            Either::Right((result, _)) => result,
        }
    };

    if let Some(timeout) = shutdown_timeout {
        let timeout_result = tokio::time::timeout(timeout, connections.shutdown())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "HTTP shutdown timed out"));

        result.and(timeout_result)
    } else {
        connections.shutdown().await;
        result
    }
}

async fn serve(address: SocketAddr, connections: &GracefulShutdown) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    let http = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());

    eprintln!("Listening on http://{address}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let http = http.clone();
        let watcher = connections.watcher();

        tokio::spawn(async move {
            let connection = http.serve_connection(io, service::service_fn(handle_request));

            if let Err(error) = watcher.watch(connection).await {
                eprintln!("HTTP connection from {peer} failed: {error}");
            }
        });
    }
}

fn handle_request(
    _request: Request<Incoming>,
) -> impl Future<Output = Result<Response<ResponseBody>, Infallible>> {
    let mut response = Response::new(ResponseBody::default());
    *response.status_mut() = StatusCode::NOT_FOUND;
    future::ready(Ok(response))
}
