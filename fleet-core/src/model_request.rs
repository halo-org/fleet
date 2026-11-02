use crate::{
    Event, EventLog, EventPayload, ModelContext, ModelContinuation, ModelInput, ModelsErrorKind,
};
use alloc::{boxed::Box, vec::Vec};
use core::pin::Pin;
use futures_util::TryStream;

pub struct ModelRequest<'log, S> {
    event_log: &'log mut EventLog<S>,
    context: ModelContext,
    #[expect(dead_code, reason = "will be used by reduce_input in the future")]
    context_window: u32,
    continuation: Option<ModelContinuation>,
    input: Vec<ModelInput>,
    should_compact: bool,
    include_continuation: bool,
    include_compaction: bool,
}

enum Recovery {
    None,
    Compact,
    RebuildWithoutContinuation,
    RebuildWithoutCompaction,
    ReduceInput,
}

impl<S> ModelRequest<'_, S> {
    pub const fn context(&self) -> &ModelContext {
        &self.context
    }

    pub const fn continuation(&self) -> Option<&ModelContinuation> {
        self.continuation.as_ref()
    }

    pub fn input(&self) -> &[ModelInput] {
        &self.input
    }

    pub const fn should_compact(&self) -> bool {
        self.should_compact
    }

    const fn recovery(&self, error_kind: ModelsErrorKind) -> Recovery {
        match error_kind {
            ModelsErrorKind::InvalidContinuation if self.include_continuation => {
                Recovery::RebuildWithoutContinuation
            }
            ModelsErrorKind::InvalidCompaction if self.include_compaction => {
                Recovery::RebuildWithoutCompaction
            }
            ModelsErrorKind::ContextTooLong if self.should_compact => Recovery::ReduceInput,
            ModelsErrorKind::ContextTooLong => Recovery::Compact,
            _ => Recovery::None,
        }
    }

    fn reduce_input(&mut self) -> bool {
        // TODO: Implement this to remove most likely stale input first until we reach an estimated
        // target token count.
        self.input = core::mem::take(&mut self.input);
        false
    }
}

