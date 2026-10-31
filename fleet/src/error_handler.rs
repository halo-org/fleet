use core::{error::Error, future, time::Duration};

/// Logs runtime errors and delays retries.
pub struct ErrorHandler;

impl fleet_core::ErrorHandler for ErrorHandler {
    fn handle_error<E>(&self, error: E) -> impl Future<Output = ()> + Send
    where
        E: Error,
    {
        eprintln!("{error}");
        future::ready(())
    }

    fn handle_error_with_backoff<E>(
        &self,
        error: E,
        retries: u32,
    ) -> impl Future<Output = ()> + Send
    where
        E: Error,
    {
        eprintln!("{error}");

        let delay = Duration::from_millis(100)
            .saturating_mul(2_u32.saturating_pow(retries))
            .min(Duration::from_secs(300));

        tokio::time::sleep(delay)
    }
}
