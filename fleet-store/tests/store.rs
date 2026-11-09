//! Tests for persistent store operations.

use core::{assert_matches, pin::pin};
use fleet_core::{
    Agent, ClientStore as _, Compaction, Configured, ContentBlock, EventPayload, JsonObject,
    Message, MessageMode, ModelOutput, ModelProvenance, ModelProviderToolCall, ModelReasoning,
    ModelResponse, ModelUsage, RuntimeStore as _, ToolCall, ToolOutput, ToolResult, TurnEnd, Wake,
};
use fleet_store::{Store, StoreError};
use futures_util::{StreamExt as _, TryStreamExt as _};

fn agent() -> Agent {
    Agent {
        id: u128::MAX.into(),
        role: Configured {
            name: "assistant".into(),
            config: JsonObject::new(),
        },
        extensions: JsonObject::new(),
    }
}

fn message(id: u128, mode: MessageMode) -> Message {
    Message {
        id: id.into(),
        mode,
        content: vec![ContentBlock::Text("Hello".into())],
    }
}

fn response(output: Vec<ModelOutput>) -> ModelResponse {
    ModelResponse {
        output,
        usage: ModelUsage {
            input: 8,
            output: 4,
            cache_read: 2,
            cache_write: 1,
        },
        provenance: ModelProvenance {
            provider_id: "provider".into(),
            model_id: "model".into(),
        },
        continuation_token: Some("continuation".into()),
    }
}

fn tool_call(id: &str, is_async: bool) -> ToolCall {
    ToolCall {
        id: id.into(),
        tool: "shell".into(),
        is_async,
        input: JsonObject::new(),
    }
}

fn tool_result(id: &str) -> ToolResult {
    ToolResult {
        call_id: id.into(),
        output: ToolOutput {
            content: vec![ContentBlock::Text("Done".into())],
            structured_content: None,
            is_error: false,
        },
    }
}

#[test]
fn open_preserves_records_and_idempotency() {
    let path = std::env::temp_dir().join(format!("fleet-store-{}.redb", std::process::id()));
    let agent = agent();
    let message = message(u128::MAX, MessageMode::Steer);
    let response = response(vec![
        ModelOutput::Reasoning(ModelReasoning {
            content: Some("Thinking".into()),
            redacted_content: Some("opaque".into()),
            attestation: Some("signature".into()),
        }),
        ModelOutput::ProviderToolCall(ModelProviderToolCall {
            call_id: "search-1".into(),
            name: "search".into(),
            content: Some("Found results".into()),
            payload: "opaque call".into(),
        }),
        ModelOutput::Text("Hello".into()),
        ModelOutput::ToolCall(tool_call("async", true)),
    ]);

    futures_executor::block_on(async {
        {
            let store = Store::open(&path).unwrap();
            store
                .create_agent("create", &agent, Some(&message))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .append_model_response(agent.id, 0.into(), true, &response)
                .await
                .unwrap();
            let prepared = store
                .prepare_tool_call(agent.id, &"async".into())
                .await
                .unwrap();
            assert!(!prepared);
        }

        let store = Store::open(&path).unwrap();
        store
            .create_agent("create", &agent, Some(&message))
            .await
            .unwrap();
        let prepared = store
            .prepare_tool_call(agent.id, &"async".into())
            .await
            .unwrap();
        assert!(prepared);
        let stored_agent = store.agent(agent.id).await.unwrap().unwrap();
        assert_eq!(stored_agent, agent);
        let events = store
            .events(agent.id, 2.into(), None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].payload, EventPayload::ModelResponse(response));
        assert_eq!(
            events[2].payload,
            EventPayload::Input(message.id, message.content)
        );

        let error = store
            .prepare_tool_call(agent.id, &"search-1".into())
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));

        let mut wakes = pin!(store.wakes());
        let wake = wakes.next().await.unwrap().unwrap();
        assert_eq!(wake, Wake::Reconcile(agent.id));
        let wake = wakes.next().await.unwrap().unwrap();
        assert_eq!(wake, Wake::Idle);
    });

    std::fs::remove_file(path).unwrap();
}

