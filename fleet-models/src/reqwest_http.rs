use crate::Http;
use bytes::Bytes;
use core::{pin::Pin, time::Duration};
use futures_util::Stream;
use http::{Request, Response};

/// An HTTP client backed by Reqwest.
///
/// Requests require a Tokio runtime with I/O and time enabled.
#[derive(Debug, Clone)]
pub struct ReqwestHttp(reqwest::Client);

impl ReqwestHttp {
    /// Creates an HTTP client with TLS and connection pooling.
    ///
    /// # Errors
    ///
    /// Returns an error when the client cannot be initialized.
    pub fn new() -> Result<Self, reqwest::Error> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()
            .map(Self)
    }

    /// Uses an existing client.
    ///
    /// The client must not forward credentials to another origin on redirects.
    #[must_use]
    pub const fn with_client(client: reqwest::Client) -> Self {
        Self(client)
    }
}

impl Http for ReqwestHttp {
    type Error = reqwest::Error;
    type Body = Pin<Box<dyn Stream<Item = Result<Bytes, Self::Error>> + Send>>;

    async fn send(&self, request: Request<Bytes>) -> Result<Response<Self::Body>, Self::Error> {
        let (parts, body) = request.into_parts();
        let response = self
            .0
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body)
            .send()
            .await?;

        let status = response.status();
        let version = response.version();
        let headers = response.headers().clone();
        let body: Self::Body = Box::pin(response.bytes_stream());
        let mut response = Response::new(body);
        *response.status_mut() = status;
        *response.version_mut() = version;
        *response.headers_mut() = headers;
        Ok(response)
    }
}
