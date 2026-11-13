//! Tests for turn notifications and devices.

#![cfg(feature = "notifications")]

use fleet_core::{
    Agent, AgentId, ClientStore as _, Configured, ContentBlock, JsonObject, Message, MessageMode,
    ModelOutput, ModelProvenance, ModelResponse, ModelUsage, RuntimeStore as _,
};
use fleet_store::Store;

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

fn response() -> ModelResponse {
    ModelResponse {
        output: vec![ModelOutput::Text("Done".into())],
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

async fn start_turn(store: &Store) -> AgentId {
    let agent = agent();
    store
        .create_agent("create", &agent, Some(&message(1)))
        .await
        .expect("agent can be created");
    store
        .consume_messages(agent.id, None, true)
        .await
        .expect("message can be consumed");
    agent.id
}

#[test]
fn completed_turns_are_claimed_until_completed() {
    let store = Store::memory().unwrap();

    futures_executor::block_on(async {
        let agent = start_turn(&store).await;
        assert_eq!(store.next_notification().await.unwrap(), None);
        store
            .append_model_response(agent, 0.into(), true, &response())
            .await
            .unwrap();
        assert_eq!(store.next_notification().await.unwrap(), Some(0));

        let first = store.claim_notification(100, 10).await.unwrap().unwrap();
        assert_eq!(first.agent(), agent);
        assert_eq!(store.claim_notification(109, 10).await.unwrap(), None);
        assert_eq!(store.next_notification().await.unwrap(), Some(110));

        let second = store.claim_notification(110, 10).await.unwrap().unwrap();
        store.complete_notification(first).await.unwrap();
        assert_eq!(store.next_notification().await.unwrap(), Some(120));
        store.complete_notification(second).await.unwrap();
        assert_eq!(store.next_notification().await.unwrap(), None);
    });
}

#[test]
fn turns_with_later_work_do_not_notify() {
    let store = Store::memory().unwrap();

    futures_executor::block_on(async {
        let agent = start_turn(&store).await;
        store
            .send_message("queued", agent, &message(2))
            .await
            .unwrap();
        store
            .append_model_response(agent, 0.into(), true, &response())
            .await
            .unwrap();
        assert_eq!(store.next_notification().await.unwrap(), None);

        store
            .consume_messages(agent, Some(2.into()), true)
            .await
            .unwrap();
        store
            .append_model_response(agent, 3.into(), true, &response())
            .await
            .unwrap();
        store
            .send_message("later", agent, &message(3))
            .await
            .unwrap();
        assert_eq!(store.claim_notification(0, 10).await.unwrap(), None);
        assert_eq!(store.next_notification().await.unwrap(), None);

        store
            .consume_messages(agent, Some(5.into()), true)
            .await
            .unwrap();
        store
            .interrupt_agent("interrupt", agent, None)
            .await
            .unwrap();
        store
            .settle_interrupt(agent, Some(6.into()), &[])
            .await
            .unwrap();
        assert_eq!(store.next_notification().await.unwrap(), None);
    });
}

#[test]
fn replaced_devices_are_not_deleted_as_stale() {
    let store = Store::memory().unwrap();
    let old = serde_json::json!({ "endpoint": "old" })
        .as_object()
        .unwrap()
        .clone();
    let new = serde_json::json!({ "endpoint": "new" })
        .as_object()
        .unwrap()
        .clone();

    futures_executor::block_on(async {
        store.put_device(1, old.clone()).await.unwrap();
        store.put_device(2, old.clone()).await.unwrap();
        store.put_device(1, new.clone()).await.unwrap();
        store
            .delete_device_if_unchanged(1, old.clone())
            .await
            .unwrap();
        store.delete_device(2).await.unwrap();
        assert_eq!(store.devices().await.unwrap(), vec![(1, new.clone())]);

        store.delete_device_if_unchanged(1, new).await.unwrap();
        assert_eq!(store.devices().await.unwrap(), vec![]);
    });
}
