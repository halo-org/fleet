use core::{convert::Infallible, fmt};
use http::{HeaderMap, HeaderValue, header};

/// Provides credentials for HTTP requests.
pub trait Credentials {
    /// The error returned by the credential provider.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Returns the current authentication headers.
    ///
    /// # Errors
    ///
    /// Returns an error when credentials cannot be obtained.
    fn get(&self) -> impl Future<Output = Result<HeaderMap, Self::Error>> + Send;

    /// Refreshes rejected credentials and returns whether the request can be retried.
    ///
    /// `rejected` contains the headers used by the unauthorized request.
    ///
    /// # Errors
    ///
    /// Returns an error when credentials cannot be refreshed.
    fn refresh(
        &self,
        _rejected: &HeaderMap,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        core::future::ready(Ok(false))
    }
}

impl Credentials for HeaderMap {
    type Error = Infallible;

    fn get(&self) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        core::future::ready(Ok(self.clone()))
    }
}

/// A bearer API key.
#[derive(Clone)]
pub struct ApiKey(HeaderValue);

impl ApiKey {
    /// Creates credentials from an API key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key cannot be used in an HTTP header.
    pub fn new(key: &str) -> Result<Self, header::InvalidHeaderValue> {
        let mut value = HeaderValue::from_str(&format!("Bearer {key}"))?;
        value.set_sensitive(true);
        Ok(Self(value))
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKey").finish_non_exhaustive()
    }
}

impl Credentials for ApiKey {
    type Error = Infallible;

    fn get(&self) -> impl Future<Output = Result<HeaderMap, Self::Error>> + Send {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, self.0.clone());
        core::future::ready(Ok(headers))
    }
}
