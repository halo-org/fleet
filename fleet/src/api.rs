use crate::{
    ApiEvent, ApiMessage, CreateAgentRequest, ErrorDetails, ErrorResponse, EventsQuery, Fleet,
    FleetError, InterruptAgentRequest,
};
use core::{convert::Infallible, error::Error};
use fleet_core::{AgentId, ClientStoreErrorKind, Error as _};
use fleet_kit::QueriesErrorKind;
use futures_util::{StreamExt as _, TryStreamExt as _};
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited, combinators::UnsyncBoxBody};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode,
    body::{Body, Bytes},
    header::{self, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};

const MAX_BODY_BYTES: usize = 1_048_576;

type ResponseBody = UnsyncBoxBody<Bytes, Box<dyn Error + Send + Sync>>;

// === Routing ===

/// Handles an API request.
pub async fn handle<B>(
    fleet: Fleet,
    request: Request<B>,
) -> Result<Response<ResponseBody>, Infallible>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let response = match route(&fleet, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };

    Ok(response)
}

async fn route<B>(fleet: &Fleet, request: Request<B>) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let (parts, body) = request.into_parts();
    let segments: Vec<_> = parts.uri.path().split('/').collect();

    match (&parts.method, segments.as_slice()) {
        (&Method::POST, ["", "agents"]) => create_agent(fleet, &parts.headers, body).await,
        (&Method::POST, ["", "agents", agent_id, "messages"]) => {
            send_message(fleet, parse_id(agent_id)?, &parts.headers, body).await
        }
        (&Method::POST, ["", "agents", agent_id, "interrupts"]) => {
            interrupt_agent(fleet, parse_id(agent_id)?, &parts.headers, body).await
        }
        (&Method::GET, ["", "agents", agent_id, "events"]) => {
            events(
                fleet,
                parse_id(agent_id)?,
                parse_events_query(parts.uri.query())?,
            )
            .await
        }
        (_, ["", "agents"] | ["", "agents", _, "messages" | "interrupts"]) => {
            Err(ApiError::MethodNotAllowed("POST"))
        }
        (_, ["", "agents", _, "events"]) => Err(ApiError::MethodNotAllowed("GET")),
        _ => Err(ApiError::NotFound),
    }
}

// === Handlers ===

async fn create_agent<B>(
    fleet: &Fleet,
    headers: &HeaderMap,
    body: B,
) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let idempotency_key = idempotency_key(headers)?;
    let request: CreateAgentRequest = read_json(headers, body).await?;
    let (agent, message) = request.into_parts();
    let agent_id = agent.id;

    fleet
        .create_agent(idempotency_key, agent, message)
        .await
        .map_err(ApiError::Fleet)?;

    Ok(created_response(&format!("/agents/{agent_id}")))
}

async fn send_message<B>(
    fleet: &Fleet,
    agent_id: AgentId,
    headers: &HeaderMap,
    body: B,
) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let idempotency_key = idempotency_key(headers)?;
    let request: ApiMessage = read_json(headers, body).await?;
    let message_id = request.id;

    fleet
        .send_message(idempotency_key, agent_id, request.into())
        .await
        .map_err(ApiError::Fleet)?;

    Ok(created_response(&format!(
        "/agents/{agent_id}/messages/{message_id}",
    )))
}

async fn interrupt_agent<B>(
    fleet: &Fleet,
    agent_id: AgentId,
    headers: &HeaderMap,
    body: B,
) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let idempotency_key = idempotency_key(headers)?;
    let request: InterruptAgentRequest = read_json(headers, body).await?;

    fleet
        .interrupt_agent(idempotency_key, agent_id, request.message.map(Into::into))
        .await
        .map_err(ApiError::Fleet)?;

    Ok(empty_response(StatusCode::ACCEPTED))
}

async fn events(
    fleet: &Fleet,
    agent_id: AgentId,
    query: EventsQuery,
) -> Result<Response<ResponseBody>, ApiError> {
    let events = fleet
        .events(
            agent_id,
            query.from.map(Into::into),
            query.until.map(Into::into),
            query.direction.into(),
        )
        .take(query.limit)
        .map_ok(ApiEvent::from)
        .try_collect::<Vec<_>>()
        .await
        .map_err(ApiError::Fleet)?;

    Ok(json_response(StatusCode::OK, &events))
}

// === Request parsing ===

fn parse_events_query(query: Option<&str>) -> Result<EventsQuery, ApiError> {
    query
        .unwrap_or_default()
        .parse::<EventsQuery>()
        .map_err(|error| ApiError::BadRequest(error.to_string()))
}

