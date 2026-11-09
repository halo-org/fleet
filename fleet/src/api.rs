use crate::api_types::{AgentsReadQuery, ApiAgentsPage, SetInboxRequest};
use crate::{
    ApiAgent, ApiAgentActivity, ApiEvent, ApiMessage, ApiMessageRecord, ApiProvider, ApiRole,
    ApiToolCallRecord, CreateAgentRequest, ErrorDetails, ErrorResponse, EventsQuery, Fleet,
    FleetError, InterruptAgentRequest, SinceQuery,
};
use core::{convert::Infallible, error::Error, fmt, str::FromStr};
use fleet_core::{AgentId, ClientStoreErrorKind, Error as _};
use fleet_kit::{QueriesErrorKind, QueryUpdate};
use futures_util::Stream;
use http_body_util::{
    BodyExt as _, Full, LengthLimitError, Limited, StreamBody, combinators::UnsyncBoxBody,
};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode,
    body::{Body, Bytes, Frame},
    header::{self, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};

const MAX_BODY_BYTES: usize = 1_048_576;

pub type ResponseBody = UnsyncBoxBody<Bytes, Box<dyn Error + Send + Sync>>;

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

    if segments.get(1) == Some(&"connections") {
        return crate::authentication_api::route(fleet, &parts, &segments, body).await;
    }

    match (&parts.method, segments.as_slice()) {
        (&Method::GET, ["", "providers"]) => Ok(providers(fleet)),
        (&Method::GET, ["", "roles"]) => roles(fleet).await,
        (&Method::POST, ["", "agents"]) => create_agent(fleet, &parts.headers, body).await,
        (&Method::GET, ["", "agents"]) => {
            let query: AgentsReadQuery = parse_query(parts.uri.query())?;
            let page = fleet.agents(query.into()).await.map_err(ApiError::Fleet)?;
            Ok(read_response(
                page.revision,
                json_response(StatusCode::OK, &ApiAgentsPage::from(page)),
            ))
        }
        (&Method::GET, ["", "agents", "subscription"]) => {
            if parts.uri.query().is_some_and(|query| !query.is_empty()) {
                return Err(ApiError::BadRequest(
                    "Subscription does not accept query parameters".into(),
                ));
            }
            agents_subscription(fleet, &parts.headers).await
        }
        (_, ["", "agents", "subscription"]) => Err(ApiError::MethodNotAllowed("GET")),
        (&Method::PUT, ["", "agents", agent_id, "inbox"]) => {
            set_inbox(fleet, parse_id(agent_id)?, &parts.headers, body).await
        }
        (_, ["", "agents", _, "inbox"]) => Err(ApiError::MethodNotAllowed("PUT")),
        (&Method::POST, ["", "agents", agent_id, "messages"]) => {
            send_message(fleet, parse_id(agent_id)?, &parts.headers, body).await
        }
        (&Method::POST, ["", "agents", agent_id, "interrupts"]) => {
            interrupt_agent(fleet, parse_id(agent_id)?, &parts.headers, body).await
        }
        (&Method::GET, ["", "agents", agent_id]) => {
            agent(fleet, parse_id(agent_id)?, parse_query(parts.uri.query())?).await
        }
        (&Method::GET, ["", "agents", agent_id, "messages"]) => {
            messages(fleet, parse_id(agent_id)?, parse_query(parts.uri.query())?).await
        }
        (&Method::GET, ["", "agents", agent_id, "activity"]) => {
            activity(fleet, parse_id(agent_id)?, parse_query(parts.uri.query())?).await
        }
        (&Method::GET, ["", "agents", agent_id, "tool-calls"]) => {
            tool_calls(fleet, parse_id(agent_id)?, parse_query(parts.uri.query())?).await
        }
        (&Method::GET, ["", "agents", agent_id, "preview", "subscription"]) => {
            if parts.uri.query().is_some_and(|query| !query.is_empty()) {
                return Err(ApiError::BadRequest(
                    "Subscription does not accept query parameters".into(),
                ));
            }
            preview_subscription(fleet, parse_id(agent_id)?, &parts.headers).await
        }
        (&Method::GET, ["", "agents", agent_id, "subscription"]) => {
            if parts.uri.query().is_some_and(|query| !query.is_empty()) {
                return Err(ApiError::BadRequest(
                    "Subscription does not accept query parameters".into(),
                ));
            }
            subscription(fleet, parse_id(agent_id)?, &parts.headers).await
        }
        (&Method::GET, ["", "agents", agent_id, "events"]) => {
            events(fleet, parse_id(agent_id)?, parse_query(parts.uri.query())?).await
        }
        (_, ["", "agents"] | ["", "agents", _, "messages"]) => {
            Err(ApiError::MethodNotAllowed("GET, POST"))
        }
        (_, ["", "agents", _, "interrupts"]) => Err(ApiError::MethodNotAllowed("POST")),
        (
            _,
            ["", "agents", _]
            | ["", "providers" | "roles"]
            | [
                "",
                "agents",
                _,
                "events" | "activity" | "tool-calls" | "subscription",
            ]
            | ["", "agents", _, "preview", "subscription"],
        ) => Err(ApiError::MethodNotAllowed("GET")),
        _ => Err(ApiError::NotFound),
    }
}

