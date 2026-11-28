use core::fmt;
use fleet_kit::{Credential, Credentials};
use futures_util::future::BoxFuture;
use std::sync::Arc;

/// Credentials available to a WebAssembly provider.
#[derive(Clone)]
pub struct WasmCredentials(Arc<dyn CredentialsObject>);

trait CredentialsObject: Send + Sync {
    fn get<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Credential>, String>>;
    fn reject<'a>(
        &'a self,
        name: &'a str,
        token: Option<&'a str>,
        challenge: Option<&'a str>,
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

    pub(crate) async fn reject(
        &self,
        name: &str,
        token: Option<&str>,
        challenge: Option<&str>,
    ) -> Result<bool, String> {
        self.0.reject(name, token, challenge).await
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

    fn reject<'a>(
        &'a self,
        name: &'a str,
        token: Option<&'a str>,
        challenge: Option<&'a str>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            self.reject(name, token, challenge)
                .await
                .map_err(|error| error.to_string())
        })
    }
}
