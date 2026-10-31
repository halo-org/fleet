use crate::{Fleet, FleetError};
use core::{convert::Infallible, error::Error};
use fleet_core::{
    Agent, AgentId, AgentParent, ClientStoreErrorKind, Configured, ContentBlock, Error as _,
    JsonObject, JsonValue, Message, MessageId, MessageMode,
};
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited, combinators::UnsyncBoxBody};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode,
    body::{Body, Bytes},
    header::{self, HeaderValue},
};
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::json;

const MAX_BODY_BYTES: usize = 1_048_576;

type ResponseBody = UnsyncBoxBody<Bytes, Box<dyn Error + Send + Sync>>;

// === Requests ===

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateAgentRequest {
    #[serde(deserialize_with = "deserialize_id")]
    id: AgentId,
    role: String,
    #[serde(default)]
    parameters: JsonObject,
    message: Option<MessageInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InterruptAgentRequest {
    message: Option<MessageInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageInput {
    #[serde(deserialize_with = "deserialize_id")]
    id: MessageId,
    #[serde(with = "MessageModeInput")]
    mode: MessageMode,
    content: Vec<ContentInput>,
}

#[derive(Deserialize)]
#[serde(remote = "MessageMode", rename_all = "snake_case")]
enum MessageModeInput {
    Steer,
    Queue,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ContentInput {
    Text { text: String },
}

impl From<MessageInput> for Message {
    fn from(input: MessageInput) -> Self {
        Self {
            id: input.id,
            mode: input.mode,
            content: input
                .content
                .into_iter()
                .map(|content| match content {
                    ContentInput::Text { text } => ContentBlock::Text(text),
                })
                .collect(),
        }
    }
}

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
        (_, ["", "agents"] | ["", "agents", _, "messages" | "interrupts"]) => {
            Err(ApiError::MethodNotAllowed)
        }
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
    let agent = Agent {
        id: request.id,
        parent: AgentParent::User,
        role: Configured {
            name: request.role.into(),
            config: request.parameters,
        },
    };

    fleet
        .create_agent(idempotency_key, agent, request.message.map(Message::from))
        .await
        .map_err(ApiError::Fleet)?;

    Ok(created_response(&format!("/agents/{}", request.id)))
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
    let request: MessageInput = read_json(headers, body).await?;
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
        .interrupt_agent(
            idempotency_key,
            agent_id,
            request.message.map(Message::from),
        )
        .await
        .map_err(ApiError::Fleet)?;

    Ok(empty_response(StatusCode::ACCEPTED))
}

// === Request parsing ===

fn parse_id<T: From<u128>>(value: &str) -> Result<T, ApiError> {
    value.parse::<u128>().map(T::from).map_err(|_| {
        ApiError::BadRequest(
            "ID must be a decimal string containing a 128-bit unsigned integer".into(),
        )
    })
}

fn deserialize_id<'de, D: Deserializer<'de>, T: From<u128>>(
    deserializer: D,
) -> Result<T, D::Error> {
    let value = String::deserialize(deserializer)?;
    value
        .parse::<u128>()
        .map(T::from)
        .map_err(serde::de::Error::custom)
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

fn json_response(status: StatusCode, value: &JsonValue) -> Response<ResponseBody> {
    let body = Full::new(Bytes::from(value.to_string()))
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
    MethodNotAllowed,
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
            Self::MethodNotAllowed => (
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
                ClientStoreErrorKind::MessageNotFound => (
                    StatusCode::NOT_FOUND,
                    "message_not_found",
                    "Message not found",
                ),
                ClientStoreErrorKind::Other => {
                    eprintln!("HTTP request failed: {error}");
                    let mut source = error.source();

                    while let Some(error) = source {
                        eprintln!("Caused by: {error}");
                        source = error.source();
                    }

                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        "Internal server error",
                    )
                }
            },
        };
        let mut response = json_response(
            status,
            &json!({
                "error": {
                    "code": code,
                    "message": message,
                },
            }),
        );

        if matches!(self, Self::MethodNotAllowed) {
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST"));
        }

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use http_body_util::StreamBody;
    use hyper::body::Frame;

    fn request(path: &str, idempotency_key: &str, body: &JsonValue) -> Request<Full<Bytes>> {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", idempotency_key)
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap()
    }

    #[test]
    fn handle_applies_mutations_and_replays_requests() {
        let fleet = Fleet::new(None);
        let agent = json!({
            "id": "1",
            "role": "assistant",
            "parameters": {
                "name": "Alice",
            },
        });
        let message = json!({
            "id": "2",
            "mode": "queue",
            "content": [
                {
                    "type": "text",
                    "text": "Hello",
                },
            ],
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async {
            let response = handle(fleet.clone(), request("/agents", "create", &agent))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(response.headers()[header::LOCATION], "/agents/1");

            let response = handle(fleet.clone(), request("/agents", "create", &agent))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);

            let response = handle(
                fleet.clone(),
                request("/agents/1/messages", "send", &message),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(response.headers()[header::LOCATION], "/agents/1/messages/2");

            let response = handle(
                fleet,
                request("/agents/1/interrupts", "interrupt", &json!({})),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        });
    }

    #[test]
    fn handle_reports_idempotency_conflicts() {
        let fleet = Fleet::new(None);
        let mut agent = json!({
            "id": "340282366920938463463374607431768211455",
            "role": "assistant",
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async {
            let response = handle(fleet.clone(), request("/agents", "create", &agent))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(
                response.headers()[header::LOCATION],
                "/agents/340282366920938463463374607431768211455",
            );

            agent["role"] = "another-role".into();
            let response = handle(fleet, request("/agents", "create", &agent))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: JsonValue = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "idempotency_conflict");
        });
    }

    #[test]
    fn handle_distinguishes_routes_and_methods() {
        let fleet = Fleet::new(None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async {
            for path in ["/agents", "/agents/1/messages", "/agents/1/interrupts"] {
                let request = Request::builder().uri(path).body(Full::default()).unwrap();

                let response = handle(fleet.clone(), request).await.unwrap();
                assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
                assert_eq!(response.headers()[header::ALLOW], "POST");
            }

            for path in ["/missing", "/agents/", "/agents/1/messages/extra"] {
                let response = handle(fleet.clone(), request(path, "key", &json!({})))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
            }
        });
    }

    #[test]
    fn handle_rejects_invalid_requests() {
        let fleet = Fleet::new(None);
        let agent = json!({
            "id": "1",
            "role": "assistant",
        });
        let mut missing_key = request("/agents", "create", &agent);
        missing_key.headers_mut().remove("idempotency-key");
        let mut wrong_content_type = request("/agents", "create", &agent);
        wrong_content_type
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        let malformed_json =
            request("/agents", "create", &agent).map(|_| Full::new(Bytes::from_static(b"{")));
        let missing_mode = request(
            "/agents/1/messages",
            "send",
            &json!({
                "id": "2",
                "content": [],
            }),
        );
        let invalid_id = request("/agents/invalid/messages", "send", &json!({}));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async {
            for (request, status) in [
                (missing_key, StatusCode::BAD_REQUEST),
                (wrong_content_type, StatusCode::UNSUPPORTED_MEDIA_TYPE),
                (malformed_json, StatusCode::BAD_REQUEST),
                (missing_mode, StatusCode::BAD_REQUEST),
                (invalid_id, StatusCode::BAD_REQUEST),
            ] {
                let response = handle(fleet.clone(), request).await.unwrap();
                assert_eq!(response.status(), status);
                assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
            }
        });
    }

    #[test]
    fn handle_limits_bodies_without_content_length() {
        let fleet = Fleet::new(None);
        let body = StreamBody::new(stream::iter([
            Ok::<_, Infallible>(Frame::data(Bytes::from(vec![b' '; MAX_BODY_BYTES]))),
            Ok(Frame::data(Bytes::from_static(b" "))),
        ]));
        let request = request("/agents", "create", &json!({})).map(|_| body);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async {
            let response = handle(fleet, request).await.unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        });
    }

    #[test]
    fn handle_reports_missing_agents_and_shutdown() {
        let fleet = Fleet::new(None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async {
            let response = handle(
                fleet.clone(),
                request("/agents/1/interrupts", "interrupt", &json!({})),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);

            fleet.shutdown();
            let response = handle(
                fleet,
                request("/agents/1/interrupts", "interrupt", &json!({})),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        });
    }
}