// === Handlers ===

fn providers(fleet: &Fleet) -> Response<ResponseBody> {
    let providers: Vec<_> = fleet
        .providers()
        .into_iter()
        .map(|(name, plugin)| ApiProvider { name, plugin })
        .collect();
    let mut response = json_response(StatusCode::OK, &providers);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn roles(fleet: &Fleet) -> Result<Response<ResponseBody>, ApiError> {
    let roles: Vec<_> = fleet
        .roles()
        .await
        .map_err(ApiError::Fleet)?
        .into_iter()
        .map(ApiRole::try_from)
        .collect::<Result<_, _>>()
        .map_err(|error| {
            ApiError::Fleet(FleetError::Role(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )))
        })?;
    let mut response = json_response(StatusCode::OK, &roles);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn agents_subscription(
    fleet: &Fleet,
    headers: &HeaderMap,
) -> Result<Response<ResponseBody>, ApiError> {
    let mut values = headers.get_all("last-event-id").iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ApiError::BadRequest("Last-Event-ID from a collection read is required".into())
        })?;
    if values.next().is_some() {
        return Err(ApiError::BadRequest("Last-Event-ID must occur once".into()));
    }
    let after = fleet_kit::CollectionRevision(parse_id::<u128>(value)?);
    if after > fleet.collection_revision().await.map_err(ApiError::Fleet)? {
        return Err(ApiError::Fleet(FleetError::Query(
            fleet_store::StoreError::InvalidRevision,
        )));
    }
    Ok(subscription_response(crate::subscription::agents(
        fleet.clone(),
        after,
    )))
}

async fn set_inbox<B>(
    fleet: &Fleet,
    agent_id: AgentId,
    headers: &HeaderMap,
    body: B,
) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let key = idempotency_key(headers)?;
    let mut values = headers.get_all(header::IF_MATCH).iter();
    let expected = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix('"'))
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| {
            ApiError::BadRequest("If-Match must contain the quoted summary version".into())
        })?;
    if values.next().is_some() {
        return Err(ApiError::BadRequest("If-Match must occur once".into()));
    }
    let expected = fleet_kit::CollectionRevision(parse_id::<u128>(expected)?);
    let request: SetInboxRequest = read_json(headers, body).await?;
    fleet
        .set_inbox(key, agent_id, expected, request.into())
        .await
        .map_err(ApiError::Fleet)?;
    Ok(empty_response(StatusCode::NO_CONTENT))
}

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

async fn agent(
    fleet: &Fleet,
    agent_id: AgentId,
    query: SinceQuery,
) -> Result<Response<ResponseBody>, ApiError> {
    let response = fleet
        .agent(agent_id, query.since.map(Into::into))
        .await
        .map_err(ApiError::Fleet)?;
    let body = match response.value {
        QueryUpdate::Unchanged => empty_response(StatusCode::NO_CONTENT),
        QueryUpdate::Changed(agent) => json_response(StatusCode::OK, &ApiAgent::from(agent)),
    };
    Ok(read_response(response.revision, body))
}

async fn messages(
    fleet: &Fleet,
    agent_id: AgentId,
    query: SinceQuery,
) -> Result<Response<ResponseBody>, ApiError> {
    let response = fleet
        .messages(agent_id, query.since.map(Into::into))
        .await
        .map_err(ApiError::Fleet)?;
    let messages = response
        .value
        .into_iter()
        .map(ApiMessageRecord::from)
        .collect::<Vec<_>>();
    Ok(read_response(
        response.revision,
        json_response(StatusCode::OK, &messages),
    ))
}

async fn activity(
    fleet: &Fleet,
    agent_id: AgentId,
    query: SinceQuery,
) -> Result<Response<ResponseBody>, ApiError> {
    let response = fleet
        .activity(agent_id, query.since.map(Into::into))
        .await
        .map_err(ApiError::Fleet)?;
    let body = match response.value {
        QueryUpdate::Unchanged => empty_response(StatusCode::NO_CONTENT),
        QueryUpdate::Changed(activity) => {
            json_response(StatusCode::OK, &ApiAgentActivity::from(activity))
        }
    };
    Ok(read_response(response.revision, body))
}

