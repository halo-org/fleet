//! Tests for resumable store queries.

#![cfg(feature = "queries")]

use core::{assert_matches, pin::pin};
use fleet_core::{
    Agent, ClientStore as _, Compaction, Configured, ContentBlock, JsonObject, Message,
    MessageMode, ModelOutput, ModelProvenance, ModelResponse, ModelResponseChunk, ModelUsage,
    RuntimeStore as _, ToolCall, ToolOutput, ToolResult, TurnEnd,
};
use fleet_kit::{MessageRecord, Preview, Queries as _, QueryUpdate};
use fleet_store::Store;
use futures_util::{TryStreamExt as _, future};

fn agent() -> Agent {
    Agent {
        id: 1.into(),
        role: Configured {
            name: "assistant".into(),
            config: JsonObject::new(),
        },
        extensions: JsonObject::new(),
    }
}

fn message(id: u128) -> Message {
    Message {
        id: id.into(),
        mode: MessageMode::Queue,
        content: vec![ContentBlock::Text("Hello".into())],
    }
}

fn response(output: Vec<ModelOutput>) -> ModelResponse {
    ModelResponse {
        output,
        usage: ModelUsage {
            input: 1,
            output: 1,
            cache_read: 0,
            cache_write: 0,
        },
        provenance: ModelProvenance {
            provider_id: "provider".into(),
            model_id: "model".into(),
        },
        continuation_token: None,
    }
}

#[test]
fn wait_for_turn_end_ignores_pending_messages_and_async_tools() {
    let store = Store::memory().unwrap();
    let agent = agent();
    let message_a = message(1);
    let message_b = message(2);
    let response = response(vec![ModelOutput::ToolCall(ToolCall {
        id: "async".into(),
        tool: "shell".into(),
        is_async: true,
        input: JsonObject::new(),
    })]);

    futures_executor::block_on(async {
        store
            .create_agent("create", &agent, Some(&message_a))
            .await
            .unwrap();
        store.consume_messages(agent.id, None, true).await.unwrap();
        store
            .send_message("send", agent.id, &message_b)
            .await
            .unwrap();
        let (turn_end, ()) = future::join(store.wait_for_turn_end(agent.id, message_a.id), async {
            store
                .append_model_response(agent.id, 0.into(), true, &response)
                .await
                .unwrap();
        })
        .await;
        let turn_end = turn_end.unwrap();
        assert_eq!(turn_end.cursor, 2.into());
        assert_eq!(turn_end.payload, TurnEnd::Completed);

        store
            .append_tool_result(
                agent.id,
                &ToolResult {
                    call_id: "async".into(),
                    output: ToolOutput {
                        content: vec![],
                        structured_content: None,
                        is_error: false,
                    },
                },
            )
            .await
            .unwrap();
        let repeated = store
            .wait_for_turn_end(agent.id, message_a.id)
            .await
            .unwrap();
        assert_eq!(repeated, turn_end);
        let pending = store.messages(agent.id, None).await.unwrap();
        assert_eq!(
            pending.value,
            vec![MessageRecord::Pending {
                position: 1,
                message: message_b
            }]
        );
    });
}

