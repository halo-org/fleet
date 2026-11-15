//! Collection scans, lifecycle transitions, and resumption across raced reads.

#![cfg(feature = "queries")]

use core::{assert_matches, num::NonZeroUsize};
use fleet_core::{
    Agent, ClientStore as _, Configured, ContentBlock, JsonObject, Message, MessageMode,
    ModelOutput, ModelProvenance, ModelResponse, ModelResponseChunk, ModelUsage, RuntimeStore as _,
};
use fleet_kit::{
    AgentCollection as _, AgentParent, AgentSummary, AgentsQuery, CollectionRevision,
    EventDirection, Extension as _, InboxAction, InboxFilter, InboxState, Queries, QueryUpdate,
};
use fleet_store::{Store, StoreError};
use futures_util::future;
use std::time::{SystemTime, UNIX_EPOCH};

fn agent(id: u128) -> Agent {
    Agent {
        id: id.into(),
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
        mode: MessageMode::Steer,
        content: vec![ContentBlock::Text("hello".into())],
    }
}

fn response() -> ModelResponse {
    ModelResponse {
        output: vec![ModelOutput::Text("done".into())],
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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after Unix epoch")
        .as_millis()
        .try_into()
        .expect("timestamp fits u64")
}

async fn summary(store: &Store, id: u128) -> AgentSummary {
    store
        .agents(AgentsQuery::default())
        .await
        .expect("collection can be read")
        .agents
        .into_iter()
        .find(|row| row.id == id.into())
        .expect("requested agent exists")
}

#[test]
fn raced_scans_resume_from_the_minimum_and_coalesce_superseded_versions() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        store.create_agent("create", &agent(1), None).await.unwrap();
        store
            .set_inbox(
                "settle",
                1.into(),
                summary(&store, 1).await.version,
                InboxAction::Settle,
            )
            .await
            .unwrap();
        let unsettled = store
            .agents(AgentsQuery {
                inbox: InboxFilter::Unsettled,
                ..AgentsQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(unsettled.agents, []);

        store
            .send_message("send", 1.into(), &message(1))
            .await
            .unwrap();
        let settled = store
            .agents(AgentsQuery {
                inbox: InboxFilter::State(InboxState::Settled),
                ..AgentsQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(settled.agents, []);
        let update = store
            .agents_updates(unsettled.revision.min(settled.revision))
            .await
            .unwrap();
        assert_eq!(update.agents[0].inbox, InboxState::Busy);

        store.consume_messages(1.into(), None, true).await.unwrap();
        store
            .append_model_response(1.into(), 0.into(), true, &response())
            .await
            .unwrap();
        store
            .set_inbox(
                "settle-again",
                1.into(),
                summary(&store, 1).await.version,
                InboxAction::Settle,
            )
            .await
            .unwrap();
        let later = store.agents(AgentsQuery::default()).await.unwrap();
        let catchup = store.agents_updates(update.revision).await.unwrap();
        assert_eq!(catchup.agents, later.agents);
        assert_eq!(catchup.revision, later.revision);
        assert_eq!(catchup.agents[0].inbox, InboxState::Settled);
        assert_eq!(
            store.agents_updates(update.revision).await.unwrap(),
            catchup
        );
    });
}

#[test]
fn messages_reopen_settled_agents_before_consumption() {
    futures_executor::block_on(async {
        for mode in [MessageMode::Steer, MessageMode::Queue] {
            let store = Store::memory().unwrap();
            let message = Message { mode, ..message(1) };
            store.create_agent("create", &agent(1), None).await.unwrap();
            store
                .set_inbox(
                    "settle",
                    1.into(),
                    summary(&store, 1).await.version,
                    InboxAction::Settle,
                )
                .await
                .unwrap();
            let settled = summary(&store, 1).await;

            store
                .send_message("send", 1.into(), &message)
                .await
                .unwrap();
            let busy = summary(&store, 1).await;
            let state = store.agent_state(1.into()).await.unwrap().unwrap();

            assert_eq!(busy.inbox, InboxState::Busy);
            assert!(busy.version > settled.version);
            assert_eq!(state.latest_event, None);

            let update = store.agents_updates(settled.version).await.unwrap();
            assert_eq!(update.agents, vec![busy.clone()]);

            let error = store
                .set_inbox("stale", 1.into(), settled.version, InboxAction::Settle)
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::PreconditionFailed);

            let error = store
                .set_inbox("busy", 1.into(), busy.version, InboxAction::Settle)
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::Conflict(_));
        }
    });
}

