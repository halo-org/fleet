use crate::{
    Fleet,
    api::{ApiError, ResponseBody},
    authentication::AuthorizationStatus,
};
use http_body_util::{BodyExt as _, Full};
use hyper::{
    Method, Response, StatusCode,
    body::Bytes,
    header::{self, HeaderValue},
    http::request::Parts,
};

const SIGNED_IN: &str = "<!doctype html>\n<html lang=\"en\">\n<meta charset=\"utf-8\">\n\
    <title>Signed in</title>\n\
    <p>Signed in. You can close this page and return to Fleet.</p>\n</html>\n";
const FAILED: &str = "<!doctype html>\n<html lang=\"en\">\n<meta charset=\"utf-8\">\n\
    <title>Sign-in failed</title>\n\
    <p>Sign-in did not complete. Return to Fleet to try again.</p>\n</html>\n";

/// Completes an automatic client's authorization from the browser's callback.
///
/// The page never reflects the request. Unknown, expired and used states get the
/// failure page and change nothing.
pub async fn callback(fleet: &Fleet, parts: &Parts) -> Result<Response<ResponseBody>, ApiError> {
    if parts.method != Method::GET {
        return Err(ApiError::MethodNotAllowed("GET"));
    }
    let completed = match fleet.authentication() {
        Some(authentication) => {
            let authentication = authentication.clone();
            let query = parts.uri.query().map(str::to_owned);
            crate::authentication::finish(async move {
                authentication.hosted_callback(query.as_deref()).await
            })
            .await
            .is_some_and(|info| info.status == AuthorizationStatus::Complete)
        }
        None => false,
    };
    let (status, page) = if completed {
        (StatusCode::OK, SIGNED_IN)
    } else {
        (StatusCode::BAD_REQUEST, FAILED)
    };
    let mut response = Response::new(
        Full::new(Bytes::from_static(page.as_bytes()))
            .map_err(Into::into)
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    Ok(response)
}

/// Serves this installation's client ID metadata document.
///
/// Only an HTTPS public URL has one.
pub fn client(fleet: &Fleet, method: &Method) -> Result<Response<ResponseBody>, ApiError> {
    if method != Method::GET {
        return Err(ApiError::MethodNotAllowed("GET"));
    }
    let document = fleet
        .authentication()
        .and_then(crate::Authentication::client_metadata)
        .ok_or(ApiError::NotFound)?;
    let mut response = crate::api::json_response(StatusCode::OK, &document);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=3600"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AdminToken, Authentication, AutomaticClient, SandboxOAuth};
    use fleet_store::Store;
    use hyper::{HeaderMap, Request};
    use serde_json::{Value, json};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// Returns the response and whether the request counted as activity.
    fn call(
        public_url: Option<&str>,
        method: &str,
        path: &str,
    ) -> (StatusCode, HeaderMap, Bytes, bool) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let fleet = Fleet::new(Store::memory().unwrap(), None);
                let provider =
                    SandboxOAuth::new("http://127.0.0.1:1337/sandbox/oauth".parse().unwrap());
                let client = public_url.map(|url| AutomaticClient::new(url.into(), None));
                let authentication =
                    Authentication::sandbox(provider, client, fleet.idle_handler().clone())
                        .await
                        .unwrap();
                let fleet = fleet.with_authentication(authentication);
                let activity = fleet.idle_handler().last_activity();
                let request = Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Full::new(Bytes::new()))
                    .unwrap();
                let token = AdminToken::new(TOKEN.as_ref()).unwrap();
                let response = crate::admin_auth::handle(token, fleet.clone(), request)
                    .await
                    .unwrap();
                let (parts, body) = response.into_parts();
                let bytes = body.collect().await.unwrap().to_bytes();
                let active = fleet.idle_handler().last_activity() != activity;
                (parts.status, parts.headers, bytes, active)
            })
    }

    #[test]
    fn callback_pages_reveal_nothing_about_the_request() {
        for path in [
            "/oauth/callback",
            "/oauth/callback?state=%3Cscript%3Ealert(1)%3C/script%3E&code=granted",
            "/oauth/callback?error=%3Cb%3Edeclined%3C/b%3E",
        ] {
            let (status, headers, body, active) = call(Some("http://127.0.0.1:1337"), "GET", path);

            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body, FAILED.as_bytes());
            assert!(!active);
            let headers: Vec<_> = headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.to_str().unwrap()))
                .collect();
            assert_eq!(
                headers,
                [
                    ("content-type", "text/html; charset=utf-8"),
                    ("cache-control", "no-store"),
                    ("referrer-policy", "no-referrer"),
                    ("x-content-type-options", "nosniff"),
                    ("content-security-policy", "default-src 'none'"),
                ]
            );
        }
        for method in ["POST", "PUT", "HEAD"] {
            let (status, headers, _, _) =
                call(Some("http://127.0.0.1:1337"), method, "/oauth/callback");
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(headers[header::ALLOW], "GET");
        }
    }

    #[test]
    fn client_documents_are_served_only_for_https_public_urls() {
        let (status, headers, body, _) = call(
            Some("https://fleet.example.com"),
            "GET",
            "/oauth/client.json?client_id=https://other.example.com",
        );
        let document: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(headers[header::CACHE_CONTROL], "max-age=3600");
        assert_eq!(
            document,
            json!({
                "client_id": "https://fleet.example.com/oauth/client.json",
                "client_name": "Fleet",
                "redirect_uris": ["https://fleet.example.com/oauth/callback"],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })
        );

        for public_url in [Some("http://127.0.0.1:1337"), None] {
            let (status, _, _, _) = call(public_url, "GET", "/oauth/client.json");
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        let (status, _, _, _) = call(
            Some("https://fleet.example.com"),
            "POST",
            "/oauth/client.json",
        );
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
}