fn parse_id<T: From<u128>>(value: &str) -> Result<T, ApiError> {
    value.parse::<u128>().map(T::from).map_err(|_| {
        ApiError::BadRequest(
            "ID must be a decimal string containing a 128-bit unsigned integer".into(),
        )
    })
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiError::BadRequest("Idempotency-Key is required".into()))?;

    if values.next().is_some() {
        return Err(ApiError::BadRequest(
            "Idempotency-Key must occur once".into(),
        ));
    }

    Ok(value.to_owned())
}

async fn read_json<T: DeserializeOwned, B>(headers: &HeaderMap, body: B) -> Result<T, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next());

    if !content_type.is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        || headers.contains_key(header::CONTENT_ENCODING)
    {
        return Err(ApiError::UnsupportedMediaType);
    }

    let body = Limited::new(body, MAX_BODY_BYTES)
        .collect()
        .await
        .map_err(|error| {
            if error.is::<LengthLimitError>() {
                ApiError::BodyTooLarge
            } else {
                ApiError::BadRequest("Could not read request body".into())
            }
        })?
        .to_bytes();

    serde_json::from_slice(&body).map_err(|error| ApiError::BadRequest(error.to_string()))
}

// === Responses ===

fn empty_response(status: StatusCode) -> Response<ResponseBody> {
    let mut response = Response::new(ResponseBody::default());
    *response.status_mut() = status;
    response
}

fn created_response(location: &str) -> Response<ResponseBody> {
    let mut response = empty_response(StatusCode::CREATED);
    response.headers_mut().insert(
        header::LOCATION,
        location
            .parse()
            .expect("resource location is a valid header"),
    );
    response
}

fn json_response(status: StatusCode, value: &impl Serialize) -> Response<ResponseBody> {
    let bytes = serde_json::to_vec(value).expect("response is serializable");
    let body = Full::new(Bytes::from(bytes))
        .map_err(Into::into)
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

// === Errors ===

enum ApiError {
    BadRequest(String),
    NotFound,
    MethodNotAllowed(&'static str),
    UnsupportedMediaType,
    BodyTooLarge,
    Fleet(FleetError),
}

impl ApiError {
    fn into_response(self) -> Response<ResponseBody> {
        let (status, code, message) = match &self {
            Self::BadRequest(message) => {
                (StatusCode::BAD_REQUEST, "invalid_request", message.as_str())
            }
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "Route not found"),
            Self::MethodNotAllowed(_) => (
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "Method not allowed",
            ),
            Self::UnsupportedMediaType => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "Request body must be unencoded application/json",
            ),
            Self::BodyTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Request body exceeds the size limit",
            ),
            Self::Fleet(FleetError::ShuttingDown) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "Server is shutting down",
            ),
            Self::Fleet(FleetError::Store(error)) => match error.kind() {
                ClientStoreErrorKind::Conflict => (
                    StatusCode::CONFLICT,
                    "conflict",
                    "Request conflicts with the current state",
                ),
                ClientStoreErrorKind::IdempotencyConflict => (
                    StatusCode::CONFLICT,
                    "idempotency_conflict",
                    "Idempotency key was used for a different request",
                ),
                ClientStoreErrorKind::AgentNotFound => {
                    (StatusCode::NOT_FOUND, "agent_not_found", "Agent not found")
                }
                ClientStoreErrorKind::Other => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Internal server error",
                ),
            },
            Self::Fleet(FleetError::Query(error)) => match error.kind() {
                QueriesErrorKind::AgentNotFound => {
                    (StatusCode::NOT_FOUND, "agent_not_found", "Agent not found")
                }
                QueriesErrorKind::MessageNotFound => (
                    StatusCode::NOT_FOUND,
                    "message_not_found",
                    "Message not found",
                ),
                QueriesErrorKind::Other => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Internal server error",
                ),
            },
        };

        if status == StatusCode::INTERNAL_SERVER_ERROR
            && let Self::Fleet(error) = &self
        {
            eprintln!("HTTP request failed: {error}");
            let mut source = error.source();

            while let Some(error) = source {
                eprintln!("Caused by: {error}");
                source = error.source();
            }
        }

        let mut response = json_response(
            status,
            &ErrorResponse {
                error: ErrorDetails { code, message },
            },
        );

        if let Self::MethodNotAllowed(method) = self {
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(method));
        }

        response
    }
}
