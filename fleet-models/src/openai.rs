mod api;
mod client;
mod compaction;
mod decoder;
mod images;
mod input;

use self::{
    client::{Client, ErrorScope, Events},
    decoder::{Decoder, Step},
};
use crate::{Credentials, Http, OpenAiError};
use fleet_core::{
    Model, ModelCompactionResponse, ModelContext, ModelContinuation, ModelInput, ModelName,
    ModelProvenance, ModelResponseChunk, Models, ModelsErrorKind,
};
use fleet_kit::{Blobs, NoBlobs};
use futures_util::{Stream, StreamExt as _, future::Either, stream};
use serde::Deserialize as _;

const MAX_CONTINUATIONS: u8 = 8;

/// Configuration for an [`OpenAiModels`] provider.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// The API base URL, including its path prefix.
    pub base_url: String,
    /// The identity used to recognize this provider's saved state.
    ///
    /// Use a distinct ID for endpoints or accounts that cannot share saved state.
    pub provider_id: String,
    /// The models available through this provider.
    pub models: Vec<Model>,
    /// Whether to store responses and return continuation tokens.
    pub store: bool,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            provider_id: "openai".into(),
            models: Vec::new(),
            store: false,
        }
    }
}

/// A model provider backed by the [OpenAI Responses API](https://developers.openai.com/api/docs/guides/text).
///
/// Model config accepts optional `effort` and `speed` strings. These select the
/// reasoning effort and service tier. Missing or null values leave them unset.
/// The optional `tools` array accepts `web_search` tool definitions.
/// Missing, null or empty `tools` disables provider tools.
///
/// Commentary and token limit continuations are handled within each response.
/// Compaction retains provider state for later requests. If standalone compaction is not
/// authorized, a streaming request stops at the first completed checkpoint. This fallback
/// uses the provider's default compaction instructions. Its token usage is unavailable
/// and is reported as zero.
pub struct OpenAiModels<H, C, B = NoBlobs> {
    blobs: B,
    client: Client<H, C>,
    config: OpenAiConfig,
}

struct ResponseStream<'a> {
    request: api::Request,
    scope: ErrorScope,
    decoder: Decoder<'a>,
    events: Option<Events>,
    input_len: usize,
    continuations: u8,
    done: bool,
    prepared: bool,
}

impl<H, C> OpenAiModels<H, C> {
    /// Creates a model provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the base URL is invalid.
    pub fn new(http: H, credentials: C, config: OpenAiConfig) -> Result<Self, OpenAiError> {
        let client = Client::new(http, credentials, &config.base_url)?;
        Ok(Self {
            client,
            config,
            blobs: NoBlobs,
        })
    }
}

impl<H, C, B> OpenAiModels<H, C, B> {
    /// Supplies access to immutable image blobs.
    #[must_use]
    pub fn with_blobs<N>(self, blobs: N) -> OpenAiModels<H, C, N> {
        OpenAiModels {
            client: self.client,
            config: self.config,
            blobs,
        }
    }

    fn provenance(&self, context: &ModelContext) -> ModelProvenance {
        ModelProvenance {
            provider_id: self.config.provider_id.clone(),
            model_id: context.model.name.to_string(),
        }
    }

    fn continuation(
        &self,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
    ) -> Result<Option<String>, OpenAiError> {
        continuation
            .map(|continuation| {
                if !self.config.store
                    || continuation.provenance != self.provenance(context)
                    || continuation.token.is_empty()
                {
                    return Err(OpenAiError::new(
                        ModelsErrorKind::InvalidContinuation,
                        "continuation is incompatible with this model",
                    ));
                }
                Ok(continuation.token.clone())
            })
            .transpose()
    }