#[test]
fn compaction_notifications_are_transient_and_superseded() {
    let store = Store::memory().unwrap();
    let agent = agent();
    let response = response(vec![]);
    let compaction = Compaction {
        last_event: 2.into(),
        input: vec![],
        pending_tool_calls: vec![],
        usage: response.usage,
    };

    futures_executor::block_on(async {
        store
            .create_agent("create", &agent, Some(&message(1)))
            .await
            .unwrap();
        store.consume_messages(agent.id, None, true).await.unwrap();
        store
            .append_model_response(agent.id, 0.into(), true, &response)
            .await
            .unwrap();
        let state = store.agent_state(agent.id).await.unwrap();
        let activity = store.activity(agent.id, None).await.unwrap();
        let mut previews = pin!(store.previews(agent.id));
        let preview = previews.try_next().await.unwrap().unwrap();
        assert_eq!(preview, None);

        store
            .publish_compaction_started(agent.id, 2.into())
            .await
            .unwrap();
        let preview = previews.try_next().await.unwrap().unwrap();
        assert_eq!(
            preview,
            Some(Preview::Compaction {
                input_cursor: 2.into()
            })
        );
        store
            .publish_compaction_started(agent.id, 2.into())
            .await
            .unwrap();
        let repeated = pin!(store.previews(agent.id))
            .try_next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeated, preview);
        let current_state = store.agent_state(agent.id).await.unwrap();
        let current_activity = store.activity(agent.id, None).await.unwrap();
        assert_eq!(current_state, state);
        assert_eq!(current_activity, activity);

        store
            .append_compaction(agent.id, 2.into(), &compaction)
            .await
            .unwrap();
        store
            .publish_compaction_started(agent.id, 2.into())
            .await
            .unwrap();
        let preview = previews.try_next().await.unwrap().unwrap();
        assert_eq!(preview, None);

        store
            .publish_compaction_started(agent.id, 3.into())
            .await
            .unwrap();
        let preview = previews.try_next().await.unwrap().unwrap();
        assert_eq!(
            preview,
            Some(Preview::Compaction {
                input_cursor: 3.into()
            })
        );
        store
            .interrupt_agent("interrupt", agent.id, None)
            .await
            .unwrap();
        store
            .publish_compaction_started(agent.id, 3.into())
            .await
            .unwrap();
        let preview = previews.try_next().await.unwrap().unwrap();
        assert_eq!(preview, None);
    });
}

#[test]
fn compaction_notifications_preserve_response_sequence_invalidation() {
    let store = Store::memory().unwrap();
    let agent = agent();
    let chunk = ModelResponseChunk::TextStart(0);

    futures_executor::block_on(async {
        store
            .create_agent("create", &agent, Some(&message(1)))
            .await
            .unwrap();
        store.consume_messages(agent.id, None, true).await.unwrap();
        store
            .publish_model_response_chunk(agent.id, 0.into(), 0, &chunk)
            .await
            .unwrap();
        let mut previews = pin!(store.previews(agent.id));
        let preview = previews.try_next().await.unwrap().unwrap().unwrap();
        assert_matches!(preview, Preview::Response { .. });

        for _ in 0..2 {
            store
                .publish_compaction_started(agent.id, 0.into())
                .await
                .unwrap();
            let preview = previews.try_next().await.unwrap().unwrap();
            assert_eq!(
                preview,
                Some(Preview::Compaction {
                    input_cursor: 0.into()
                })
            );
            store
                .publish_model_response_chunk(agent.id, 0.into(), 0, &chunk)
                .await
                .unwrap();
            let preview = previews.try_next().await.unwrap().unwrap();
            assert_eq!(preview, None);
        }
    });
}

#[test]
fn updates_resume_complete_revisions_after_reopening() {
    let path =
        std::env::temp_dir().join(format!("fleet-store-queries-{}.redb", std::process::id()));
    let agent = agent();
    let message_a = message(1);
    let message_b = message(2);

    futures_executor::block_on(async {
        {
            let store = Store::open(&path).unwrap();
            store
                .create_agent("create", &agent, Some(&message_a))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .append_model_response(agent.id, 0.into(), true, &response(vec![]))
                .await
                .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let consumed = store.updates(agent.id, 1.into()).await.unwrap();
        assert_eq!(consumed.revision, 2.into());
        assert_eq!(
            consumed.messages,
            vec![MessageRecord::Consumed(message_a.id)]
        );
        assert_eq!(consumed.events.len(), 1);
        assert_eq!(consumed.activity, QueryUpdate::Unchanged);
        let completed = store.updates(agent.id, consumed.revision).await.unwrap();
        assert_eq!(completed.revision, 3.into());
        assert_eq!(completed.events.len(), 2);

        let (updates, ()) = future::join(store.updates(agent.id, completed.revision), async {
            store
                .send_message("send", agent.id, &message_b)
                .await
                .unwrap();
        })
        .await;
        let updates = updates.unwrap();
        assert_eq!(updates.revision, 4.into());
        assert_eq!(
            updates.messages,
            vec![MessageRecord::Pending {
                position: 1,
                message: message_b
            }]
        );
        let repeated = store.updates(agent.id, completed.revision).await.unwrap();
        assert_eq!(repeated, updates);
    });

    std::fs::remove_file(path).unwrap();
}