#[test]
fn pages_are_scans_and_a_removed_boundary_still_works() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        for id in 1..=6 {
            store
                .create_agent(&format!("create-{id}"), &agent(id), None)
                .await
                .unwrap();
            store
                .set_inbox(
                    &format!("settle-{id}"),
                    id.into(),
                    summary(&store, id).await.version,
                    InboxAction::Settle,
                )
                .await
                .unwrap();
        }
        let query = AgentsQuery {
            inbox: InboxFilter::State(InboxState::Settled),
            direction: EventDirection::Backward,
            limit: NonZeroUsize::new(2).unwrap(),
            ..AgentsQuery::default()
        };
        let first = store.agents(query).await.unwrap();
        assert_eq!(
            first
                .agents
                .iter()
                .map(|row| u128::from(row.id))
                .collect::<Vec<_>>(),
            vec![6, 5]
        );
        let boundary = first.next.unwrap();
        store
            .set_inbox(
                "restore-boundary",
                5.into(),
                first.agents[1].version,
                InboxAction::Pending,
            )
            .await
            .unwrap();
        let second = store
            .agents(AgentsQuery {
                from: Some(boundary),
                ..query
            })
            .await
            .unwrap();
        assert_eq!(
            second
                .agents
                .iter()
                .map(|row| u128::from(row.id))
                .collect::<Vec<_>>(),
            vec![4, 3]
        );
        let last = store
            .agents(AgentsQuery {
                from: second.next,
                ..query
            })
            .await
            .unwrap();
        assert_eq!(
            last.agents
                .iter()
                .map(|row| u128::from(row.id))
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(last.next, None);
        let bounded = store
            .agents(AgentsQuery {
                until: Some(boundary),
                ..query
            })
            .await
            .unwrap();
        assert_eq!(bounded.agents.len(), 1);
        assert_eq!(bounded.agents[0].id, 6.into());
        let empty = store
            .agents(AgentsQuery {
                since: Some(second.revision),
                ..query
            })
            .await
            .unwrap();
        assert_eq!(empty.agents, []);
        assert_eq!(empty.revision, second.revision);
        assert!(matches!(
            store
                .agents(AgentsQuery {
                    since: Some(CollectionRevision(u128::MAX)),
                    ..query
                })
                .await,
            Err(StoreError::InvalidRevision)
        ));
    });
}

