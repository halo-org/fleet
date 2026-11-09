use super::api;
use crate::{Credentials, Http, OpenAiError};
use bytes::Bytes;
use core::pin::Pin;
use fleet_core::ModelsErrorKind;
use futures_util::{Stream, StreamExt as _, TryStreamExt as _};
use http::{Request, Response, StatusCode, Uri, header};
use serde::{Deserialize, Serialize};

pub type Events = Pin<Box<dyn Stream<Item = Result<api::Event, OpenAiError>> + Send>>;

pub struct Client<H, C> {
    http: H,
    credentials: C,
    responses: Uri,
    compactions: Uri,
}

#[derive(Clone, Copy)]
pub struct ErrorScope {
    pub continuation: bool,
    pub compaction: bool,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: api::ApiError,
}

impl<H, C> Client<H, C> {
    pub fn new(http: H, credentials: C, base_url: &str) -> Result<Self, OpenAiError> {
        let base: Uri = base_url
            .parse()
            .map_err(|error| OpenAiError::source("invalid OpenAI base URL", error))?;
        if !matches!(base.scheme_str(), Some("https" | "http"))
            || base.host().is_none()
            || base.query().is_some()
            || base
                .authority()
                .is_some_and(|authority| authority.as_str().contains('@'))
        {
            return Err(OpenAiError::new(
                ModelsErrorKind::Other,
                "OpenAI base URL must be an HTTP URL without credentials or a query",
            ));
        }
        let base_url = base_url.trim_end_matches('/');
        let responses = format!("{base_url}/responses")
            .parse()
            .map_err(|error| OpenAiError::source("invalid OpenAI response URL", error))?;
        let compactions = format!("{base_url}/responses/compact")
            .parse()
            .map_err(|error| OpenAiError::source("invalid OpenAI compaction URL", error))?;

        Ok(Self {
            http,
            credentials,
            responses,
            compactions,
        })
    }
}

impl<H, C> Client<H, C>
where
    H: Http + Sync,
    C: Credentials + Sync,
{
    pub async fn events(
        &self,
        request: &api::Request,
        scope: ErrorScope,
    ) -> Result<Events, OpenAiError> {
        let response = self.post(&self.responses, request, true, scope).await?;
        let request_id = request_id(&response);
        let events = sse_stream::SseStream::from_bytes_stream(response.into_body());
        let events = events
            .map_err(|error| OpenAiError::source("failed to read model response", error))
            .try_filter_map(|event| async move {
                let Some(data) = event
                    .data
                    .filter(|data| !data.is_empty() && data != "[DONE]")
                else {
                    return Ok(None);
                };
                serde_json::from_str::<api::Event>(&data)
                    .map(Some)
                    .map_err(|error| OpenAiError::source("failed to decode model response", error))
            });
        let events = events.map(move |result| {
            result.and_then(|event| match event {
                api::Event::Error { detail, error } => Err(api_error(
                    error.unwrap_or(detail),
                    None,
                    request_id.clone(),
                    scope,
                )),
                api::Event::Failed { response } => Err(api_error(
                    response.error.unwrap_or_default(),
                    None,
                    request_id.clone(),
                    scope,
                )),
                event => Ok(event),
            })
        });
        Ok(Box::pin(events))
    }

    pub async fn compact(
        &self,
        request: &api::CompactRequest,
        scope: ErrorScope,
    ) -> Result<api::CompactResponse, OpenAiError> {
        let response = self.post(&self.compactions, request, false, scope).await?;
        let body = read_body(response.into_body(), usize::MAX).await?;
        serde_json::from_slice(&body)
            .map_err(|error| OpenAiError::source("failed to decode compaction", error))
    }

    async fn post(
        &self,
        url: &Uri,
        body: &(impl Serialize + Sync),
        streaming: bool,
        scope: ErrorScope,
    ) -> Result<Response<H::Body>, OpenAiError> {
        let body = Bytes::from(
            serde_json::to_vec(body)
                .map_err(|error| OpenAiError::source("failed to encode model request", error))?,
        );
        let mut refreshed = false;

        loop {
            let credentials = self.credentials.get().await.map_err(|error| {
                OpenAiError::source("failed to obtain model credentials", error)
            })?;
            let mut request = Request::new(body.clone());
            *request.method_mut() = http::Method::POST;
            *request.uri_mut() = url.clone();
            *request.headers_mut() = credentials.clone();
            request.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            request.headers_mut().insert(
                header::ACCEPT,
                header::HeaderValue::from_static(if streaming {
                    "text/event-stream"
                } else {
                    "application/json"
                }),
            );
            let response = self
                .http
                .send(request)
                .await
                .map_err(|error| OpenAiError::source("failed to send model request", error))?;

            if response.status() == StatusCode::UNAUTHORIZED
                && !refreshed
                && self
                    .credentials
                    .refresh(&credentials)
                    .await
                    .map_err(|error| {
                        OpenAiError::source("failed to refresh model credentials", error)
                    })?
            {
                refreshed = true;
                continue;
            }
            if response.status().is_success() {
                return Ok(response);
            }

            let status = response.status();
            let request_id = request_id(&response);
            let body = read_body(response.into_body(), 65_536).await?;
            let detail = serde_json::from_slice::<ErrorResponse>(&body)
                .map(|response| response.error)
                .unwrap_or_default();
            return Err(api_error(detail, Some(status), request_id, scope));
        }
    }
}

fn api_error(
    detail: api::ApiError,
    status: Option<StatusCode>,
    request_id: Option<String>,
    scope: ErrorScope,
) -> OpenAiError {
    let kind = detail.kind(scope.continuation, scope.compaction);
    OpenAiError::api(status, detail.code, detail.message, request_id, kind)
}

fn request_id<T>(response: &Response<T>) -> Option<String> {
    response
        .headers()
        .get("x-request-id")
        .and_then(|id| id.to_str().ok())
        .map(str::to_owned)
}

async fn read_body<E: core::error::Error + Send + Sync + 'static>(
    body: impl Stream<Item = Result<Bytes, E>>,
    limit: usize,
) -> Result<Vec<u8>, OpenAiError> {
    let mut body = core::pin::pin!(body);
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let mut chunk = chunk
            .map_err(|error| OpenAiError::source("failed to read model response body", error))?;
        chunk.truncate(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk);
        if bytes.len() == limit {
            break;
        }
    }
    Ok(bytes)
}