impl<'log, S> ModelRequest<'log, S>
where
    S: TryStream<Ok = Event>,
    Pin<Box<S>>: TryStream<Ok = Event, Error = S::Error>,
{
    pub async fn prepare(
        event_log: &'log mut EventLog<S>,
        context: ModelContext,
        context_window: u32,
        should_compact: bool,
    ) -> Result<Self, S::Error> {
        let mut request = Self {
            event_log,
            context,
            context_window,
            continuation: None,
            input: Vec::new(),
            should_compact,
            include_continuation: true,
            include_compaction: true,
        };

        request.rebuild_input().await?;

        Ok(request)
    }

    pub async fn recover(&mut self, error_kind: ModelsErrorKind) -> Result<bool, S::Error> {
        match self.recovery(error_kind) {
            Recovery::None => Ok(false),
            Recovery::Compact => {
                self.should_compact = true;
                Ok(true)
            }
            Recovery::RebuildWithoutContinuation => {
                self.include_continuation = false;
                self.rebuild_input().await?;
                Ok(true)
            }
            Recovery::RebuildWithoutCompaction => {
                self.include_continuation = false;
                self.include_compaction = false;
                self.should_compact = true;
                self.rebuild_input().await?;
                Ok(true)
            }
            Recovery::ReduceInput => {
                if self.continuation.is_some() {
                    self.include_continuation = false;
                    self.rebuild_input().await?;
                }

                Ok(self.reduce_input())
            }
        }
    }

    async fn rebuild_input(&mut self) -> Result<(), S::Error> {
        let mut input = Vec::new();
        let mut input_buffer = Vec::new();
        let mut continuation = None;
        let mut last_compacted_event = None;

        for index in 0.. {
            let Some(event) = self.event_log.get(index).await? else {
                break;
            };

            if last_compacted_event.is_some_and(|cursor| cursor >= event.cursor) {
                break;
            }

            match &event.payload {
                EventPayload::Input(_, content) => {
                    input_buffer.push(ModelInput::User(content.clone()));
                }
                EventPayload::ModelResponse(response) => {
                    let can_use_continuation =
                        self.include_continuation && last_compacted_event.is_none();

                    if can_use_continuation && let Some(token) = &response.continuation_token {
                        continuation = Some(ModelContinuation {
                            token: token.clone(),
                            provenance: response.provenance.clone(),
                        });

                        break;
                    }

                    input_buffer.push(ModelInput::Assistant(
                        response.provenance.clone(),
                        response.output.clone(),
                    ));
                }
                EventPayload::ToolResult(result) => {
                    input_buffer.push(ModelInput::ToolResult(result.clone()));
                }
                EventPayload::Compaction(compaction) => {
                    let can_use_compaction =
                        self.include_compaction && last_compacted_event.is_none();

                    if can_use_compaction {
                        last_compacted_event = Some(compaction.last_event);
                        input = compaction.input.clone();
                    }
                }
                EventPayload::TurnEnd(_) => {}
            }
        }

        input_buffer.reverse();
        input.extend(input_buffer);

        self.input = input;
        self.continuation = continuation;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Compaction, Configured, ContentBlock, JsonObject, ModelCompaction, ModelProvenance,
        ModelResponse, ModelUsage, TurnEnd,
    };
    use alloc::vec;
    use core::fmt;
    use futures_util::stream;

    const USAGE: ModelUsage = ModelUsage {
        input: 100,
        output: 20,
        cache_read: 0,
        cache_write: 0,
    };

    fn context() -> ModelContext {
        ModelContext {
            model: Configured {
                name: "current-model".into(),
                config: JsonObject::new(),
            },
            system_prompt: "system prompt".into(),
            tools: Vec::new(),
            role_extensions: JsonObject::new(),
        }
    }

    fn provenance() -> ModelProvenance {
        ModelProvenance {
            provider_id: "provider".into(),
            model_id: "previous-model".into(),
        }
    }

    fn input(cursor: u128, text: &str) -> Event {
        Event {
            cursor: cursor.into(),
            payload: EventPayload::Input(cursor.into(), vec![ContentBlock::Text(text.into())]),
        }
    }

    fn model_response(cursor: u128, continuation_token: &str) -> Event {
        Event {
            cursor: cursor.into(),
            payload: EventPayload::ModelResponse(ModelResponse {
                output: Vec::new(),
                usage: USAGE,
                provenance: provenance(),
                continuation_token: Some(continuation_token.into()),
            }),
        }
    }

    fn compacted_input(summary: &str) -> ModelInput {
        ModelInput::Compaction(
            provenance(),
            ModelCompaction {
                content: Some(summary.into()),
                redacted_content: None,
                attestation: None,
            },
        )
    }

    fn compaction(cursor: u128, last_event: u128, summary: &str) -> Event {
        Event {
            cursor: cursor.into(),
            payload: EventPayload::Compaction(Compaction {
                last_event: last_event.into(),
                input: vec![compacted_input(summary)],
                pending_tool_calls: Vec::new(),
                usage: USAGE,
            }),
        }
    }

    #[test]
    fn prepare_orders_input_chronologically() {
        let event_a = input(1, "first");
        let event_b = Event {
            cursor: 2.into(),
            payload: EventPayload::TurnEnd(TurnEnd::Completed),
        };
        let event_c = input(3, "second");
        let expected = [
            ModelInput::User(vec![ContentBlock::Text("first".into())]),
            ModelInput::User(vec![ContentBlock::Text("second".into())]),
        ];

        let events = [event_c, event_b, event_a].map(Ok::<_, fmt::Error>);
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let request = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .unwrap();
            assert_eq!(request.input(), &expected);
        });
    }

    #[test]
    fn prepare_uses_latest_continuation() {
        let event_a = model_response(1, "older");
        let event_b = model_response(2, "latest");
        let event_c = input(3, "follow-up");
        let continuation = ModelContinuation {
            token: "latest".into(),
            provenance: provenance(),
        };
        let expected = [ModelInput::User(vec![ContentBlock::Text(
            "follow-up".into(),
        )])];

        let events = [event_c, event_b, event_a].map(Ok::<_, fmt::Error>);
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let request = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .unwrap();
            assert_eq!(request.input(), &expected);
            assert_eq!(request.continuation(), Some(&continuation));
        });
    }

    #[test]
    fn prepare_replaces_compacted_input() {
        let event_a = input(1, "original input");
        let event_b = compaction(2, 1, "summary");
        let event_c = input(3, "follow-up");
        let expected = [
            compacted_input("summary"),
            ModelInput::User(vec![ContentBlock::Text("follow-up".into())]),
        ];

        let events = [event_c, event_b, event_a].map(Ok::<_, fmt::Error>);
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let request = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .unwrap();
            assert_eq!(request.input(), &expected);
        });
    }

    #[test]
    fn prepare_propagates_stream_errors() {
        let events = [Err::<Event, _>(fmt::Error)];
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let error = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .map(|_| ())
                .unwrap_err();
            assert_eq!(error, fmt::Error);
        });
    }

    #[test]
    fn recover_rebuilds_without_continuation() {
        let event_a = input(1, "request");
        let event_b = model_response(2, "continuation");
        let expected = [
            ModelInput::User(vec![ContentBlock::Text("request".into())]),
            ModelInput::Assistant(provenance(), vec![]),
        ];

        let events = [event_b, event_a].map(Ok::<_, fmt::Error>);
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let mut request = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .unwrap();

            let recovered = request
                .recover(ModelsErrorKind::InvalidContinuation)
                .await
                .unwrap();
            assert!(recovered);
            assert_eq!(request.input(), &expected);
            assert_eq!(request.continuation(), None);
        });
    }

    #[test]
    fn recover_rebuilds_without_compaction() {
        let event_a = input(1, "original input");
        let event_b = model_response(2, "continuation");
        let event_c = compaction(3, 2, "summary");
        let expected = [
            ModelInput::User(vec![ContentBlock::Text("original input".into())]),
            ModelInput::Assistant(provenance(), vec![]),
        ];

        let events = [event_c, event_b, event_a].map(Ok::<_, fmt::Error>);
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let mut request = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .unwrap();

            let recovered = request
                .recover(ModelsErrorKind::InvalidCompaction)
                .await
                .unwrap();
            assert!(recovered);
            assert_eq!(request.input(), &expected);
            assert_eq!(request.continuation(), None);
            assert!(request.should_compact());
        });
    }

    #[test]
    fn recover_requests_compaction_when_context_is_too_long() {
        let event = input(1, "request");

        let events = [Ok::<_, fmt::Error>(event)];
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let mut request = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .unwrap();
            assert!(!request.should_compact());

            let recovered = request
                .recover(ModelsErrorKind::ContextTooLong)
                .await
                .unwrap();
            assert!(recovered);
            assert!(request.should_compact());
        });
    }

    #[test]
    fn recover_returns_false_for_other_errors() {
        let stream = stream::empty::<Result<Event, fmt::Error>>();

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let mut request = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .unwrap();

            let recovered = request.recover(ModelsErrorKind::Other).await.unwrap();
            assert!(!recovered);
        });
    }

    #[test]
    fn recover_propagates_stream_errors() {
        let event = model_response(1, "continuation");

        let events = [Ok(event), Err(fmt::Error)];
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let mut request = ModelRequest::prepare(&mut event_log, context(), 4096, false)
                .await
                .unwrap();

            let error = request
                .recover(ModelsErrorKind::InvalidContinuation)
                .await
                .unwrap_err();
            assert_eq!(error, fmt::Error);
        });
    }
}
