use core::cfg_select;
use futures_util::{
    FutureExt as _,
    future::{self, BoxFuture, Shared},
};
use std::sync::LazyLock;

static SHUTDOWN_SIGNAL: LazyLock<Shared<BoxFuture<'static, ()>>> =
    LazyLock::new(|| wait_for_signal().boxed().shared());

/// Waits for a shutdown signal.
pub async fn wait_for_shutdown() {
    SHUTDOWN_SIGNAL.clone().await;
}

async fn wait_for_signal() {
    let future = cfg_select! {
        unix => wait_for_unix_signal(),
        windows => wait_for_windows_signal(),
        _ => core::future::pending::<()>(),
    };

    future.await;
}

#[cfg(unix)]
async fn wait_for_unix_signal() {
    use core::pin::pin;
    use tokio::signal::unix::{self, SignalKind};

    let mut interrupt =
        unix::signal(SignalKind::interrupt()).expect("SIGINT handler should register");
    let mut terminate =
        unix::signal(SignalKind::terminate()).expect("SIGTERM handler should register");

    let signals = [pin!(interrupt.recv()), pin!(terminate.recv())];

    let future = future::select_all(signals);

    future.await;
}

#[cfg(windows)]
async fn wait_for_windows_signal() {
    use core::pin::{Pin, pin};
    use tokio::signal::windows;

    let mut ctrl_c = windows::ctrl_c().expect("CTRL_C_EVENT handler should register");
    let mut ctrl_break = windows::ctrl_break().expect("CTRL_BREAK_EVENT handler should register");
    let mut ctrl_close = windows::ctrl_close().expect("CTRL_CLOSE_EVENT handler should register");
    let mut ctrl_shutdown =
        windows::ctrl_shutdown().expect("CTRL_SHUTDOWN_EVENT handler should register");

    let signals = [
        pin!(ctrl_c.recv()) as Pin<&mut (dyn Future<Output = _> + Send)>,
        pin!(ctrl_break.recv()),
        pin!(ctrl_close.recv()),
        pin!(ctrl_shutdown.recv()),
    ];

    let future = future::select_all(signals);

    future.await;
}
