use crate::{
    Fleet,
    api::{self, ApiError, ResponseBody},
};
use core::{convert::Infallible, error::Error};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode,
    body::{Body, Bytes},
    header::{self, HeaderValue},
    http::{request::Parts, uri::Authority},
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{ffi::OsStr, io};

const COOKIE_NAME: &str = "fleet_session";
const COOKIE_MAX_AGE_SECONDS: u32 = 2_592_000;
const MIN_TOKEN_BYTES: usize = 32;

/// The secret that grants access to the HTTP API.
#[derive(Clone)]
pub struct AdminToken {
    digest: [u8; 32],
}

impl AdminToken {
    /// Validates a token that is safe to carry in a header or cookie.
    pub fn new(value: &OsStr) -> io::Result<Self> {
        value
            .to_str()
            .filter(|value| {
                value.len() >= MIN_TOKEN_BYTES
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/=".contains(&byte))
            })
            .map(|value| Self {
                digest: Sha256::digest(value).into(),
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the admin token must be at least 32 characters from A-Z, a-z, 0-9 and -._~+/=",
                )
            })
    }

    fn matches(&self, value: &str) -> bool {
        self.digest == <[u8; 32]>::from(Sha256::digest(value))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignInRequest {
    token: String,
}

/// Handles an HTTP request, requiring the admin token for the API.
pub async fn handle<B>(
    token: AdminToken,
    fleet: Fleet,
    request: Request<B>,
) -> Result<Response<ResponseBody>, Infallible>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let (parts, body) = request.into_parts();
    let result = match parts.uri.path() {
        "/health" => health(&parts.method),
        "/session" => session(&token, &parts, body).await,
        _ => match authorize(&token, &parts) {
            Ok(()) => return api::handle(fleet, Request::from_parts(parts, body)).await,
            Err(error) => Err(error),
        },
    };

    Ok(result.unwrap_or_else(ApiError::into_response))
}

fn health(method: &Method) -> Result<Response<ResponseBody>, ApiError> {
    if method != Method::GET {
        return Err(ApiError::MethodNotAllowed("GET"));
    }
    Ok(api::empty_response(StatusCode::NO_CONTENT))
}

async fn session<B>(
    token: &AdminToken,
    parts: &Parts,
    body: B,
) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    match parts.method {
        Method::POST => {
            check_origin(parts)?;
            let request: SignInRequest = api::read_json(&parts.headers, body).await?;
            if !token.matches(&request.token) {
                return Err(ApiError::Unauthorized);
            }
            Ok(cookie_response(
                parts,
                &request.token,
                COOKIE_MAX_AGE_SECONDS,
            ))
        }
        Method::DELETE => {
            check_origin(parts)?;
            Ok(cookie_response(parts, "", 0))
        }
        _ => Err(ApiError::MethodNotAllowed("POST, DELETE")),
    }
}

fn authorize(token: &AdminToken, parts: &Parts) -> Result<(), ApiError> {
    if bearer(&parts.headers).is_some_and(|value| token.matches(value)) {
        return Ok(());
    }
    if !cookies(&parts.headers).any(|value| token.matches(value)) {
        return Err(ApiError::Unauthorized);
    }
    check_origin(parts)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

fn cookies(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|cookie| cookie.trim().strip_prefix(COOKIE_NAME)?.strip_prefix('='))
}

fn check_origin(parts: &Parts) -> Result<(), ApiError> {
    if matches!(parts.method, Method::GET | Method::HEAD) {
        return Ok(());
    }
    let Some(origin) = parts.headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let origin = origin
        .to_str()
        .ok()
        .and_then(|origin| url::Url::parse(origin).ok());
    let host = forwarded(&parts.headers, "x-forwarded-host")
        .or_else(|| parts.headers.get(header::HOST)?.to_str().ok())
        .or_else(|| parts.uri.authority().map(Authority::as_str));
    match (origin, host) {
        (Some(origin), Some(host)) if origin.authority().eq_ignore_ascii_case(host) => Ok(()),
        _ => Err(ApiError::CrossOrigin),
    }
}

fn forwarded<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
}

