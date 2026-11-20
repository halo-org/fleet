//! Tests for execution owned by driving tool calls.

#![cfg(test)]
#![cfg(feature = "subagents")]

use core::assert_matches;
use fleet_core::{
    Agent, AgentId, ClientStore as _, Configured, ContentBlock, JsonObject, Message, MessageMode,
    ModelOutput, ModelProvenance, ModelResponse, ModelUsage, RuntimeStore as _, ToolCall,
    ToolOutput, ToolResult, TurnEnd,
};
use fleet_kit::{Agents as _, EventQuery, MessageRecord, Queries as _};
use fleet_store::{Store, StoreError, WorkspaceRelease};

fn role() -> Configured<fleet_core::RoleName> {
    Configured {
        name: "assistant".into(),
        config: JsonObject::new(),
    }
}

fn call(id: &str) -> ModelOutput {
    ModelOutput::ToolCall(ToolCall {
        id: id.into(),
        tool: "drive".into(),
        is_async: true,
        input: JsonObject::new(),
    })
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
            provider_id: "test".into(),
            model_id: "test".into(),
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

async fn root(store: &Store) {
    let agent = Agent {
        id: 1.into(),
        role: role(),
        extensions: JsonObject::new(),
    };
    let message = Message {
        id: 1.into(),
        mode: MessageMode::Queue,
        content: vec![],
    };
    store
        .create_agent("root", &agent, Some(&message))
        .await
        .unwrap();
    store.consume_messages(agent.id, None, true).await.unwrap();
    store
        .append_model_response(
            agent.id,
            0.into(),
            true,
            &response(vec![call("drive"), call("other")]),
        )
        .await
        .unwrap();
}

async fn released(store: &Store) -> Vec<AgentId> {
    let (due, _) = store.workspace_releases(0, None).await.unwrap();
    due.iter().map(WorkspaceRelease::agent).collect()
}

async fn answer(store: &Store, id: AgentId, output: Vec<ModelOutput>) {
    let state = store.agent_state(id).await.unwrap().unwrap();
    store
        .consume_messages(id, state.latest_event, true)
        .await
        .unwrap();
    let state = store.agent_state(id).await.unwrap().unwrap();
    store
        .append_model_response(id, state.latest_event.unwrap(), true, &response(output))
        .await
        .unwrap();
    let state = store.agent_state(id).await.unwrap().unwrap();
    store.park(id, state.latest_event).await.unwrap();
}

#[test]
fn responses_keep_background_work_until_the_driving_call_ends() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        root(&store).await;
        let access = store.agent_access(1.into()).for_call("drive".into());
        let child = access
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        let first = access.send("first", child, &[]).await.unwrap();
        answer(
            &store,
            child,
            vec![call("server"), ModelOutput::Text("Ready".into())],
        )
        .await;

        let end = access.wait_for_turn_end(child, first).await.unwrap();
        assert_eq!(end.payload, TurnEnd::Completed);
        let remaining = store.reconcile_agent_scopes().await.unwrap();
        let state = store.agent_state(child).await.unwrap().unwrap();
        assert_eq!(remaining, 1);
        assert!(!state.interrupt_intent);

        let second = access
            .send("second", child, &[ContentBlock::Text("Next".into())])
            .await
            .unwrap();
        answer(&store, child, vec![ModelOutput::Text("Done".into())]).await;
        let end = access.wait_for_turn_end(child, second).await.unwrap();
        assert_eq!(end.payload, TurnEnd::Completed);
        store
            .append_tool_result(1.into(), &result("drive"))
            .await
            .unwrap();
        store.reconcile_agent_scopes().await.unwrap();

        let state = store.agent_state(child).await.unwrap().unwrap();
        assert!(state.interrupt_intent);
        assert_eq!(released(&store).await, []);
        store
            .settle_interrupt(child, state.latest_event, &[result("server")])
            .await
            .unwrap();
        let remaining = store.reconcile_agent_scopes().await.unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(released(&store).await, [child]);
        let repeated = access
            .send("second", child, &[ContentBlock::Text("Next".into())])
            .await
            .unwrap();
        assert_eq!(repeated, second);
    });
}

#[test]
fn closure_after_a_completed_turn_does_not_interrupt() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        root(&store).await;
        let access = store.agent_access(1.into()).for_call("drive".into());
        let child = access
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        access.send("start", child, &[]).await.unwrap();
        store.consume_messages(child, None, true).await.unwrap();
        store
            .append_model_response(
                child,
                0.into(),
                true,
                &response(vec![ModelOutput::Text("Done".into())]),
            )
            .await
            .unwrap();

        store
            .append_tool_result(1.into(), &result("drive"))
            .await
            .unwrap();
        let remaining = store.reconcile_agent_scopes().await.unwrap();
        let state = store.agent_state(child).await.unwrap().unwrap();
        assert_eq!(remaining, 0);
        assert!(!state.interrupt_intent);
        assert_eq!(released(&store).await, [child]);
    });
}