    fn request(
        &self,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
    ) -> Result<api::Request, OpenAiError> {
        let config = api::Config::deserialize(&context.model.config)
            .map_err(|error| OpenAiError::source("invalid OpenAI model config", error))?;
        let previous_response_id = self.continuation(context, continuation)?;
        let input = input::encode(input, &self.provenance(context))?;
        let tools = context
            .tools
            .iter()
            .map(|tool| api::FunctionTool {
                r#type: "function",
                name: tool.name.to_string(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone().into(),
                strict: false,
            })
            .map(api::Tool::Function)
            .chain(config.tools.into_iter().flatten().map(api::Tool::Provider))
            .collect();

        Ok(api::Request {
            model: context.model.name.to_string(),
            instructions: (!context.system_prompt.is_empty())
                .then(|| context.system_prompt.clone()),
            input,
            tools,
            reasoning: config.effort.map(|effort| api::Reasoning { effort }),
            service_tier: config.speed,
            previous_response_id,
            store: self.config.store,
            stream: true,
            include: ["reasoning.encrypted_content"],
        })
    }
}

impl<H, C, B> Models for OpenAiModels<H, C, B>
where
    B: Blobs + Sync,
    B::Error: Send + Sync,
    H: Http + Sync,
    C: Credentials + Sync,
{
    type Error = OpenAiError;

    fn get(
        &self,
        name: &ModelName,
    ) -> impl Future<Output = Result<Option<Model>, Self::Error>> + Send {
        core::future::ready(Ok(self
            .config
            .models
            .iter()
            .find(|model| model.name == *name)
            .cloned()))
    }

    fn generate(
        &self,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
    ) -> impl Stream<Item = Result<ModelResponseChunk, Self::Error>> + Send {
        let request = match self.request(context, continuation, input) {
            Ok(request) => request,
            Err(error) => return Either::Right(stream::iter([Err(error)])),
        };
        let state = ResponseStream {
            scope: ErrorScope {
                continuation: continuation.is_some(),
                compaction: input
                    .iter()
                    .any(|input| matches!(input, ModelInput::Compaction(_, _))),
            },
            decoder: Decoder::new(&context.tools, self.provenance(context), self.config.store),
            events: None,
            input_len: request.input.len(),
            request,
            continuations: 0,
            done: false,
            prepared: false,
        };

        Either::Left(stream::unfold(state, move |mut state| async move {
            if state.done {
                return None;
            }
            if !state.prepared {
                if let Err(error) = images::resolve(&mut state.request.input, &self.blobs).await {
                    state.done = true;
                    return Some((Err(error), state));
                }
                state.prepared = true;
            }
            let result = state.next(&self.client).await;
            state.done = matches!(result, Err(_) | Ok(ModelResponseChunk::ResponseEnd(_)));
            Some((result, state))
        }))
    }

    async fn compact(
        &self,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
        instructions: Option<&str>,
    ) -> Result<ModelCompactionResponse, Self::Error> {
        let request = self.request(context, continuation, input)?;
        let scope = ErrorScope {
            continuation: continuation.is_some(),
            compaction: input
                .iter()
                .any(|input| matches!(input, ModelInput::Compaction(_, _))),
        };
        compaction::compact(
            &self.client,
            &self.blobs,
            request,
            self.provenance(context),
            instructions,
            scope,
        )
        .await
    }
}

impl ResponseStream<'_> {
    async fn next<H: Http + Sync, C: Credentials + Sync>(
        &mut self,
        client: &Client<H, C>,
    ) -> Result<ModelResponseChunk, OpenAiError> {
        loop {
            let events = match &mut self.events {
                Some(events) => events,
                slot @ None => slot.insert(client.events(&self.request, self.scope).await?),
            };
            let event = events.next().await.ok_or_else(|| {
                OpenAiError::new(
                    ModelsErrorKind::Other,
                    "model response ended before completion",
                )
            })??;
            match self.decoder.decode(event)? {
                Step::Chunk(chunk) => return Ok(chunk),
                Step::Skip => {}
                Step::Continue => {
                    if self.continuations == MAX_CONTINUATIONS {
                        return Err(OpenAiError::new(
                            ModelsErrorKind::Other,
                            "model response exceeded its continuation limit",
                        ));
                    }
                    self.continuations = self.continuations.saturating_add(1);
                    self.request.input.truncate(self.input_len);
                    self.request.input.extend(self.decoder.continuation()?);
                    self.events = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenAiConfig, OpenAiModels};
    use crate::{ApiKey, Credentials, Http};
    use bytes::Bytes;
    use core::{
        assert_matches,
        convert::Infallible,
        fmt::Write as _,
        future::Future,
        sync::atomic::{AtomicBool, Ordering},
    };
    use fleet_core::{
        Configured, ContentBlock, Error as _, JsonObject, Model, ModelCompaction, ModelContext,
        ModelContinuation, ModelInput, ModelOutput, ModelProvenance, ModelProviderToolCall,
        ModelResponse, ModelResponseChunk, ModelTool, ModelUsage, Models as _, ModelsErrorKind,
        ToolCall,
    };
    use futures_util::{StreamExt as _, TryStreamExt as _, stream};
    use http::{HeaderMap, Request, Response, StatusCode, header};
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    type Body = stream::Iter<std::vec::IntoIter<Result<Bytes, Infallible>>>;

    #[derive(Clone, Default)]
    struct TestHttp(Arc<Mutex<HttpState>>);

    #[derive(Default)]
    struct HttpState {
        responses: VecDeque<Response<Body>>,
        requests: Vec<Request<Bytes>>,
    }

    impl TestHttp {
        fn new(responses: impl IntoIterator<Item = Response<Body>>) -> Self {
            Self(Arc::new(Mutex::new(HttpState {
                responses: responses.into_iter().collect(),
                requests: Vec::new(),
            })))
        }

        fn requests(&self) -> Vec<Request<Bytes>> {
            self.0.lock().unwrap().requests.clone()
        }
    }

    impl Http for TestHttp {
        type Error = Infallible;
        type Body = Body;

        fn send(
            &self,
            request: Request<Bytes>,
        ) -> impl Future<Output = Result<Response<Self::Body>, Self::Error>> + Send {
            let mut state = self.0.lock().unwrap();
            state.requests.push(request);
            core::future::ready(Ok(state.responses.pop_front().unwrap()))
        }
    }

    fn context() -> ModelContext {
        ModelContext {
            model: Configured {
                name: "test-model".into(),
                config: JsonObject::new(),
            },
            system_prompt: "Be helpful.".into(),
            tools: Vec::new(),
            role_extensions: JsonObject::new(),
        }
    }

    fn models(http: TestHttp) -> OpenAiModels<TestHttp, ApiKey> {
        OpenAiModels::new(
            http,
            ApiKey::new("test-key").unwrap(),
            OpenAiConfig::default(),
        )
        .unwrap()
    }

    fn message(text: &str, phase: Option<&str>) -> Value {
        json!({
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "phase": phase,
            "content": [{
                "type": "output_text",
                "text": text,
            }],
        })
    }

    fn response(output: &[Value], incomplete: bool) -> Response<Body> {
        let mut events = Vec::new();
        for (index, item) in output.iter().enumerate() {
            events.push(json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": item,
            }));
            if item["type"] == "message" {
                events.push(json!({
                    "type": "response.output_text.delta",
                    "output_index": index,
                    "delta": item["content"][0]["text"],
                }));
            }
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            }));
        }
        events.push(json!({
            "type": if incomplete { "response.incomplete" } else { "response.completed" },
            "response": {
                "id": "resp_test",
                "output": output,
                "incomplete_details": {
                    "reason": "max_output_tokens",
                },
                "usage": {
                    "input_tokens": 8,
                    "output_tokens": 3,
                    "input_tokens_details": {
                        "cached_tokens": 4,
                    },
                },
            },
        }));
        sse(&events)
    }

    fn sse(events: &[Value]) -> Response<Body> {
        let mut text = String::from(": keepalive\r\n\r\n");
        for event in events {
            write!(text, "data: {event}\r\n\r\n").unwrap();
        }
        let chunks = text
            .as_bytes()
            .chunks(7)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(stream::iter(chunks))
            .unwrap()
    }

    fn json_response(status: StatusCode, body: &Value) -> Response<Body> {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-request-id", "request-test")
            .body(stream::iter(vec![Ok(Bytes::from(body.to_string()))]))
            .unwrap()
    }

    fn rejected_compaction() -> Response<Body> {
        json_response(
            StatusCode::UNAUTHORIZED,
            &json!({
                "error": {
                    "code": "hardened_oauth_rule_missing",
                    "message": "This ChatPass credential is not authorized for the requested operation.",
                },
            }),
        )
    }

    #[test]
    fn get_returns_configured_metadata() {
        let model = Model {
            name: "test-model".into(),
            context_window: 100_000,
            compaction_threshold: 80_000,
            extensions: JsonObject::new(),
        };
        let config = OpenAiConfig {
            models: vec![model.clone()],
            ..Default::default()
        };
        let models = OpenAiModels::new(TestHttp::default(), HeaderMap::new(), config).unwrap();

        let loaded = futures_executor::block_on(models.get(&model.name))
            .unwrap()
            .unwrap();
        assert_eq!(loaded, model);

        let missing = futures_executor::block_on(models.get(&"missing".into())).unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn request_leaves_missing_and_null_options_unset() {
        let models = models(TestHttp::default());
        let mut context = context();

        for config in [
            json!({}),
            json!({
                "effort": null,
                "speed": null,
                "tools": null,
            }),
            json!({
                "tools": [],
            }),
        ] {
            context.model.config = serde_json::from_value(config).unwrap();
            let request = models.request(&context, None, &[]).unwrap();
            let request = serde_json::to_value(request).unwrap();
            assert_eq!(request.get("reasoning"), None);
            assert_eq!(request.get("service_tier"), None);
            assert_eq!(request.get("tools"), None);
        }

        context.model.config = serde_json::from_value(json!({
            "effort": "high",
            "speed": "priority",
        }))
        .unwrap();
        let request = models.request(&context, None, &[]).unwrap();
        let request = serde_json::to_value(request).unwrap();
        assert_eq!(
            request["reasoning"],
            json!({
                "effort": "high",
            })
        );
        assert_eq!(request["service_tier"], "priority");
    }

    #[test]
    fn request_combines_function_and_provider_tools() {
        let models = models(TestHttp::default());
        let mut context = context();
        context.tools.push(ModelTool {
            name: "read".into(),
            description: None,
            is_async: false,
            input_schema: JsonObject::new(),
        });
        let tools = json!([{
            "type": "web_search",
            "search_context_size": "low",
            "filters": {
                "allowed_domains": ["example.com"],
            },
        }]);
        context.model.config.insert("tools".into(), tools.clone());

        let request = models.request(&context, None, &[]).unwrap();
        let request = serde_json::to_value(request).unwrap();
        assert_eq!(request["tools"].as_array().unwrap().len(), 2);
        assert_eq!(request["tools"][0]["type"], "function");
        assert_eq!(request["tools"][0]["name"], "read");
        assert_eq!(request["tools"][1], tools[0]);
    }

    #[test]
    fn generate_rejects_unsupported_provider_tools() {
        let models = models(TestHttp::default());
        let mut context = context();
        context.model.config.insert(
            "tools".into(),
            json!([{
                "type": "computer",
            }]),
        );

        let error = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::Other);
    }

    #[test]
    fn generate_continues_commentary_and_preserves_output_order() {
        let commentary = message("Looking…", Some("commentary"));
        let answer = message("Done.", None);
        let http = TestHttp::new([
            response(core::slice::from_ref(&commentary), false),
            response(&[answer], false),
        ]);
        let models = models(http.clone());
        let context = context();
        let input = [ModelInput::User(vec![ContentBlock::Text("Help.".into())])];

        let chunks = futures_executor::block_on(
            models
                .generate(&context, None, &input)
                .try_collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(
            chunks,
            vec![
                ModelResponseChunk::TextStart(0),
                ModelResponseChunk::TextDelta(0, "Looking…".into()),
                ModelResponseChunk::OutputEnd(0, ModelOutput::Text("Looking…".into())),
                ModelResponseChunk::TextStart(1),
                ModelResponseChunk::TextDelta(1, "Done.".into()),
                ModelResponseChunk::OutputEnd(1, ModelOutput::Text("Done.".into())),
                ModelResponseChunk::ResponseEnd(ModelResponse {
                    output: vec![
                        ModelOutput::Text("Looking…".into()),
                        ModelOutput::Text("Done.".into())
                    ],
                    usage: ModelUsage {
                        input: 8,
                        output: 6,
                        cache_read: 4,
                        cache_write: 0
                    },
                    provenance: models.provenance(&context),
                    continuation_token: None,
                }),
            ]
        );

        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].uri(), "https://api.openai.com/v1/responses");
        assert_eq!(
            requests[0].headers()[header::AUTHORIZATION],
            "Bearer test-key"
        );
        let first: Value = serde_json::from_slice(requests[0].body()).unwrap();
        let second: Value = serde_json::from_slice(requests[1].body()).unwrap();
        assert_eq!(first["input"][0], second["input"][0]);
        assert_eq!(second["input"][1], commentary);
        assert_eq!(first["instructions"], second["instructions"]);
        assert_eq!(first["store"], false);
    }

    #[test]
    fn generate_continues_token_limited_responses() {
        let http = TestHttp::new([
            response(&[message("First", None)], true),
            response(&[message("Second", None)], false),
        ]);
        let models = models(http.clone());
        let context = context();

        let chunks = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap();
        let ModelResponseChunk::ResponseEnd(response) = chunks.last().unwrap() else {
            panic!("expected response end");
        };
        assert_eq!(
            response.output,
            vec![
                ModelOutput::Text("First".into()),
                ModelOutput::Text("Second".into())
            ]
        );
        assert_eq!(http.requests().len(), 2);
    }

    #[test]
    fn generate_returns_tool_calls_without_continuing() {
        let tool_call = json!({
            "type": "function_call",
            "call_id": "call-test",
            "name": "search",
            "arguments": "{}",
            "status": "completed",
        });
        let http = TestHttp::new([response(
            &[message("Searching.", Some("commentary")), tool_call],
            true,
        )]);
        let models = models(http.clone());
        let mut context = context();
        context.tools.push(ModelTool {
            name: "search".into(),
            description: None,
            is_async: true,
            input_schema: JsonObject::new(),
        });

        let chunks = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap();
        let ModelResponseChunk::ResponseEnd(response) = chunks.last().unwrap() else {
            panic!("expected response end");
        };
        assert_eq!(
            response.output[1],
            ModelOutput::ToolCall(ToolCall {
                id: "call-test".into(),
                tool: "search".into(),
                is_async: true,
                input: JsonObject::new(),
            })
        );
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn generate_preserves_reasoning_on_replay() {
        let reasoning = json!({
            "type": "reasoning",
            "id": "rs_test",
            "encrypted_content": "opaque-reasoning",
            "summary": [{
                "type": "summary_text",
                "text": "Thinking.",
            }],
        });
        let http = TestHttp::new([response(
            &[reasoning.clone(), message("Done.", None)],
            false,
        )]);
        let models = models(http);
        let context = context();

        let chunks = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap();
        let ModelResponseChunk::ResponseEnd(response) = chunks.into_iter().last().unwrap() else {
            panic!("expected response end");
        };
        let input = [ModelInput::Assistant(response.provenance, response.output)];
        let request = models.request(&context, None, &input).unwrap();
        assert_eq!(request.input[0], reasoning);
        assert_eq!(request.input[1]["phase"], "final_answer");
    }

    #[test]
    fn generate_preserves_web_search_on_replay() {
        let tool_call = json!({
            "type": "web_search_call",
            "id": "ws_test",
            "status": "completed",
            "action": {
                "type": "search",
                "queries": ["Fleet documentation"],
                "sources": [{
                    "type": "url",
                    "url": "https://example.com",
                }],
            },
        });
        let http = TestHttp::new([response(
            &[tool_call.clone(), message("Found it.", None)],
            false,
        )]);
        let models = models(http.clone());
        let context = context();

        let chunks = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(
            chunks[0],
            ModelResponseChunk::ProviderToolCallStart(0, "ws_test".into(), "web_search".into(),)
        );
        assert_eq!(
            chunks[1],
            ModelResponseChunk::OutputEnd(
                0,
                ModelOutput::ProviderToolCall(ModelProviderToolCall {
                    call_id: "ws_test".into(),
                    name: "web_search".into(),
                    content: Some("Search: Fleet documentation".into()),
                    payload: tool_call.to_string(),
                }),
            )
        );
        let ModelResponseChunk::ResponseEnd(response) = chunks.into_iter().last().unwrap() else {
            panic!("expected response end");
        };
        assert_eq!(http.requests().len(), 1);

        let input = [ModelInput::Assistant(response.provenance, response.output)];
        let request = models.request(&context, None, &input).unwrap();
        assert_eq!(request.input[0], tool_call);
        assert_eq!(request.input[1]["phase"], "final_answer");

        let mut context = context;
        context.model.name = "another-model".into();
        let request = models.request(&context, None, &input).unwrap();
        assert_eq!(request.input.len(), 1);
        assert_eq!(request.input[0]["content"][0]["text"], "Found it.");

        context.model.name = "test-model".into();
        let models = OpenAiModels::new(
            TestHttp::default(),
            HeaderMap::new(),
            OpenAiConfig {
                provider_id: "another-provider".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let request = models.request(&context, None, &input).unwrap();
        assert_eq!(request.input.len(), 1);
        assert_eq!(request.input[0]["content"][0]["text"], "Found it.");
    }

    #[test]
    fn generate_continues_after_web_search_commentary() {
        let tool_call = json!({
            "type": "web_search_call",
            "id": "ws_test",
            "status": "completed",
            "action": {
                "type": "open_page",
                "url": "https://example.com",
            },
        });
        let http = TestHttp::new([
            response(
                &[tool_call.clone(), message("Reading…", Some("commentary"))],
                false,
            ),
            response(&[message("Done.", Some("final_answer"))], false),
        ]);
        let models = models(http.clone());
        let context = context();

        let chunks = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap();
        let ModelResponseChunk::ResponseEnd(response) = chunks.last().unwrap() else {
            panic!("expected response end");
        };
        assert_eq!(response.output.len(), 3);
        assert_eq!(response.output[2], ModelOutput::Text("Done.".into()));

        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        let request: Value = serde_json::from_slice(requests[1].body()).unwrap();
        assert_eq!(request["input"][0], tool_call);
    }

    #[test]
    fn generate_rejects_unfinished_web_search() {
        let tool_call = json!({
            "type": "web_search_call",
            "id": "ws_test",
            "status": "searching",
        });
        let http = TestHttp::new([response(&[tool_call], true)]);
        let models = models(http.clone());
        let context = context();

        let chunks =
            futures_executor::block_on(models.generate(&context, None, &[]).collect::<Vec<_>>());
        assert_eq!(chunks.len(), 2);
        let mut chunks = chunks.into_iter();
        let start = chunks.next().unwrap().unwrap();
        assert_eq!(
            start,
            ModelResponseChunk::ProviderToolCallStart(0, "ws_test".into(), "web_search".into(),)
        );
        let error = chunks.next().unwrap().unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::Other);
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn generate_rejects_unfinished_tool_calls() {
        let tool_call = json!({
            "type": "function_call",
            "call_id": "call-test",
            "name": "search",
            "arguments": "{}",
            "status": "incomplete",
        });
        let http = TestHttp::new([response(&[tool_call], true)]);
        let models = models(http.clone());
        let context = context();

        let chunks =
            futures_executor::block_on(models.generate(&context, None, &[]).collect::<Vec<_>>());
        assert_eq!(chunks.len(), 2);
        assert_matches!(chunks[0], Ok(ModelResponseChunk::ToolCallStart(..)));
        let error = chunks.into_iter().last().unwrap().unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::Other);
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn generate_rejects_truncated_streams() {
        let http = TestHttp::new([sse(&[json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": message("Hello", None),
        })])]);
        let models = models(http);
        let context = context();

        let chunks =
            futures_executor::block_on(models.generate(&context, None, &[]).collect::<Vec<_>>());
        assert_eq!(chunks.len(), 2);
        assert_matches!(chunks[0], Ok(ModelResponseChunk::TextStart(0)));
        let error = chunks.into_iter().last().unwrap().unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::Other);
    }

    #[test]
    fn generate_accepts_completion_without_repeated_output() {
        let output = message("Done.", None);
        let http = TestHttp::new([sse(&[
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": output,
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": output,
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "output": [],
                    "usage": {
                        "input_tokens": 8,
                        "output_tokens": 3,
                    },
                },
            }),
        ])]);
        let models = models(http);
        let context = context();

        let chunks = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap();
        let ModelResponseChunk::ResponseEnd(response) = chunks.into_iter().last().unwrap() else {
            panic!("expected response end");
        };
        assert_eq!(response.output, vec![ModelOutput::Text("Done.".into())]);
        assert_eq!(response.usage.input, 8);
        assert_eq!(response.usage.output, 3);
    }

    #[test]
    fn generate_rejects_completion_with_unfinished_output() {
        let output = message("Done.", None);
        let http = TestHttp::new([sse(&[
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": output,
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "output": [output],
                    "usage": {
                        "input_tokens": 8,
                        "output_tokens": 3,
                    },
                },
            }),
        ])]);
        let models = models(http);
        let context = context();

        let error = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::Other);
    }

    #[test]
    fn generate_bounds_internal_continuations() {
        let http = TestHttp::new(
            (0..=8).map(|_| response(&[message("Still working.", Some("commentary"))], false)),
        );
        let models = models(http.clone());
        let context = context();

        let error = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::Other);
        assert_eq!(http.requests().len(), 9);
    }

    #[test]
    fn generate_returns_and_reuses_stored_continuations() {
        let http = TestHttp::new([
            response(&[message("Hello.", None)], false),
            response(&[message("Again.", None)], false),
        ]);
        let config = OpenAiConfig {
            store: true,
            ..Default::default()
        };
        let models = OpenAiModels::new(http.clone(), HeaderMap::new(), config).unwrap();
        let context = context();

        let chunks = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap();
        let ModelResponseChunk::ResponseEnd(response) = chunks.last().unwrap() else {
            panic!("expected response end");
        };
        let continuation = ModelContinuation {
            token: response.continuation_token.clone().unwrap(),
            provenance: response.provenance.clone(),
        };
        assert_eq!(continuation.token, "resp_test");

        futures_executor::block_on(
            models
                .generate(&context, Some(&continuation), &[])
                .try_collect::<Vec<_>>(),
        )
        .unwrap();
        let requests = http.requests();
        let request: Value = serde_json::from_slice(requests[1].body()).unwrap();
        assert_eq!(request["previous_response_id"], "resp_test");
        assert_eq!(request["instructions"], "Be helpful.");
    }

    #[test]
    fn generate_classifies_http_and_stream_errors() {
        let detail = json!({
            "code": "context_length_exceeded",
            "message": "Too many tokens.",
        });
        let http = TestHttp::new([
            json_response(
                StatusCode::BAD_REQUEST,
                &json!({
                    "error": detail,
                }),
            ),
            sse(&[json!({
                "type": "error",
                "error": detail,
            })]),
            sse(&[json!({
                "type": "response.failed",
                "response": {
                    "error": detail,
                },
            })]),
        ]);
        let models = models(http);
        let context = context();

        for _ in 0..3 {
            let error = futures_executor::block_on(
                models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), ModelsErrorKind::ContextTooLong);
            assert!(error.to_string().contains("Too many tokens."));
        }
    }

    #[test]
    fn generate_preserves_http_error_details() {
        for body in [
            json!({
                "detail": "Unsupported service_tier: fast",
            })
            .to_string(),
            "Unsupported service_tier: fast".into(),
        ] {
            let response = Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(stream::iter(vec![Ok(Bytes::from(body))]))
                .unwrap();
            let models = models(TestHttp::new([response]));
            let context = context();

            let error = futures_executor::block_on(
                models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), ModelsErrorKind::Other);
            assert_eq!(
                error.to_string(),
                "OpenAI request failed with HTTP 400 Bad Request: Unsupported service_tier: fast",
            );
        }
    }

    #[test]
    fn generate_rejects_incompatible_saved_state_before_sending() {
        let http = TestHttp::default();
        let models = models(http.clone());
        let context = context();
        let provenance = ModelProvenance {
            provider_id: "another-provider".into(),
            model_id: "test-model".into(),
        };
        let continuation = ModelContinuation {
            token: "resp_test".into(),
            provenance: provenance.clone(),
        };
        let compaction = ModelInput::Compaction(
            provenance,
            ModelCompaction {
                content: None,
                redacted_content: Some("opaque-state".into()),
                attestation: None,
            },
        );

        let error = futures_executor::block_on(
            models
                .generate(&context, Some(&continuation), &[])
                .try_collect::<Vec<_>>(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::InvalidContinuation);

        let error = futures_executor::block_on(
            models
                .generate(&context, None, &[compaction])
                .try_collect::<Vec<_>>(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::InvalidCompaction);
        assert!(http.requests().is_empty());
    }

    #[test]
    fn compact_preserves_the_complete_returned_window() {
        let output = json!([
            {
                "type": "message",
                "id": "msg_retained",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "Remember this.",
                }],
            },
            {
                "type": "compaction",
                "id": "cmp_test",
                "encrypted_content": "opaque-context",
            },
        ]);
        let http = TestHttp::new([
            json_response(
                StatusCode::OK,
                &json!({
                    "output": output,
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 20,
                    },
                }),
            ),
            response(&[message("Remembered.", None)], false),
        ]);
        let models = models(http.clone());
        let context = context();
        let input = [ModelInput::User(vec![ContentBlock::Text(
            "Remember this.".into(),
        )])];

        let compaction = futures_executor::block_on(models.compact(
            &context,
            None,
            &input,
            Some("Keep the important facts."),
        ))
        .unwrap();
        assert_eq!(compaction.usage.output, 20);
        futures_executor::block_on(
            models
                .generate(&context, None, &compaction.input)
                .try_collect::<Vec<_>>(),
        )
        .unwrap();

        let requests = http.requests();
        assert_eq!(
            requests[0].uri(),
            "https://api.openai.com/v1/responses/compact"
        );
        let compact: Value = serde_json::from_slice(requests[0].body()).unwrap();
        let generate: Value = serde_json::from_slice(requests[1].body()).unwrap();
        assert_eq!(compact["instructions"], "Keep the important facts.");
        assert_eq!(generate["input"], output);
        assert_eq!(generate["instructions"], "Be helpful.");
    }

    #[test]
    fn compact_preserves_the_request_when_falling_back_to_streaming() {
        let checkpoint = json!({
            "type": "compaction",
            "id": "cmp_first",
            "encrypted_content": "original-context",
        });
        let http = TestHttp::new([
            rejected_compaction(),
            response(
                &[
                    checkpoint.clone(),
                    message("The task is finished.", Some("final_answer")),
                    json!({
                        "type": "compaction",
                        "id": "cmp_later",
                        "encrypted_content": "later-context",
                    }),
                ],
                false,
            ),
            response(&[message("Running the tests.", None)], false),
        ]);
        let credentials = RefreshingCredentials(AtomicBool::new(false));
        let models = OpenAiModels::new(http.clone(), credentials, OpenAiConfig::default()).unwrap();
        let mut context = context();
        context.model.config = json!({
            "effort": "high",
            "tools": [{
                "type": "web_search",
            }],
        })
        .as_object()
        .unwrap()
        .clone();
        context.tools.push(ModelTool {
            name: "test-tool".into(),
            description: Some("A test tool.".into()),
            input_schema: JsonObject::new(),
            is_async: false,
        });
        let input = [ModelInput::User(vec![ContentBlock::Text(
            "Run the tests.".into(),
        )])];

        let compacted = futures_executor::block_on(models.compact(
            &context,
            None,
            &input,
            Some("Keep the test commands."),
        ))
        .unwrap();
        futures_executor::block_on(
            models
                .generate(&context, None, &compacted.input)
                .try_collect::<Vec<_>>(),
        )
        .unwrap();

        assert_eq!(
            compacted.usage,
            ModelUsage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            }
        );
        let requests = http.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].uri().path(), "/v1/responses/compact");
        assert_eq!(requests[1].uri().path(), "/v1/responses");
        assert_eq!(requests[1].headers()[header::AUTHORIZATION], "Bearer old");
        let compact: Value = serde_json::from_slice(requests[0].body()).unwrap();
        let streaming: Value = serde_json::from_slice(requests[1].body()).unwrap();
        let generate: Value = serde_json::from_slice(requests[2].body()).unwrap();
        assert_eq!(streaming["instructions"], generate["instructions"]);
        assert_eq!(streaming["tools"], generate["tools"]);
        assert_eq!(streaming["reasoning"], generate["reasoning"]);
        assert_eq!(streaming["input"], compact["input"]);
        assert!(streaming.get("tool_choice").is_none());
        assert_eq!(
            streaming["context_management"][0]["compact_threshold"],
            1024
        );
        assert_eq!(streaming["store"], false);
        assert_eq!(generate["input"], json!([checkpoint]));
        assert!(generate.get("context_management").is_none());
        assert!(generate.get("tool_choice").is_none());
    }

    #[test]
    fn compact_does_not_fall_back_for_unrelated_errors() {
        for (status, code, kind) in [
            (
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                ModelsErrorKind::Other,
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                ModelsErrorKind::Other,
            ),
            (
                StatusCode::BAD_REQUEST,
                "context_length_exceeded",
                ModelsErrorKind::ContextTooLong,
            ),
        ] {
            let http = TestHttp::new([json_response(
                status,
                &json!({
                    "error": {
                        "code": code,
                    },
                }),
            )]);
            let models = models(http.clone());
            let context = context();

            let error =
                futures_executor::block_on(models.compact(&context, None, &[], None)).unwrap_err();

            assert_eq!(error.kind(), kind);
            assert_eq!(http.requests().len(), 1);
        }
    }

    #[test]
    fn compact_rejects_incomplete_and_unsafe_streamed_output() {
        for response in [
            sse(&[]),
            sse(&[json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "type": "compaction",
                    "encrypted_content": "partial-context",
                },
            })]),
            response(
                &[
                    message("The task is finished.", None),
                    json!({
                        "type": "compaction",
                        "encrypted_content": "later-context",
                    }),
                ],
                false,
            ),
            response(
                &[json!({
                    "type": "function_call",
                    "call_id": "call_test",
                    "name": "test-tool",
                    "arguments": "{}",
                    "status": "completed",
                })],
                false,
            ),
            response(
                &[json!({
                    "type": "compaction",
                    "encrypted_content": "",
                })],
                false,
            ),
            response(
                &[json!({
                    "type": "compaction",
                    "encrypted_content": "partial-context",
                    "status": "incomplete",
                })],
                false,
            ),
            sse(&[json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {
                    "type": "compaction",
                    "encrypted_content": "later-context",
                },
            })]),
            sse(&[json!({
                "type": "response.completed",
                "response": {
                    "output": [],
                },
            })]),
        ] {
            let models = models(TestHttp::new([rejected_compaction(), response]));
            let context = context();

            let error =
                futures_executor::block_on(models.compact(&context, None, &[], None)).unwrap_err();

            assert_eq!(error.kind(), ModelsErrorKind::Other);
        }
    }

    #[test]
    fn compact_preserves_stream_error_kinds() {
        let models = models(TestHttp::new([
            rejected_compaction(),
            sse(&[json!({
                "type": "response.failed",
                "response": {
                    "error": {
                        "code": "context_length_exceeded",
                    },
                },
            })]),
        ]));
        let context = context();

        let error =
            futures_executor::block_on(models.compact(&context, None, &[], None)).unwrap_err();

        assert_eq!(error.kind(), ModelsErrorKind::ContextTooLong);
    }

    #[test]
    fn compact_returns_a_checkpoint_without_waiting_for_response_completion() {
        let checkpoint = json!({
            "type": "compaction",
            "encrypted_content": "opaque-context",
        });
        let models = models(TestHttp::new([
            json_response(
                StatusCode::FORBIDDEN,
                &json!({
                    "error": {
                        "code": "subscription_sharing_route_not_supported",
                    },
                }),
            ),
            sse(&[
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "type": "compaction",
                    },
                }),
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": checkpoint,
                }),
                json!({
                    "type": "response.failed",
                    "response": {
                        "error": {
                            "code": "server_error",
                        },
                    },
                }),
            ]),
        ]));
        let context = context();

        let compacted =
            futures_executor::block_on(models.compact(&context, None, &[], None)).unwrap();

        let ModelInput::Compaction(_, state) = &compacted.input[0] else {
            panic!("expected compaction");
        };
        let state: Value = serde_json::from_str(state.redacted_content.as_ref().unwrap()).unwrap();
        assert_eq!(state["output"], json!([checkpoint]));
        assert_eq!(compacted.usage.output, 0);
    }

    #[test]
    fn compact_requests_full_input_before_falling_back_with_a_continuation() {
        let http = TestHttp::new([rejected_compaction()]);
        let models = OpenAiModels::new(
            http.clone(),
            HeaderMap::new(),
            OpenAiConfig {
                store: true,
                ..Default::default()
            },
        )
        .unwrap();
        let context = context();
        let continuation = ModelContinuation {
            provenance: models.provenance(&context),
            token: "resp_previous".into(),
        };

        let error =
            futures_executor::block_on(models.compact(&context, Some(&continuation), &[], None))
                .unwrap_err();

        assert_eq!(error.kind(), ModelsErrorKind::InvalidContinuation);
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn compact_preserves_image_references_for_replay() {
        use base64::Engine as _;
        use fleet_kit::Blobs as _;

        let image = include_bytes!("../../fleet-store/tests/fixtures/image.png");
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(image)
        );
        let retained = json!([{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_image",
                "image_url": data_url,
            }],
        }]);
        let http = TestHttp::new([
            json_response(
                StatusCode::OK,
                &json!({
                    "output": retained,
                    "usage": {
                        "input_tokens": 20,
                        "output_tokens": 5,
                    },
                }),
            ),
            response(&[message("Red and green.", None)], false),
        ]);
        let blobs = fleet_store::Store::memory().unwrap();
        let models = models(http.clone()).with_blobs(blobs.clone());
        let context = context();
        let input = [ModelInput::User(vec![ContentBlock::Image(1.into())])];

        futures_executor::block_on(async {
            blobs.put(1.into(), "image/png", image).await.unwrap();
            let compacted = models.compact(&context, None, &input, None).await.unwrap();
            let ModelInput::Compaction(_, state) = &compacted.input[0] else {
                panic!("expected compaction");
            };
            assert!(!state.redacted_content.as_ref().unwrap().contains("base64"));
            models
                .generate(&context, None, &compacted.input)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
        });

        let requests = http.requests();
        let compact: Value = serde_json::from_slice(requests[0].body()).unwrap();
        let generate: Value = serde_json::from_slice(requests[1].body()).unwrap();
        assert_eq!(
            compact["input"][0]["content"][0]["text"],
            "Image reference: 1"
        );
        assert_eq!(compact["input"][0]["content"][1]["image_url"], data_url);
        assert_eq!(generate["input"], retained);
    }

    #[test]
    fn missing_images_fail_before_sending_a_model_request() {
        let http = TestHttp::default();
        let models = models(http.clone()).with_blobs(fleet_store::Store::memory().unwrap());
        let context = context();
        let input = [ModelInput::User(vec![ContentBlock::Image(1.into())])];

        let error = futures_executor::block_on(
            models
                .generate(&context, None, &input)
                .try_collect::<Vec<_>>(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), ModelsErrorKind::Other);
        assert!(http.requests().is_empty());
    }

    #[test]
    fn tool_images_preserve_structured_output_and_error_status() {
        use fleet_core::{ToolOutput, ToolResult};
        use fleet_kit::Blobs as _;

        let http = TestHttp::new([response(&[message("Seen.", None)], false)]);
        let blobs = fleet_store::Store::memory().unwrap();
        let models = models(http.clone()).with_blobs(blobs.clone());
        let context = context();
        let input = [ModelInput::ToolResult(ToolResult {
            call_id: "read-image".into(),
            output: ToolOutput {
                content: vec![
                    ContentBlock::Text("Screenshot".into()),
                    ContentBlock::Image(1.into()),
                ],
                structured_content: Some(JsonObject::from_iter([("ok".into(), false.into())])),
                is_error: true,
            },
        })];

        futures_executor::block_on(async {
            blobs
                .put(
                    1.into(),
                    "image/png",
                    include_bytes!("../../fleet-store/tests/fixtures/image.png"),
                )
                .await
                .unwrap();
            models
                .generate(&context, None, &input)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
        });

        let requests = http.requests();
        let request: Value = serde_json::from_slice(requests[0].body()).unwrap();
        let output = &request["input"][0];
        assert_eq!(output["call_id"], "read-image");
        assert_eq!(output["output"][0]["text"], "Tool returned an error:");
        assert_eq!(output["output"][3]["type"], "input_image");
        assert_eq!(output["output"][4]["text"], "{\"ok\":false}");
    }

    struct RefreshingCredentials(AtomicBool);

    impl Credentials for RefreshingCredentials {
        type Error = Infallible;

        fn get(&self) -> impl Future<Output = Result<HeaderMap, Self::Error>> + Send {
            let key = if self.0.load(Ordering::Relaxed) {
                "new"
            } else {
                "old"
            };
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {key}").parse().unwrap(),
            );
            core::future::ready(Ok(headers))
        }

        fn refresh(
            &self,
            rejected: &HeaderMap,
        ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
            assert_eq!(rejected[header::AUTHORIZATION], "Bearer old");
            self.0.store(true, Ordering::Relaxed);
            core::future::ready(Ok(true))
        }
    }

    #[test]
    fn generate_refreshes_rejected_credentials_once() {
        let http = TestHttp::new([
            json_response(StatusCode::UNAUTHORIZED, &json!({})),
            json_response(StatusCode::UNAUTHORIZED, &json!({})),
        ]);
        let credentials = RefreshingCredentials(AtomicBool::new(false));
        let models = OpenAiModels::new(http.clone(), credentials, OpenAiConfig::default()).unwrap();
        let context = context();

        let error = futures_executor::block_on(
            models.generate(&context, None, &[]).try_collect::<Vec<_>>(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ModelsErrorKind::Other);

        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].headers()[header::AUTHORIZATION], "Bearer old");
        assert_eq!(requests[1].headers()[header::AUTHORIZATION], "Bearer new");
        assert_eq!(requests[0].body(), requests[1].body());
    }
}
