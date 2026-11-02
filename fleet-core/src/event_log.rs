use crate::{Event, EventPayload};
use alloc::{boxed::Box, vec::Vec};
use core::pin::Pin;
use futures_util::{TryStream, TryStreamExt as _};

pub struct EventLog<S> {
    stream: Pin<Box<S>>,
    events: Vec<Event>,
    exhausted: bool,
}

impl<S> EventLog<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream: Box::pin(stream),
            events: Vec::new(),
            exhausted: false,
        }
    }
}

impl<S> EventLog<S>
where
    S: TryStream<Ok = Event>,
    Pin<Box<S>>: TryStream<Ok = Event, Error = S::Error>,
{
    pub async fn get(&mut self, index: usize) -> Result<Option<&Event>, S::Error> {
        while self.events.len() <= index && !self.exhausted {
            match self.stream.try_next().await? {
                Some(event) => self.events.push(event),
                None => self.exhausted = true,
            }
        }

        Ok(self.events.get(index))
    }

    pub async fn is_turn_finished(&mut self) -> Result<bool, S::Error> {
        for index in 0.. {
            match self.get(index).await?.map(|event| &event.payload) {
                Some(EventPayload::Compaction(_)) => {}
                Some(EventPayload::TurnEnd(_)) => return Ok(true),
                _ => break,
            }
        }

        Ok(false)
    }

    pub async fn last_token_count(&mut self) -> Result<Option<u32>, S::Error> {
        for index in 0.. {
            match self.get(index).await?.map(|event| &event.payload) {
                Some(EventPayload::ModelResponse(response)) => {
                    return Ok(Some(
                        response.usage.input.saturating_add(response.usage.output),
                    ));
                }
                Some(EventPayload::Compaction(_)) | None => break,
                Some(
                    EventPayload::Input(..)
                    | EventPayload::ToolResult(_)
                    | EventPayload::TurnEnd(_),
                ) => {}
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Compaction, ContentBlock, ModelOutput, ModelProvenance, ModelResponse, ModelUsage,
        ToolOutput, ToolResult, TurnEnd,
    };
    use alloc::vec;
    use core::{cell::Cell, fmt, task::Poll};
    use futures_util::{FutureExt as _, StreamExt as _, stream};

    const USAGE: ModelUsage = ModelUsage {
        input: 100,
        output: 20,
        cache_read: 50,
        cache_write: 25,
    };

    fn input(cursor: u128) -> Event {
        Event {
            cursor: cursor.into(),
            payload: EventPayload::Input(cursor.into(), vec![ContentBlock::Text("input".into())]),
        }
    }

    fn model_response(cursor: u128, output: Vec<ModelOutput>, usage: ModelUsage) -> Event {
        Event {
            cursor: cursor.into(),
            payload: EventPayload::ModelResponse(ModelResponse {
                output,
                usage,
                provenance: ModelProvenance {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                },
                continuation_token: None,
            }),
        }
    }

    fn tool_result(cursor: u128) -> Event {
        Event {
            cursor: cursor.into(),
            payload: EventPayload::ToolResult(ToolResult {
                call_id: "call-1".into(),
                output: ToolOutput {
                    content: vec![ContentBlock::Text("result".into())],
                    structured_content: None,
                    is_error: false,
                },
            }),
        }
    }

    fn compaction(cursor: u128) -> Event {
        Event {
            cursor: cursor.into(),
            payload: EventPayload::Compaction(Compaction {
                last_event: 1.into(),
                input: Vec::new(),
                pending_tool_calls: Vec::new(),
                usage: USAGE,
            }),
        }
    }

    fn turn_end(cursor: u128, payload: TurnEnd) -> Event {
        Event {
            cursor: cursor.into(),
            payload: EventPayload::TurnEnd(payload),
        }
    }

    #[test]
    fn get_returns_none_when_empty() {
        let mut event_log = EventLog::new(stream::empty::<Result<Event, fmt::Error>>());

        futures_executor::block_on(async {
            let event = event_log.get(0).await.unwrap();
            assert_eq!(event, None);
        });
    }

    #[test]
    fn get_loads_and_caches_events_on_demand() {
        let event_a = input(3);
        let event_b = input(2);
        let event_c = input(1);

        let count = Cell::new(0);
        let events = [event_a.clone(), event_b.clone(), event_c.clone()].map(Ok::<_, fmt::Error>);
        let stream = stream::iter(events).inspect(|_| count.set(count.get() + 1));

        let mut event_log = EventLog::new(stream);
        assert_eq!(count.get(), 0);

        futures_executor::block_on(async {
            let event = event_log.get(1).await.unwrap();
            assert_eq!(event, Some(&event_b));
            assert_eq!(count.get(), 2);

            let event = event_log.get(0).await.unwrap();
            assert_eq!(event, Some(&event_a));
            let event = event_log.get(1).await.unwrap();
            assert_eq!(event, Some(&event_b));
            assert_eq!(count.get(), 2);

            let event = event_log.get(2).await.unwrap();
            assert_eq!(event, Some(&event_c));
            assert_eq!(count.get(), 3);
        });
    }

    #[test]
    fn get_does_not_poll_stream_after_exhaustion() {
        let event_a = input(1);

        let count = Cell::new(0);
        let mut events = core::iter::once(Ok::<_, fmt::Error>(event_a.clone()));
        let stream = stream::poll_fn(|_| {
            count.set(count.get() + 1);
            Poll::Ready(events.next())
        });

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let event = event_log.get(1).await.unwrap();
            assert_eq!(event, None);
            assert_eq!(count.get(), 2);

            let event = event_log.get(2).await.unwrap();
            assert_eq!(event, None);
            let event = event_log.get(0).await.unwrap();
            assert_eq!(event, Some(&event_a));
            assert_eq!(count.get(), 2);
        });
    }

    #[test]
    fn get_preserves_loaded_events_on_error() {
        let event_a = input(2);
        let event_b = input(1);

        let events = [Ok(event_a.clone()), Err(fmt::Error), Ok(event_b.clone())];
        let stream = stream::iter(events);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let error = event_log.get(1).await.unwrap_err();
            assert_eq!(error, fmt::Error);

            let event = event_log.get(0).await.unwrap();
            assert_eq!(event, Some(&event_a));
            let event = event_log.get(1).await.unwrap();
            assert_eq!(event, Some(&event_b));
            let event = event_log.get(2).await.unwrap();
            assert_eq!(event, None);
        });
    }

    #[test]
    fn get_preserves_loaded_events_when_cancelled() {
        let event_a = input(2);
        let event_b = input(1);

        let ready = Cell::new(true);
        let mut events = [event_a.clone(), event_b.clone()]
            .map(Ok::<_, fmt::Error>)
            .into_iter();
        let stream = stream::poll_fn(|_| {
            if ready.replace(false) {
                Poll::Ready(events.next())
            } else {
                Poll::Pending
            }
        });

        let mut event_log = EventLog::new(stream);

        let output = event_log.get(1).now_or_never();
        assert_eq!(output, None);

        let event = event_log.get(0).now_or_never().unwrap().unwrap();
        assert_eq!(event, Some(&event_a));

        ready.set(true);
        let event = event_log.get(1).now_or_never().unwrap().unwrap();
        assert_eq!(event, Some(&event_b));
    }

    #[test]
    fn is_turn_finished_returns_false_without_turn_end() {
        let cases = [
            vec![],
            vec![compaction(1)],
            vec![model_response(1, vec![], USAGE)],
        ];

        for events in cases {
            let stream = stream::iter(events.into_iter().map(Ok::<_, fmt::Error>));

            let mut event_log = EventLog::new(stream);

            futures_executor::block_on(async {
                let is_turn_finished = event_log.is_turn_finished().await.unwrap();
                assert!(!is_turn_finished);
            });
        }
    }

    #[test]
    fn is_turn_finished_returns_true_at_turn_end() {
        for payload in [TurnEnd::Completed, TurnEnd::Interrupted] {
            let event = turn_end(1, payload);

            let stream = stream::iter([Ok(event), Err(fmt::Error)]);

            let mut event_log = EventLog::new(stream);

            futures_executor::block_on(async {
                let is_turn_finished = event_log.is_turn_finished().await.unwrap();
                assert!(is_turn_finished);
            });
        }
    }

    #[test]
    fn is_turn_finished_skips_compactions() {
        let event_a = compaction(4);
        let event_b = compaction(3);
        let event_c = turn_end(2, TurnEnd::Completed);

        let stream = stream::iter([Ok(event_a), Ok(event_b), Ok(event_c), Err(fmt::Error)]);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let is_turn_finished = event_log.is_turn_finished().await.unwrap();
            assert!(is_turn_finished);
        });
    }

    #[test]
    fn is_turn_finished_returns_false_after_new_activity() {
        for event_a in [input(2), tool_result(2), model_response(2, vec![], USAGE)] {
            let event_b = turn_end(1, TurnEnd::Completed);

            let stream = stream::iter([event_a, event_b].map(Ok::<_, fmt::Error>));

            let mut event_log = EventLog::new(stream);

            futures_executor::block_on(async {
                let is_turn_finished = event_log.is_turn_finished().await.unwrap();
                assert!(!is_turn_finished);
            });
        }
    }

    #[test]
    fn is_turn_finished_propagates_stream_errors() {
        let event = compaction(2);

        let stream = stream::iter([Ok(event), Err(fmt::Error)]);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let error = event_log.is_turn_finished().await.unwrap_err();
            assert_eq!(error, fmt::Error);
        });
    }

    #[test]
    fn last_token_count_returns_none_without_model_responses() {
        let cases = [vec![], vec![input(2), tool_result(1)]];

        for events in cases {
            let stream = stream::iter(events.into_iter().map(Ok::<_, fmt::Error>));

            let mut event_log = EventLog::new(stream);

            futures_executor::block_on(async {
                let token_count = event_log.last_token_count().await.unwrap();
                assert_eq!(token_count, None);
            });
        }
    }

    #[test]
    fn last_token_count_uses_latest_model_response() {
        let event_a = model_response(2, vec![], USAGE);
        let event_b = model_response(
            1,
            vec![],
            ModelUsage {
                input: 200,
                output: 40,
                ..USAGE
            },
        );

        let stream = stream::iter([event_a, event_b].map(Ok::<_, fmt::Error>));

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let token_count = event_log.last_token_count().await.unwrap();
            assert_eq!(token_count, Some(USAGE.input + USAGE.output));
        });
    }

    #[test]
    fn last_token_count_skips_events_without_usage() {
        let event_a = input(4);
        let event_b = tool_result(3);
        let event_c = turn_end(2, TurnEnd::Completed);
        let event_d = model_response(1, vec![], USAGE);

        let stream = stream::iter([
            Ok(event_a),
            Ok(event_b),
            Ok(event_c),
            Ok(event_d),
            Err(fmt::Error),
        ]);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let token_count = event_log.last_token_count().await.unwrap();
            assert_eq!(token_count, Some(USAGE.input + USAGE.output));
        });
    }

    #[test]
    fn last_token_count_stops_at_compaction() {
        let event_a = input(4);
        let event_b = compaction(3);
        let event_c = model_response(2, vec![], USAGE);

        let stream = stream::iter([event_a, event_b, event_c].map(Ok::<_, fmt::Error>));

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let token_count = event_log.last_token_count().await.unwrap();
            assert_eq!(token_count, None);
        });
    }

    #[test]
    fn last_token_count_propagates_stream_errors() {
        let event = input(1);

        let stream = stream::iter([Ok(event), Err(fmt::Error)]);

        let mut event_log = EventLog::new(stream);

        futures_executor::block_on(async {
            let error = event_log.last_token_count().await.unwrap_err();
            assert_eq!(error, fmt::Error);
        });
    }
}
