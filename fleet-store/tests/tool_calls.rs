//! Tests for access to an agent's own tool calls.

#![cfg(test)]
#![cfg(feature = "tool-calls")]

use core::assert_matches;
use fleet_core::{
    Agent, ClientStore as _, Compaction, Configured, ContentBlock, JsonObject, Message,
    MessageMode, ModelOutput, ModelProvenance, ModelResponse, ModelUsage, RuntimeStore as _,
    ToolCall, ToolCallId, ToolOutput, ToolResult,
};
use fleet_kit::MessageCounts;
use fleet_store::{Store, StoreError};
use futures_util::future;

fn message(id: u128, mode: MessageMode) -> Message {
    Message {
        id: id.into(),
        mode,
        content: vec![ContentBlock::Text("Hello".into())],
    }
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        tool: "task".into(),
        is_async: true,
        input: JsonObject::new(),
    }
}

const fn usage() -> ModelUsage {
    ModelUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    }
}

fn response(calls: &[&str]) -> ModelResponse {
    ModelResponse {
        output: calls
            .iter()
            .map(|id| ModelOutput::ToolCall(call(id)))
            .collect(),
        usage: usage(),
        provenance: ModelProvenance {
            provider_id: "provider".into(),
            model_id: "model".into(),
        },
        continuation_token: None,
    }
}

fn result(id: &str) -> ToolResult {
    ToolResult {
        call_id: id.into(),
        output: ToolOutput {
            content: vec![],
            structured_content: None,
            is_error: false,
        },
    }
}

/// Creates an agent whose first response calls `drive` and the given calls.
async fn agent(store: &Store, calls: &[&str]) {
    let agent = Agent {
        id: 1.into(),
        role: Configured {
            name: "assistant".into(),
            config: JsonObject::new(),
        },
        extensions: JsonObject::new(),
    };
    store
        .create_agent("create", &agent, Some(&message(1, MessageMode::Queue)))
        .await
        .unwrap();
    store.consume_messages(agent.id, None, true).await.unwrap();
    let calls = [&["drive"], calls].concat();
    store
        .append_model_response(agent.id, 0.into(), false, &response(&calls))
        .await
        .unwrap();
}

#[test]
fn state_lists_calls_in_context_since_the_latest_compaction() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        agent(&store, &["a", "b"]).await;
        store
            .append_tool_result(1.into(), &result("a"))
            .await
            .unwrap();
        store
            .append_tool_result(1.into(), &result("drive"))
            .await
            .unwrap();
        store
            .append_model_response(1.into(), 3.into(), false, &response(&["c"]))
            .await
            .unwrap();
        store
            .append_model_response(1.into(), 4.into(), false, &response(&["d"]))
            .await
            .unwrap();
        store
            .append_compaction(
                1.into(),
                5.into(),
                &Compaction {
                    last_event: 4.into(),
                    input: vec![],
                    pending_tool_calls: vec![call("b"), call("c")],
                    usage: usage(),
                },
            )
            .await
            .unwrap();
        store
            .append_model_response(1.into(), 6.into(), false, &response(&["e", "f"]))
            .await
            .unwrap();
        store
            .append_model_response(1.into(), 7.into(), false, &response(&["g"]))
            .await
            .unwrap();

        let state = store
            .tool_call_access(1.into(), "e".into())
            .state(None)
            .await
            .unwrap();
        assert_eq!(
            state.context,
            [call("b"), call("c"), call("d"), call("e"), call("f")]
        );
        assert_eq!(
            state.pending_tool_calls,
            [call("b"), call("c"), call("d"), call("f"), call("g")]
        );
    });
}

#[test]
fn state_counts_messages_since_the_driving_call() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        agent(&store, &[]).await;
        let access = store.tool_call_access(1.into(), "drive".into());

        let state = access.state(None).await.unwrap();
        assert_eq!(state.messages, MessageCounts::default());

        store
            .send_message("steer", 1.into(), &message(2, MessageMode::Steer))
            .await
            .unwrap();
        store
            .send_message("queue", 1.into(), &message(3, MessageMode::Queue))
            .await
            .unwrap();
        let latest = store
            .agent_state(1.into())
            .await
            .unwrap()
            .unwrap()
            .latest_event;
        store
            .consume_messages(1.into(), latest, false)
            .await
            .unwrap();

        let state = access.state(None).await.unwrap();
        assert_eq!(
            state.messages,
            MessageCounts {
                steering: 1,
                queued: 1,
            }
        );
    });
}

#[test]
fn state_waits_for_a_newer_revision() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        agent(&store, &[]).await;
        let access = store.tool_call_access(1.into(), "drive".into());
        let current = access.state(None).await.unwrap();

        let (state, ()) = future::join(access.state(Some(current.revision)), async {
            store
                .send_message("steer", 1.into(), &message(2, MessageMode::Steer))
                .await
                .unwrap();
        })
        .await;
        let state = state.unwrap();
        assert!(state.revision > current.revision);
        assert_eq!(state.messages.steering, 1);
    });
}

#[test]
fn interrupt_targets_other_calls_of_the_agent() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        agent(&store, &["a"]).await;
        let access = store.tool_call_access(1.into(), "drive".into());

        let error = access
            .interrupt("self", &["drive".into()])
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));

        access.interrupt("stop", &["a".into()]).await.unwrap();
        access.interrupt("stop", &["a".into()]).await.unwrap();
        let error = access.interrupt("stop", &[]).await.unwrap_err();
        assert_matches!(error, StoreError::IdempotencyConflict);
        let state = store.agent_state(1.into()).await.unwrap().unwrap();
        assert_eq!(
            state
                .tool_call_interrupt_intents
                .into_iter()
                .collect::<Vec<_>>(),
            [ToolCallId::from("a")]
        );
    });
}

#[test]
fn access_closes_once_the_driving_call_is_interrupted() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        agent(&store, &["a"]).await;
        let access = store.tool_call_access(1.into(), "drive".into());
        access.interrupt("stop", &["a".into()]).await.unwrap();
        let before = access.state(None).await.unwrap().revision;

        store
            .interrupt_tool_calls("stop", 1.into(), &["drive".into()])
            .await
            .unwrap();
        let error = access.state(Some(before)).await.unwrap_err();
        assert_matches!(error, StoreError::ScopeClosed);
        access.interrupt("stop", &["a".into()]).await.unwrap();
        let error = access.interrupt("stop", &[]).await.unwrap_err();
        assert_matches!(error, StoreError::IdempotencyConflict);
        let error = access.interrupt("again", &["a".into()]).await.unwrap_err();
        assert_matches!(error, StoreError::ScopeClosed);

        store
            .append_tool_result(1.into(), &result("drive"))
            .await
            .unwrap();
        let current = fleet_kit::Queries::activity(&store, 1.into(), None)
            .await
            .unwrap()
            .revision;
        let error = access.state(Some(current)).await.unwrap_err();
        assert_matches!(error, StoreError::ScopeClosed);
    });
}
