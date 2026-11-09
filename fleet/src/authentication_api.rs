use crate::{
    Fleet, FleetError,
    api::{ApiError, ResponseBody},
};
use core::error::Error;
use hyper::{
    Method, Response, StatusCode,
    body::{Body, Bytes},
    header,
    http::request::Parts,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeRequest {
    method: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialRequest {
    method: String,
    key: String,
}

#[derive(Serialize)]
struct SignOutResponse {
    revocation_confirmed: bool,
}

pub async fn route<B>(
    fleet: &Fleet,
    parts: &Parts,
    segments: &[&str],
    body: B,
) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    validate_request(parts)?;
    let authentication = fleet.authentication().ok_or(ApiError::NotFound)?.clone();
    let id = segments
        .get(2)
        .map(|id| percent_encoding::percent_decode_str(id).decode_utf8())
        .transpose()
        .map_err(|_| ApiError::BadRequest("Invalid connection ID".into()))?;
    let id = id.as_deref().unwrap_or_default();
    let response = match (&parts.method, segments) {
        (&Method::GET, ["", "connections"]) => {
            crate::api::json_response(StatusCode::OK, &authentication.connections().await)
        }
        (&Method::POST, ["", "connections", _, "authorizations"]) => {
            let request: AuthorizeRequest = crate::api::read_json(&parts.headers, body).await?;
            let guard = fleet
                .idle_handler()
                .track_mutation()
                .ok_or(ApiError::Fleet(FleetError::ShuttingDown))?;
            let id = id.to_owned();
            let info = crate::authentication::finish(async move {
                let _guard = guard;
                authentication.authorize(&id, &request.method).await
            })
            .await
            .map_err(ApiError::Authentication)?;
            let mut response = crate::api::json_response(StatusCode::CREATED, &info);
            response.headers_mut().insert(
                header::LOCATION,
                format!("{}/{}", parts.uri.path(), info.id)
                    .parse()
                    .expect("authorization location is a valid header"),
            );
            response
        }
        (&Method::GET, ["", "connections", _, "authorizations", authorization]) => {
            let info = authentication
                .authorization(id, authorization)
                .await
                .map_err(ApiError::Authentication)?;
            crate::api::json_response(StatusCode::OK, &info)
        }
        (&Method::PUT, ["", "connections", _, "credential"]) => {
            let request: CredentialRequest = crate::api::read_json(&parts.headers, body).await?;
            let guard = fleet
                .idle_handler()
                .track_mutation()
                .ok_or(ApiError::Fleet(FleetError::ShuttingDown))?;
            let id = id.to_owned();
            crate::authentication::finish(async move {
                let _guard = guard;
                authentication
                    .set_key(&id, &request.method, request.key)
                    .await
            })
            .await
            .map_err(ApiError::Authentication)?;
            crate::api::empty_response(StatusCode::NO_CONTENT)
        }
        (&Method::DELETE, ["", "connections", _, "credential"]) => {
            let guard = fleet
                .idle_handler()
                .track_mutation()
                .ok_or(ApiError::Fleet(FleetError::ShuttingDown))?;
            let id = id.to_owned();
            let revocation_confirmed = crate::authentication::finish(async move {
                let _guard = guard;
                authentication.sign_out(&id).await
            })
            .await
            .map_err(ApiError::Authentication)?;
            crate::api::json_response(
                StatusCode::OK,
                &SignOutResponse {
                    revocation_confirmed,
                },
            )
        }
        (_, ["", "connections"] | ["", "connections", _, "authorizations", _]) => {
            return Err(ApiError::MethodNotAllowed("GET"));
        }
        (_, ["", "connections", _, "authorizations"]) => {
            return Err(ApiError::MethodNotAllowed("POST"));
        }
        (_, ["", "connections", _, "credential"]) => {
            return Err(ApiError::MethodNotAllowed("PUT, DELETE"));
        }
        _ => return Err(ApiError::NotFound),
    };
    let mut response = response;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

fn validate_request(parts: &Parts) -> Result<(), ApiError> {
    if parts.uri.query().is_some() {
        return Err(ApiError::BadRequest(
            "Connection APIs do not accept query parameters".into(),
        ));
    }

    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| url::Url::parse(&format!("http://{host}")).ok());
    if !host.is_some_and(|url| matches!(url.host_str(), Some("localhost" | "127.0.0.1"))) {
        return Err(ApiError::BadRequest(
            "Connection APIs require a loopback host".into(),
        ));
    }
    if let Some(origin) = parts.headers.get(header::ORIGIN) {
        let origin = origin
            .to_str()
            .ok()
            .and_then(|origin| url::Url::parse(origin).ok());
        if !origin.is_some_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        }) {
            return Err(ApiError::BadRequest(
                "Cross-origin authentication is not allowed".into(),
            ));
        }
    }
    Ok(())
}

pub const fn error_details(
    error: &crate::AuthenticationError,
) -> (StatusCode, &'static str, &'static str) {
    match error {
        crate::AuthenticationError::NotFound => (
            StatusCode::NOT_FOUND,
            "connection_not_found",
            "Connection or sign-in method not found",
        ),
        crate::AuthenticationError::Invalid(message) => {
            (StatusCode::BAD_REQUEST, "invalid_authentication", *message)
        }
        crate::AuthenticationError::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            "authorization_required",
            "Connection needs authorization",
        ),
        crate::AuthenticationError::Cancelled => (
            StatusCode::CONFLICT,
            "authorization_cancelled",
            "Sign-in expired or was cancelled",
        ),
        crate::AuthenticationError::Callback(_) => (
            StatusCode::CONFLICT,
            "callback_unavailable",
            "Could not open the local sign-in callback. Close any other sign-in attempt and retry.",
        ),
        crate::AuthenticationError::Rejected(_) | crate::AuthenticationError::Other(_, _) => (
            StatusCode::BAD_GATEWAY,
            "authentication_failed",
            "Authentication could not be completed",
        ),
    }
}