#[test]
fn snoozes_are_idle_only_version_checked_and_cancelled_by_activity() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        store.create_agent("create", &agent(1), None).await.unwrap();
        let original = summary(&store, 1).await;
        let agent_revision = Queries::agent(&store, 1.into(), None)
            .await
            .unwrap()
            .revision;
        let deadline = now() + 60_000;
        store
            .set_inbox(
                "snooze",
                1.into(),
                original.version,
                InboxAction::Snooze(deadline),
            )
            .await
            .unwrap();
        assert_eq!(
            Queries::agent(&store, 1.into(), None)
                .await
                .unwrap()
                .revision,
            agent_revision
        );
        assert_eq!(store.next_snooze().await.unwrap(), Some(deadline));
        let snoozed = summary(&store, 1).await;
        assert_eq!(snoozed.inbox, InboxState::Snoozed);
        // An acknowledged retry remains successful after the version changes.
        store
            .set_inbox(
                "snooze",
                1.into(),
                original.version,
                InboxAction::Snooze(deadline),
            )
            .await
            .unwrap();
        assert_eq!(summary(&store, 1).await, snoozed);
        assert!(matches!(
            store
                .set_inbox("stale", 1.into(), original.version, InboxAction::Settle)
                .await,
            Err(StoreError::PreconditionFailed)
        ));
        assert!(matches!(
            store
                .set_inbox("snooze", 1.into(), original.version, InboxAction::Settle)
                .await,
            Err(StoreError::IdempotencyConflict)
        ));

        store
            .send_message("send", 1.into(), &message(1))
            .await
            .unwrap();
        let busy = summary(&store, 1).await;
        assert_eq!(busy.inbox, InboxState::Busy);
        assert!(busy.version > snoozed.version);
        assert_eq!(busy.until, None);
        assert_eq!(store.next_snooze().await.unwrap(), None);
        assert!(matches!(
            store
                .set_inbox("busy", 1.into(), busy.version, InboxAction::Settle)
                .await,
            Err(StoreError::Conflict(_))
        ));
        store.expire_snoozes(deadline).await.unwrap();
        assert_eq!(summary(&store, 1).await, busy);
        store.consume_messages(1.into(), None, true).await.unwrap();
        store
            .append_model_response(1.into(), 0.into(), true, &response())
            .await
            .unwrap();
        let idle = summary(&store, 1).await;
        assert_eq!(idle.inbox, InboxState::Pending);
        assert!(matches!(
            store
                .set_inbox("late-click", 1.into(), snoozed.version, InboxAction::Settle)
                .await,
            Err(StoreError::PreconditionFailed)
        ));

        store
            .set_inbox(
                "snooze-again",
                1.into(),
                idle.version,
                InboxAction::Snooze(deadline),
            )
            .await
            .unwrap();
        let before_expiry = store.collection_revision().await.unwrap();
        store.expire_snoozes(deadline - 1).await.unwrap();
        assert_eq!(store.collection_revision().await.unwrap(), before_expiry);
        store.expire_snoozes(deadline + 1000).await.unwrap();
        let expired = store.agents_updates(before_expiry).await.unwrap();
        assert_eq!(expired.agents[0].inbox, InboxState::Pending);
        assert_eq!(expired.agents[0].since, deadline);
        assert_eq!(store.next_snooze().await.unwrap(), None);
    });
}

#[test]
fn pins_survive_activity_and_snoozes_but_not_settling() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        store
            .create_agent("create", &agent(1), Some(&message(1)))
            .await
            .unwrap();
        let busy = summary(&store, 1).await;
        assert_eq!(busy.inbox, InboxState::Busy);
        assert_eq!(busy.pinned, None);

        store
            .set_pinned("pin", 1.into(), busy.metadata_version, true)
            .await
            .unwrap();
        let pinned = summary(&store, 1).await;
        assert!(pinned.pinned.is_some());
        assert!(pinned.metadata_version > busy.metadata_version);
        store
            .set_pinned("pin", 1.into(), busy.metadata_version, true)
            .await
            .unwrap();
        store
            .set_pinned("pin-again", 1.into(), pinned.metadata_version, true)
            .await
            .unwrap();
        assert_eq!(summary(&store, 1).await, pinned);

        store.consume_messages(1.into(), None, true).await.unwrap();
        store
            .append_model_response(1.into(), 0.into(), true, &response())
            .await
            .unwrap();
        let idle = summary(&store, 1).await;
        assert_eq!(idle.inbox, InboxState::Pending);
        assert_eq!(idle.pinned, pinned.pinned);

        let deadline = now() + 60_000;
        store
            .set_inbox(
                "snooze",
                1.into(),
                idle.version,
                InboxAction::Snooze(deadline),
            )
            .await
            .unwrap();
        store.expire_snoozes(deadline).await.unwrap();
        let woken = summary(&store, 1).await;
        assert_eq!(woken.inbox, InboxState::Pending);
        assert_eq!(woken.pinned, pinned.pinned);
        assert_eq!(woken.metadata_version, pinned.metadata_version);

        store
            .set_inbox("settle", 1.into(), woken.version, InboxAction::Settle)
            .await
            .unwrap();
        let settled = summary(&store, 1).await;
        let metadata = Queries::agent(&store, 1.into(), None).await.unwrap().value;
        assert_eq!(settled.inbox, InboxState::Settled);
        assert_eq!(settled.pinned, None);
        assert!(settled.metadata_version > woken.metadata_version);
        assert_matches!(
            metadata,
            QueryUpdate::Changed(metadata) if metadata.agent.extensions.is_empty()
        );

        let error = store
            .set_pinned("pin-settled", 1.into(), settled.metadata_version, true)
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));
        store
            .set_pinned("unpin", 1.into(), settled.metadata_version, false)
            .await
            .unwrap();
        assert_eq!(summary(&store, 1).await, settled);
    });
}