#[test]
fn closure_withdraws_input_and_fences_late_nested_work() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        root(&store).await;
        let access = store.agent_access(1.into()).for_call("drive".into());
        let child = access
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        access.send("start", child, &[]).await.unwrap();
        answer(&store, child, vec![call("nested")]).await;
        let nested = store.agent_access(child).for_call("nested".into());
        let grandchild = nested
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        let pending = nested.send("queued", grandchild, &[]).await.unwrap();

        let revision = store.messages(grandchild, None).await.unwrap().revision;
        store.interrupt_agent("stop", 1.into(), None).await.unwrap();
        let error = nested
            .create("late", &role(), &JsonObject::new())
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::ScopeClosed);
        store.reconcile_agent_scopes().await.unwrap();
        store.reconcile_agent_scopes().await.unwrap();
        let messages = store.messages(grandchild, Some(revision)).await.unwrap();
        let error = nested
            .wait_for_turn_end(grandchild, pending)
            .await
            .unwrap_err();
        let state = store.agent_state(grandchild).await.unwrap().unwrap();
        assert_eq!(messages.value, [MessageRecord::Deleted(pending)]);
        assert_matches!(error, StoreError::MessageNotFound);
        assert_eq!(state.parked_at, state.latest_event);
    });
}

#[test]
fn only_the_creating_call_can_send() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        root(&store).await;
        let first = store.agent_access(1.into()).for_call("drive".into());
        let second = store.agent_access(1.into()).for_call("other".into());
        let child = first
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        let message = first.send("start", child, &[]).await.unwrap();
        let error = second.send("start", child, &[]).await.unwrap_err();
        assert_matches!(error, StoreError::AgentDenied);

        answer(&store, child, vec![ModelOutput::Text("Done".into())]).await;
        store
            .append_tool_result(1.into(), &result("drive"))
            .await
            .unwrap();
        assert_eq!(store.reconcile_agent_scopes().await.unwrap(), 0);
        let repeated = first.send("start", child, &[]).await.unwrap();
        assert_eq!(repeated, message);
        let error = first.send("late", child, &[]).await.unwrap_err();
        assert_matches!(error, StoreError::ScopeClosed);
        let error = second.send("late", child, &[]).await.unwrap_err();
        assert_matches!(error, StoreError::AgentDenied);
    });
}

#[test]
fn metadata_and_history_have_separate_authority() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        root(&store).await;
        let access = store.agent_access(1.into()).for_call("drive".into());
        let child = access
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        let other = Agent {
            id: 2.into(),
            role: role(),
            extensions: JsonObject::new(),
        };
        store.create_agent("unrelated", &other, None).await.unwrap();
        let metadata = store.agent_access(child);
        let parent = metadata.get(1.into()).await.unwrap();
        assert_eq!(parent.id, 1.into());
        let error = metadata.get(other.id).await.unwrap_err();
        assert_matches!(error, StoreError::AgentDenied);
        let error = metadata
            .events(1.into(), EventQuery::default())
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::ScopeClosed);
        let error = metadata
            .for_call("fake".into())
            .events(1.into(), EventQuery::default())
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::AgentDenied);
        let message = Message {
            id: 2.into(),
            mode: MessageMode::Queue,
            content: vec![],
        };
        let error = store
            .send_message("unscoped", child, &message)
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::AgentDenied);
    });
}

#[test]
fn waiting_skips_unsolicited_turns_and_preserves_interruptions() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        root(&store).await;
        let access = store.agent_access(1.into()).for_call("drive".into());
        let child = access
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        access.send("start", child, &[]).await.unwrap();
        answer(&store, child, vec![call("background")]).await;
        store
            .append_tool_result(child, &result("background"))
            .await
            .unwrap();
        let queued = access.send("queued", child, &[]).await.unwrap();
        let state = store.agent_state(child).await.unwrap().unwrap();
        store
            .append_model_response(
                child,
                state.latest_event.unwrap(),
                true,
                &response(vec![ModelOutput::Text("Unsolicited".into())]),
            )
            .await
            .unwrap();
        let unsolicited = store
            .agent_state(child)
            .await
            .unwrap()
            .unwrap()
            .latest_event
            .unwrap();

        store
            .consume_messages(child, Some(unsolicited), true)
            .await
            .unwrap();
        store
            .interrupt_agent("interrupt", child, None)
            .await
            .unwrap();
        let state = store.agent_state(child).await.unwrap().unwrap();
        store
            .settle_interrupt(child, state.latest_event, &[])
            .await
            .unwrap();
        let interrupted = access.wait_for_turn_end(child, queued).await.unwrap();
        assert!(interrupted.cursor > unsolicited);
        assert_eq!(interrupted.payload, TurnEnd::Interrupted);
        access.send("later", child, &[]).await.unwrap();
        answer(&store, child, vec![ModelOutput::Text("Later".into())]).await;
        let repeated = access.wait_for_turn_end(child, queued).await.unwrap();
        assert_eq!(repeated, interrupted);
    });
}

