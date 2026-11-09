use crate::{AuthenticationError, oauth::Authorization};
use core::{convert::Infallible, pin::pin, time::Duration};
use futures_util::future::{self, Either};
use http_body_util::Full;
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header,
};
use hyper_util::rt::TokioIo;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

pub async fn wait(
    authorization: &Authorization,
    cancelled: oneshot::Receiver<()>,
) -> Result<BTreeMap<String, String>, AuthenticationError> {
    let (sender, receiver) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(sender)));
    let accept = async {
        loop {
            let (stream, _) = authorization
                .listener
                .accept()
                .await
                .map_err(AuthenticationError::Callback)?;
            let sender = sender.clone();
            let state = authorization.id.clone();
            let path = authorization.redirect_uri.path().to_owned();
            let authority = authorization.redirect_uri
                [url::Position::BeforeHost..url::Position::AfterPort]
                .to_owned();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let result = parse(&request, &path, &authority, &state);
                    let status = if let Ok(parameters) = result
                        && let Some(sender) =
                            sender.lock().expect("callback lock is not poisoned").take()
                        && sender.send(parameters).is_ok()
                    {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    let message = if status == StatusCode::OK {
                        "Sign-in received. Return to Fleet to see the result."
                    } else {
                        "This sign-in callback is invalid or has expired. Return to Fleet and try again."
                    };
                    let mut response =
                        Response::new(Full::new(Bytes::from_static(message.as_bytes())));
                    *response.status_mut() = status;
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        header::HeaderValue::from_static("text/plain; charset=utf-8"),
                    );
                    response.headers_mut().insert(
                        header::CACHE_CONTROL,
                        header::HeaderValue::from_static("no-store"),
                    );
                    response.headers_mut().insert(
                        header::REFERRER_POLICY,
                        header::HeaderValue::from_static("no-referrer"),
                    );
                    future::ready(Ok::<_, Infallible>(response))
                });
                let connection = hyper::server::conn::http1::Builder::new()
                    .keep_alive(false)
                    .serve_connection(TokioIo::new(stream), service);
                drop(tokio::time::timeout(Duration::from_secs(10), connection).await);
            });
        }
    };
    let callback = async {
        match future::select(pin!(receiver), pin!(accept)).await {
            Either::Left((result, _)) => result.map_err(|_| AuthenticationError::Cancelled),
            Either::Right((result, _)) => result,
        }
    };
    let callback = tokio::time::timeout(Duration::from_secs(600), callback);
    match future::select(pin!(cancelled), pin!(callback)).await {
        Either::Left(_) => Err(AuthenticationError::Cancelled),
        Either::Right((result, _)) => result.map_err(|_| AuthenticationError::Cancelled)?,
    }
}

fn parse<B>(
    request: &Request<B>,
    path: &str,
    authority: &str,
    state: &str,
) -> Result<BTreeMap<String, String>, AuthenticationError> {
    if request.method() != Method::GET
        || request.uri().path() != path
        || request
            .headers()
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            != Some(authority)
    {
        return Err(AuthenticationError::Invalid("invalid callback request"));
    }
    let mut parameters = BTreeMap::new();
    for (name, value) in
        form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
    {
        if parameters
            .insert(name.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(AuthenticationError::Invalid("duplicate callback parameter"));
        }
    }
    if parameters.get("state").map(String::as_str) != Some(state) {
        return Err(AuthenticationError::Invalid("sign-in state does not match"));
    }
    Ok(parameters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_mismatched_and_duplicate_state() {
        for query in [
            "code=test",
            "state=wrong&code=test",
            "state=expected&state=expected&code=test",
        ] {
            let request = Request::builder()
                .uri(format!("/callback?{query}"))
                .header(header::HOST, "127.0.0.1:1455")
                .body(())
                .unwrap();
            let error = parse(&request, "/callback", "127.0.0.1:1455", "expected").unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }
    }

    #[test]
    fn parse_preserves_the_issued_client_id() {
        let request = Request::builder()
            .uri("/callback?state=expected&code=test&client_id=issued")
            .header(header::HOST, "127.0.0.1:1455")
            .body(())
            .unwrap();
        let parameters = parse(&request, "/callback", "127.0.0.1:1455", "expected").unwrap();

        assert_eq!(parameters.get("client_id").unwrap(), "issued");
        assert_eq!(parameters.get("code").unwrap(), "test");
    }
}