async fn tool_calls(
    fleet: &Fleet,
    agent_id: AgentId,
    query: SinceQuery,
) -> Result<Response<ResponseBody>, ApiError> {
    let response = fleet
        .tool_calls(agent_id, query.since.map(Into::into))
        .await
        .map_err(ApiError::Fleet)?;
    let tool_calls = response
        .value
        .into_iter()
        .map(ApiToolCallRecord::from)
        .collect::<Vec<_>>();
    Ok(read_response(
        response.revision,
        json_response(StatusCode::OK, &tool_calls),
    ))
}

async fn events(
    fleet: &Fleet,
    agent_id: AgentId,
    query: EventsQuery,
) -> Result<Response<ResponseBody>, ApiError> {
    let response = fleet
        .events(agent_id, query.into())
        .await
        .map_err(ApiError::Fleet)?;
    let events = response
        .value
        .into_iter()
        .map(ApiEvent::from)
        .collect::<Vec<_>>();
    Ok(read_response(
        response.revision,
        json_response(StatusCode::OK, &events),
    ))
}

async fn subscription(
    fleet: &Fleet,
    agent_id: AgentId,
    headers: &HeaderMap,
) -> Result<Response<ResponseBody>, ApiError> {
    let mut values = headers.get_all("last-event-id").iter();
    let value = values
        .next()
        .map(HeaderValue::to_str)
        .transpose()
        .map_err(|_| ApiError::BadRequest("Last-Event-ID must be a decimal string".into()))?
        .unwrap_or("0");
    if values.next().is_some() {
        return Err(ApiError::BadRequest("Last-Event-ID must occur once".into()));
    }
    let after = parse_id(value)?;
    fleet
        .agent(agent_id, Some(after))
        .await
        .map_err(ApiError::Fleet)?;

    let stream = crate::subscription::stream(fleet.clone(), agent_id, after);
    Ok(subscription_response(stream))
}

async fn preview_subscription(
    fleet: &Fleet,
    agent_id: AgentId,
    headers: &HeaderMap,
) -> Result<Response<ResponseBody>, ApiError> {
    if headers.contains_key("last-event-id") {
        return Err(ApiError::BadRequest(
            "Preview subscriptions do not accept Last-Event-ID".into(),
        ));
    }
    fleet.agent(agent_id, None).await.map_err(ApiError::Fleet)?;

    let stream = crate::subscription::previews(fleet.clone(), agent_id);
    Ok(subscription_response(stream))
}

fn subscription_response(
    stream: impl Stream<Item = Result<Frame<Bytes>, FleetError>> + Send + 'static,
) -> Response<ResponseBody> {
    let body = StreamBody::new(stream).map_err(Into::into).boxed_unsync();
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response
}

// === Request parsing ===

fn parse_query<T: FromStr>(query: Option<&str>) -> Result<T, ApiError>
where
    T::Err: fmt::Display,
{
    query
        .unwrap_or_default()
        .parse::<T>()
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

pub async fn read_json<T: DeserializeOwned, B>(headers: &HeaderMap, body: B) -> Result<T, ApiError>
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

fn read_response(
    revision: impl fmt::Display,
    mut response: Response<ResponseBody>,
) -> Response<ResponseBody> {
    response.headers_mut().insert(
        "read-revision",
        revision
            .to_string()
            .parse()
            .expect("revision is a valid header"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn empty_response(status: StatusCode) -> Response<ResponseBody> {
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

pub fn json_response(status: StatusCode, value: &impl Serialize) -> Response<ResponseBody> {
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

pub enum ApiError {
    BadRequest(String),
    NotFound,
    MethodNotAllowed(&'static str),
    UnsupportedMediaType,
    BodyTooLarge,
    Fleet(FleetError),
    Authentication(crate::AuthenticationError),
}

impl ApiError {
    fn into_response(self) -> Response<ResponseBody> {
        let (status, code, message) = match &self {
            Self::Authentication(error) => crate::authentication_api::error_details(error),
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
            Self::Fleet(FleetError::Store(fleet_store::StoreError::PreconditionFailed)) => (
                StatusCode::PRECONDITION_FAILED,
                "precondition_failed",
                "Agent summary changed",
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
                QueriesErrorKind::InvalidRevision => (
                    StatusCode::BAD_REQUEST,
                    "invalid_revision",
                    "Revision is ahead of the stored state",
                ),
                QueriesErrorKind::RevisionExpired => (
                    StatusCode::GONE,
                    "revision_expired",
                    "Changes after the revision are no longer available",
                ),
                QueriesErrorKind::Other => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Internal server error",
                ),
            },
            Self::Fleet(FleetError::Role(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Could not read roles",
            ),
        };

        if status == StatusCode::INTERNAL_SERVER_ERROR {
            self.log();
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

    fn log(&self) {
        if let Self::Fleet(error) = self {
            eprintln!("HTTP request failed: {error}");
            let mut source = error.source();

            while let Some(error) = source {
                eprintln!("Caused by: {error}");
                source = error.source();
            }
        }
    }
}