fn cookie_response(parts: &Parts, value: &str, max_age: u32) -> Response<ResponseBody> {
    let secure = if forwarded(&parts.headers, "x-forwarded-proto") == Some("https") {
        "; Secure"
    } else {
        ""
    };
    let mut response = api::empty_response(StatusCode::NO_CONTENT);
    response.headers_mut().insert(
        header::SET_COOKIE,
        format!(
            "{COOKIE_NAME}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}"
        )
        .parse()
        .expect("cookie is a valid header"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_store::Store;
    use http_body_util::{BodyExt as _, Full};
    use hyper::http::request::Builder;
    use serde_json::{Value, json};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const COOKIE: &str = "other=1; fleet_session=0123456789abcdef0123456789abcdef";

    fn call(request: Builder, body: &str) -> (StatusCode, HeaderMap, Bytes) {
        let token = AdminToken::new(OsStr::new(TOKEN)).unwrap();
        let fleet = Fleet::new(Store::memory().unwrap(), None);
        let request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.to_owned())))
            .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let response = handle(token, fleet, request).await.unwrap();
                let (parts, body) = response.into_parts();
                let bytes = body.collect().await.unwrap().to_bytes();
                (parts.status, parts.headers, bytes)
            })
    }

    fn get(path: &str) -> Builder {
        Request::builder().uri(path)
    }

    fn post(path: &str) -> Builder {
        Request::builder().method("POST").uri(path)
    }

    #[test]
    fn requests_need_a_valid_token() {
        let (status, headers, body) = call(get("/providers"), "");
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "sign_in_required");

        let wrong = get("/providers").header(header::AUTHORIZATION, "Bearer wrong");
        assert_eq!(call(wrong, "").0, StatusCode::UNAUTHORIZED);

        let bearer = get("/providers").header(header::AUTHORIZATION, format!("bearer {TOKEN}"));
        assert_eq!(call(bearer, "").0, StatusCode::OK);

        let cookie = get("/providers").header(header::COOKIE, COOKIE);
        assert_eq!(call(cookie, "").0, StatusCode::OK);
    }

    #[test]
    fn unknown_paths_need_a_token() {
        assert_eq!(call(get("/session/"), "").0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(get("/health/"), "").0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn health_is_public() {
        assert_eq!(call(get("/health"), "").0, StatusCode::NO_CONTENT);
    }

    #[test]
    fn cookie_writes_must_come_from_the_same_origin() {
        let write = || {
            post("/agents")
                .header(header::COOKIE, COOKIE)
                .header(header::HOST, "fleet.test")
        };

        let cross = write().header(header::ORIGIN, "https://other.test");
        assert_eq!(call(cross, "{}").0, StatusCode::FORBIDDEN);

        let same = write().header(header::ORIGIN, "https://fleet.test");
        assert_eq!(call(same, "{}").0, StatusCode::BAD_REQUEST);

        let proxied = write()
            .header(header::ORIGIN, "https://web.test")
            .header("x-forwarded-host", "web.test");
        assert_eq!(call(proxied, "{}").0, StatusCode::BAD_REQUEST);

        let bearer = post("/agents")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::ORIGIN, "https://other.test");
        assert_eq!(call(bearer, "{}").0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn sign_in_sets_a_session_cookie() {
        let body = json!({ "token": TOKEN }).to_string();

        let wrong = json!({ "token": "wrong" }).to_string();
        assert_eq!(call(post("/session"), &wrong).0, StatusCode::UNAUTHORIZED);

        let (status, headers, _) = call(post("/session"), &body);
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            headers[header::SET_COOKIE],
            format!("fleet_session={TOKEN}; Path=/; Max-Age=2592000; HttpOnly; SameSite=Strict")
        );

        let https = post("/session").header("x-forwarded-proto", "https");
        let (_, headers, _) = call(https, &body);
        let cookie = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.ends_with("; Secure"));

        let sign_out = Request::builder().method("DELETE").uri("/session");
        let (status, headers, _) = call(sign_out, "");
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            headers[header::SET_COOKIE],
            "fleet_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict"
        );
    }

    #[test]
    fn tokens_must_be_long_and_header_safe() {
        assert!(AdminToken::new(OsStr::new("short")).is_err());
        assert!(AdminToken::new(OsStr::new(&format!("{TOKEN};"))).is_err());
        assert!(AdminToken::new(OsStr::new(TOKEN)).is_ok());
    }
}