#[test]
fn consume_messages_prioritizes_steering_and_preserves_queue_order() {
    let store = Store::memory().unwrap();
    let agent = agent();
    let queued_a = message(1, MessageMode::Queue);
    let queued_b = message(2, MessageMode::Queue);
    let steering_a = message(3, MessageMode::Steer);
    let steering_b = message(4, MessageMode::Steer);

    futures_executor::block_on(async {
        store
            .create_agent("create", &agent, Some(&queued_a))
            .await
            .unwrap();
        store
            .send_message("steer-a", agent.id, &steering_a)
            .await
            .unwrap();
        store
            .send_message("queue", agent.id, &queued_b)
            .await
            .unwrap();
        store
            .send_message("steer-b", agent.id, &steering_b)
            .await
            .unwrap();
        let error = store
            .consume_messages(agent.id, Some(0.into()), true)
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));

        let consumed = store.consume_messages(agent.id, None, true).await.unwrap();
        assert!(consumed);
        let consumed = store
            .consume_messages(agent.id, Some(1.into()), false)
            .await
            .unwrap();
        assert!(!consumed);
        let consumed = store
            .consume_messages(agent.id, Some(1.into()), true)
            .await
            .unwrap();
        assert!(consumed);
        let consumed = store
            .consume_messages(agent.id, Some(2.into()), true)
            .await
            .unwrap();
        assert!(consumed);
        let consumed = store
            .consume_messages(agent.id, Some(3.into()), true)
            .await
            .unwrap();
        assert!(!consumed);
        let events = store
            .events(agent.id, 3.into(), None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let inputs = events
            .into_iter()
            .map(|event| event.payload)
            .collect::<Vec<_>>();
        assert_eq!(
            inputs,
            vec![
                EventPayload::Input(queued_b.id, queued_b.content),
                EventPayload::Input(queued_a.id, queued_a.content),
                EventPayload::Input(steering_b.id, steering_b.content),
                EventPayload::Input(steering_a.id, steering_a.content),
            ]
        );
    });
}

#[test]
fn append_model_response_rejects_duplicate_calls_atomically() {
    let store = Store::memory().unwrap();
    let agent = agent();
    let message = message(1, MessageMode::Steer);
    let call = tool_call("sync", false);
    let invalid = response(vec![
        ModelOutput::ToolCall(call.clone()),
        ModelOutput::ToolCall(call.clone()),
    ]);
    let valid = response(vec![ModelOutput::ToolCall(call)]);

    futures_executor::block_on(async {
        store
            .create_agent("create", &agent, Some(&message))
            .await
            .unwrap();
        store.consume_messages(agent.id, None, true).await.unwrap();
        let error = store
            .append_model_response(agent.id, 0.into(), false, &invalid)
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));
        let state = store.agent_state(agent.id).await.unwrap().unwrap();
        assert_eq!(state.latest_event, Some(0.into()));

        store
            .append_model_response(agent.id, 0.into(), false, &valid)
            .await
            .unwrap();
        let error = store
            .append_model_response(agent.id, 1.into(), true, &response(vec![]))
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));
        store
            .append_tool_result(agent.id, &tool_result("sync"))
            .await
            .unwrap();
        let error = store
            .append_tool_result(agent.id, &tool_result("sync"))
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));
        store
            .append_model_response(agent.id, 2.into(), true, &response(vec![]))
            .await
            .unwrap();
    });
}