#[test]
fn children_are_scanned_by_parent_instead_of_the_inbox() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        let parent = AgentParent {
            agent: 1.into(),
            call: "call".into(),
        };
        let mut child = agent(2);
        parent.clone().insert(&mut child.extensions).unwrap();
        store.create_agent("parent", &agent(1), None).await.unwrap();
        let before = store.collection_revision().await.unwrap();
        store.create_agent("child", &child, None).await.unwrap();

        let roots = store.agents(AgentsQuery::default()).await.unwrap();
        let children = store
            .agents(AgentsQuery {
                parent: Some(1.into()),
                ..AgentsQuery::default()
            })
            .await
            .unwrap();
        let update = store.agents_updates(before).await.unwrap();

        assert_eq!(roots.agents.len(), 1);
        assert_eq!(roots.agents[0].id, 1.into());
        assert_eq!(roots.agents[0].parent, None);
        assert_eq!(children.agents.len(), 1);
        assert_eq!(children.agents[0].id, 2.into());
        assert_eq!(children.agents[0].parent, Some(parent));
        assert_eq!(update.agents, children.agents);
    });
}

#[test]
fn internal_and_preview_writes_do_not_republish_summaries() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        store.create_agent("create", &agent(1), None).await.unwrap();
        let idle = store.collection_revision().await.unwrap();
        store
            .send_message("send", 1.into(), &message(1))
            .await
            .unwrap();
        let busy = store.collection_revision().await.unwrap();
        assert!(busy > idle);
        store.consume_messages(1.into(), None, true).await.unwrap();
        assert_eq!(store.collection_revision().await.unwrap(), busy);
        store
            .publish_model_response_chunk(1.into(), 0.into(), 0, &ModelResponseChunk::TextStart(0))
            .await
            .unwrap();
        store
            .publish_model_response_chunk(
                1.into(),
                0.into(),
                1,
                &ModelResponseChunk::TextDelta(0, "token".into()),
            )
            .await
            .unwrap();
        store
            .publish_compaction_started(1.into(), 0.into())
            .await
            .unwrap();
        assert_eq!(store.collection_revision().await.unwrap(), busy);
        store
            .append_model_response(1.into(), 0.into(), false, &response())
            .await
            .unwrap();
        assert_eq!(store.collection_revision().await.unwrap(), busy);
        let (update, ()) = future::join(store.agents_updates(busy), async {
            store
                .append_model_response(1.into(), 1.into(), true, &response())
                .await
                .unwrap();
        })
        .await;
        assert_eq!(update.unwrap().agents[0].inbox, InboxState::Pending);
    });
}

#[test]
fn collection_resume_and_deadlines_survive_reopening() {
    let path = std::env::temp_dir().join(format!("fleet-collection-{}.redb", std::process::id()));
    futures_executor::block_on(async {
        let cursor;
        let deadline = now() + 60_000;
        {
            let store = Store::open(&path).unwrap();
            store.create_agent("create", &agent(1), None).await.unwrap();
            cursor = store.collection_revision().await.unwrap();
            store
                .set_inbox("snooze", 1.into(), cursor, InboxAction::Snooze(deadline))
                .await
                .unwrap();
        }
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.next_snooze().await.unwrap(), Some(deadline));
            assert_eq!(
                store.agents_updates(cursor).await.unwrap().agents[0].inbox,
                InboxState::Snoozed
            );
            store.expire_snoozes(deadline).await.unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.agents_updates(cursor).await.unwrap().agents[0].inbox,
            InboxState::Pending
        );
    });
    std::fs::remove_file(path).unwrap();
}
