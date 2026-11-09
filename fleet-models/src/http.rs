use bytes::Bytes;
use futures_util::Stream;
use http::{Request, Response};

/// Sends HTTP requests.
pub trait Http {
    /// The error returned by the client.
    type Error: core::error::Error + Send + Sync + 'static;
    /// The streamed response body.
    type Body: Stream<Item = Result<Bytes, Self::Error>> + Send + 'static;

    /// Sends a request and returns its response headers and body.
    ///
    /// Implementations must not forward credentials to another origin on redirects.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be sent.
    fn send(
        &self,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<Self::Body>, Self::Error>> + Send;
}
