use crate::bindings::{
    self,
    wasi::http::{client, types},
};
use bytes::Bytes;
use core::pin::Pin;
use fleet_models::Http;
use futures_util::{Stream, future, stream};
use http::{Request, Response};
use std::io;
use wit_bindgen::StreamResult;

pub struct WasiHttp;

impl Http for WasiHttp {
    type Error = io::Error;
    type Body = Pin<Box<dyn Stream<Item = Result<Bytes, Self::Error>> + Send>>;

    async fn send(&self, request: Request<Bytes>) -> Result<Response<Self::Body>, Self::Error> {
        let (parts, body) = request.into_parts();
        let headers = parts
            .headers
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let headers = types::Fields::from_list(&headers).map_err(io::Error::other)?;
        let (mut writer, contents) = bindings::wit_stream::new();
        let (_, trailers) = bindings::wit_future::new(|| Ok(None));
        let (request, _) = types::Request::new(headers, Some(contents), trailers, None);
        request
            .set_method(&types::Method::Other(parts.method.to_string()))
            .map_err(|()| io::Error::other("invalid HTTP method"))?;
        let scheme = match parts.uri.scheme_str() {
            Some("https") => types::Scheme::Https,
            Some("http") => types::Scheme::Http,
            _ => return Err(io::Error::other("unsupported HTTP scheme")),
        };
        request
            .set_scheme(Some(&scheme))
            .map_err(|()| io::Error::other("invalid HTTP scheme"))?;
        request
            .set_authority(parts.uri.authority().map(http::uri::Authority::as_str))
            .map_err(|()| io::Error::other("invalid HTTP authority"))?;
        request
            .set_path_with_query(
                parts
                    .uri
                    .path_and_query()
                    .map(http::uri::PathAndQuery::as_str),
            )
            .map_err(|()| io::Error::other("invalid HTTP path"))?;

        let ((), response) = future::join(
            async move {
                drop(writer.write_all(body.to_vec()).await);
                drop(writer);
            },
            client::send(request),
        )
        .await;
        let response = response.map_err(io::Error::other)?;
        let mut builder = Response::builder().status(response.get_status_code());
        for (name, value) in response.get_headers().copy_all() {
            builder = builder.header(name, value);
        }
        let (_, result) = bindings::wit_future::new(|| Ok(()));
        let (reader, trailers) = types::Response::consume_body(response, result);
        let body = stream::unfold(Some((reader, trailers)), |state| async move {
            let (mut reader, trailers) = state?;
            loop {
                let (status, bytes) = reader.read(Vec::with_capacity(16 * 1024)).await;
                if !bytes.is_empty() {
                    return Some((Ok(Bytes::from(bytes)), Some((reader, trailers))));
                }
                if matches!(status, StreamResult::Dropped) {
                    return trailers
                        .await
                        .err()
                        .map(|error| (Err(io::Error::other(error)), None));
                }
            }
        });
        let body: Self::Body = Box::pin(body);
        builder.body(body).map_err(io::Error::other)
    }
}
