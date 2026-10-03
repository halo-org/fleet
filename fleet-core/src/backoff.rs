use crate::ErrorHandler;
use core::error::Error;

pub struct Backoff<H> {
    retries: u32,
    error_handler: H,
}

impl<H> Backoff<H>
where
    H: ErrorHandler,
{
    pub const fn new(error_handler: H) -> Self {
        Self {
            retries: 0,
            error_handler,
        }
    }

    pub fn handle_result<T, E>(&mut self, result: &Result<T, E>) -> impl Future<Output = ()>
    where
        E: Error,
    {
        let future = match result {
            Ok(_) => None,
            Err(error) => Some(
                self.error_handler
                    .handle_error_with_backoff(error, self.retries),
            ),
        };

        async {
            match future {
                Some(future) => {
                    future.await;
                    self.retries = self.retries.saturating_add(1);
                }
                None => self.retries = 0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        error::Error,
        fmt,
        sync::atomic::{AtomicU32, AtomicU64, Ordering},
    };
    use futures_util::FutureExt as _;

    const OK: fmt::Result = Ok(());

    const ERROR: fmt::Result = Err(fmt::Error);

    struct CountingErrorHandler {
        count: AtomicU32,
        last_retries: AtomicU64,
        pending: bool,
    }

    impl CountingErrorHandler {
        fn new() -> Self {
            Self {
                count: AtomicU32::new(0),
                last_retries: AtomicU64::new(u64::MAX),
                pending: false,
            }
        }

        fn count(&self) -> u32 {
            self.count.load(Ordering::Relaxed)
        }

        fn last_retries(&self) -> Option<u32> {
            self.last_retries.load(Ordering::Relaxed).try_into().ok()
        }
    }

    #[expect(clippy::manual_async_fn, reason = "false positive")]
    impl ErrorHandler for CountingErrorHandler {
        fn handle_error<E>(&self, _error: E) -> impl Future<Output = ()> + Send
        where
            E: Error,
        {
            async {
                unreachable!("backoff should never call handle_error");
            }
        }

        fn handle_error_with_backoff<E>(
            &self,
            _error: E,
            retries: u32,
        ) -> impl Future<Output = ()> + Send
        where
            E: Error,
        {
            async move {
                self.count.fetch_add(1, Ordering::Relaxed);
                self.last_retries.store(retries.into(), Ordering::Relaxed);

                if self.pending {
                    core::future::pending::<()>().await;
                }
            }
        }
    }

    #[test]
    fn handle_result_increments_retries_on_error() {
        let mut backoff = Backoff::new(CountingErrorHandler::new());
        let result = ERROR;

        futures_executor::block_on(async {
            for n in 0..5 {
                backoff.handle_result(&result).await;
                assert_eq!(backoff.retries, n + 1);
            }
        });
    }

    #[test]
    fn handle_result_resets_retries_on_success() {
        let mut backoff = Backoff::new(CountingErrorHandler::new());
        let result = ERROR;

        futures_executor::block_on(async {
            for n in 0..5 {
                backoff.handle_result(&result).await;
                assert_eq!(backoff.retries, n + 1);
                assert_eq!(backoff.error_handler.count(), n + 1);
            }

            backoff.handle_result(&OK).await;
            assert_eq!(backoff.retries, 0);
            assert_eq!(backoff.error_handler.count(), 5);

            for n in 0..5 {
                backoff.handle_result(&result).await;
                assert_eq!(backoff.retries, n + 1);
                assert_eq!(backoff.error_handler.count(), n + 6);
            }
        });
    }

    #[test]
    fn handle_result_does_not_call_error_handler_on_success() {
        let mut backoff = Backoff::new(CountingErrorHandler::new());
        let result = OK;

        futures_executor::block_on(async {
            backoff.handle_result(&result).await;
            assert_eq!(backoff.error_handler.last_retries(), None);
            assert_eq!(backoff.error_handler.count(), 0);
        });
    }

    #[test]
    fn handle_result_passes_retries_to_error_handler() {
        let mut backoff = Backoff::new(CountingErrorHandler::new());
        let result = ERROR;

        futures_executor::block_on(async {
            for n in 0..5 {
                backoff.handle_result(&result).await;
                assert_eq!(backoff.error_handler.last_retries(), Some(n));
            }

            backoff.handle_result(&OK).await;

            for n in 0..5 {
                backoff.handle_result(&result).await;
                assert_eq!(backoff.error_handler.last_retries(), Some(n));
            }
        });
    }

    #[test]
    fn handle_result_preserves_retries_when_cancelled() {
        let mut backoff = Backoff::new(CountingErrorHandler::new());
        let result = ERROR;

        for n in 0..5 {
            let output = backoff.handle_result(&result).now_or_never();
            assert_eq!(output, Some(()));
            assert_eq!(backoff.retries, n + 1);
            assert_eq!(backoff.error_handler.count(), n + 1);
            assert_eq!(backoff.error_handler.last_retries(), Some(n));
        }

        backoff.error_handler.pending = true;

        for n in 0..5 {
            let output = backoff.handle_result(&result).now_or_never();
            assert_eq!(output, None);
            assert_eq!(backoff.retries, 5);
            assert_eq!(backoff.error_handler.count(), n + 6);
            assert_eq!(backoff.error_handler.last_retries(), Some(5));
        }

        backoff.error_handler.pending = false;

        for n in 0..5 {
            let output = backoff.handle_result(&result).now_or_never();
            assert_eq!(output, Some(()));
            assert_eq!(backoff.retries, n + 6);
            assert_eq!(backoff.error_handler.count(), n + 11);
            assert_eq!(backoff.error_handler.last_retries(), Some(n + 5));
        }
    }
}
