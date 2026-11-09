use core::fmt;
use fleet_kit::{Credential, Credentials};
use futures_util::future::BoxFuture;
use std::sync::Arc;

/// Credentials available to a WebAssembly provider.
#[derive(Clone)]
pub struct WasmCredentials(Arc<dyn CredentialsObject>);

trait CredentialsObject: Send + Sync {
    fn get<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Credential>, String>>;
    fn refresh<'a>(
        &'a self,
        name: &'a str,
        rejected: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>>;
}

impl WasmCredentials {
    /// Wraps a credential provider.
    pub fn new(credentials: impl Credentials + Send + Sync + 'static) -> Self {
        Self(Arc::new(credentials))
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Credential>, String> {
        self.0.get(name).await
    }

    pub(crate) async fn refresh(&self, name: &str, rejected: &str) -> Result<bool, String> {
        self.0.refresh(name, rejected).await
    }
}

impl fmt::Debug for WasmCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmCredentials").finish_non_exhaustive()
    }
}

impl<C: Credentials + Send + Sync> CredentialsObject for C {
    fn get<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Credential>, String>> {
        Box::pin(async move { self.get(name).await.map_err(|error| error.to_string()) })
    }

    fn refresh<'a>(
        &'a self,
        name: &'a str,
        rejected: &'a str,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            self.refresh(name, rejected)
                .await
                .map_err(|error| error.to_string())
        })
    }
}