#[test]
fn restart_recovers_closed_scopes_and_operation_receipts() {
    let path = std::env::temp_dir().join(format!("fleet-scopes-{}.redb", std::process::id()));
    futures_executor::block_on(async {
        let (child, message) = {
            let store = Store::open(&path).unwrap();
            root(&store).await;
            let access = store.agent_access(1.into()).for_call("drive".into());
            let child = access
                .create("child", &role(), &JsonObject::new())
                .await
                .unwrap();
            let other = access
                .create("other", &role(), &JsonObject::new())
                .await
                .unwrap();
            assert_ne!(child, other);
            let message = access.send("queued", child, &[]).await.unwrap();
            store
                .append_tool_result(1.into(), &result("drive"))
                .await
                .unwrap();
            (child, message)
        };
        let store = Store::open(&path).unwrap();
        let remaining = store.reconcile_agent_scopes().await.unwrap();
        assert_eq!(remaining, 0);
        let access = store.agent_access(1.into()).for_call("drive".into());
        let repeated = access
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        assert_eq!(repeated, child);
        let error = access.wait_for_turn_end(child, message).await.unwrap_err();
        assert_matches!(error, StoreError::MessageNotFound);
        let error = access
            .send("queued", child, &[ContentBlock::Text("Different".into())])
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::IdempotencyConflict);
    });
    std::fs::remove_file(path).unwrap();
}

#[cfg(feature = "blobs")]
#[test]
fn queued_images_require_the_callers_grant() {
    use fleet_kit::Blobs as _;
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        root(&store).await;
        let access = store.agent_access(1.into()).for_call("drive".into());
        let child = access
            .create("child", &role(), &JsonObject::new())
            .await
            .unwrap();
        let image = include_bytes!("fixtures/image.png");
        store.put(1.into(), "image/png", image).await.unwrap();
        let content = [ContentBlock::Image(1.into())];
        let error = access.send("image", child, &content).await.unwrap_err();
        assert_matches!(error, StoreError::BlobDenied);
        store
            .send_message(
                "attach",
                1.into(),
                &Message {
                    id: 2.into(),
                    mode: MessageMode::Queue,
                    content: content.to_vec(),
                },
            )
            .await
            .unwrap();
        access.send("image", child, &content).await.unwrap();
        let bytes = store.agent_blobs(child).read(1.into(), 1000).await.unwrap();
        assert_eq!(bytes, image);
    });
}

#[cfg(feature = "blobs")]
#[test]
fn calls_can_use_images_held_by_their_agents() {
    use fleet_kit::Blobs as _;
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        root(&store).await;
        let access = store.agent_access(1.into()).for_call("drive".into());
        let first = access
            .create("first", &role(), &JsonObject::new())
            .await
            .unwrap();
        let second = access
            .create("second", &role(), &JsonObject::new())
            .await
            .unwrap();
        let image = include_bytes!("fixtures/image.png");
        store
            .agent_blobs(first)
            .put(1.into(), "image/png", image)
            .await
            .unwrap();

        let error = store
            .agent_blobs(1.into())
            .read(1.into(), 1000)
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::BlobDenied);
        let error = store
            .agent_blobs(1.into())
            .for_call("other".into())
            .read(1.into(), 1000)
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::BlobDenied);
        let drive = store.agent_blobs(1.into()).for_call("drive".into());
        assert_eq!(drive.read(1.into(), 1000).await.unwrap(), image);

        let content = [ContentBlock::Image(1.into())];
        access.send("image", second, &content).await.unwrap();
        let bytes = store
            .agent_blobs(second)
            .read(1.into(), 1000)
            .await
            .unwrap();
        assert_eq!(bytes, image);

        store
            .interrupt_agent("interrupt", 1.into(), None)
            .await
            .unwrap();
        store.reconcile_agent_scopes().await.unwrap();
        assert_eq!(drive.read(1.into(), 1000).await.unwrap(), image);
    });
}