#[test]
fn append_compaction_validates_calls_at_its_boundary() {
    let store = Store::memory().unwrap();
    let agent = agent();
    let message = message(1, MessageMode::Steer);
    let call = tool_call("async", true);
    let response = response(vec![ModelOutput::ToolCall(call.clone())]);
    let compaction = Compaction {
        last_event: 1.into(),
        input: vec![],
        pending_tool_calls: vec![call],
        usage: response.usage,
    };

    futures_executor::block_on(async {
        store
            .create_agent("create", &agent, Some(&message))
            .await
            .unwrap();
        store.consume_messages(agent.id, None, true).await.unwrap();
        store
            .append_model_response(agent.id, 0.into(), false, &response)
            .await
            .unwrap();
        store
            .append_tool_result(agent.id, &tool_result("async"))
            .await
            .unwrap();
        store.park(agent.id, Some(2.into())).await.unwrap();
        store
            .append_compaction(agent.id, 2.into(), &compaction)
            .await
            .unwrap();
        let state = store.agent_state(agent.id).await.unwrap().unwrap();
        assert_eq!(state.latest_event, Some(3.into()));
        assert_eq!(state.parked_at, state.latest_event);

        let invalid = Compaction {
            pending_tool_calls: vec![],
            ..compaction
        };
        let error = store
            .append_compaction(agent.id, 3.into(), &invalid)
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));
    });
}

#[test]
fn settle_interrupt_requires_all_pending_results() {
    let store = Store::memory().unwrap();
    let agent = agent();
    let message = message(1, MessageMode::Steer);
    let response = response(vec![
        ModelOutput::ToolCall(tool_call("a", false)),
        ModelOutput::ToolCall(tool_call("b", true)),
    ]);

    futures_executor::block_on(async {
        store
            .create_agent("create", &agent, Some(&message))
            .await
            .unwrap();
        store.consume_messages(agent.id, None, true).await.unwrap();
        store
            .append_model_response(agent.id, 0.into(), false, &response)
            .await
            .unwrap();
        store
            .interrupt_agent("interrupt", agent.id, None)
            .await
            .unwrap();
        let error = store
            .settle_interrupt(agent.id, Some(1.into()), &[tool_result("a")])
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));
        let state = store.agent_state(agent.id).await.unwrap().unwrap();
        assert!(state.interrupt_intent);
        assert_eq!(state.latest_event, Some(1.into()));

        store
            .settle_interrupt(
                agent.id,
                Some(1.into()),
                &[tool_result("a"), tool_result("b")],
            )
            .await
            .unwrap();
        let state = store.agent_state(agent.id).await.unwrap().unwrap();
        assert!(!state.interrupt_intent);
        assert_eq!(state.latest_event, Some(4.into()));
        assert_eq!(state.parked_at, state.latest_event);
        let events = store
            .events(agent.id, 4.into(), Some(3.into()))
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            events[0].payload,
            EventPayload::TurnEnd(TurnEnd::Interrupted)
        );
    });
}

#[test]
fn wakes_deliver_interrupts_before_idle() {
    let store = Store::memory().unwrap();
    let agent = agent();
    let message = message(1, MessageMode::Steer);
    let mut wakes = pin!(store.wakes());

    futures_executor::block_on(async {
        store.create_agent("create", &agent, None).await.unwrap();
        let wake = wakes.next().await.unwrap().unwrap();
        assert_eq!(wake, Wake::Idle);
        store
            .send_message("send", agent.id, &message)
            .await
            .unwrap();
        let wake = wakes.next().await.unwrap().unwrap();
        assert_eq!(wake, Wake::Reconcile(agent.id));
        store
            .interrupt_agent("interrupt", agent.id, None)
            .await
            .unwrap();
        store
            .send_message(
                "send again",
                agent.id,
                &Message {
                    id: 2.into(),
                    ..message
                },
            )
            .await
            .unwrap();
        let wake = wakes.next().await.unwrap().unwrap();
        assert_eq!(wake, Wake::Interrupt(agent.id));
        let wake = wakes.next().await.unwrap().unwrap();
        assert_eq!(wake, Wake::Idle);
    });
}
