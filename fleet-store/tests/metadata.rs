//! Tests for agent metadata and title edits.

#![cfg(feature = "queries")]

#[cfg(test)]
mod tests {
    use core::assert_matches;
    use fleet_core::{
        Agent, ClientStore as _, Configured, ContentBlock, JsonObject, Message, MessageMode,
        ModelOutput, ModelProvenance, ModelResponse, ModelResponseChunk, ModelUsage,
        RuntimeStore as _,
    };
    use fleet_kit::{
        AgentCollection as _, AgentMetadata, AgentsQuery, EventQuery, InboxAction, InboxState,
        Queries, QueryUpdate,
    };
    use fleet_store::{Store, StoreError};
    use futures_util::{StreamExt as _, future};
    use serde_json::json;

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

    fn message() -> Message {
        Message {
            id: 1.into(),
            mode: MessageMode::Steer,
            content: vec![ContentBlock::Text("Hello".into())],
        }
    }

    fn response() -> ModelResponse {
        ModelResponse {
            output: vec![ModelOutput::Text("Hi".into())],
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

    async fn metadata(store: &Store) -> AgentMetadata {
        let response = Queries::agent(store, 1.into(), None).await.unwrap();

        match response.value {
            QueryUpdate::Changed(metadata) => metadata,
            QueryUpdate::Unchanged => panic!("expected agent metadata"),
        }
    }

    #[test]
    fn set_title_preserves_other_extensions() {
        let store = Store::memory().unwrap();
        let mut agent = agent();
        agent.extensions = serde_json::from_value(json!({
            "other": {
                "enabled": true,
            },
            "fleet": "foreign data",
        }))
        .unwrap();
        let mut expected = agent.clone();
        expected
            .extensions
            .insert("fleet.title".into(), "Title".into());

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();

            store
                .set_title("rename", agent.id, 1.into(), Some("Title"))
                .await
                .unwrap();
            let renamed = metadata(&store).await;
            assert_eq!(renamed.agent, expected);
            assert_eq!(renamed.version, 2.into());

            store
                .set_title("clear", agent.id, 2.into(), None)
                .await
                .unwrap();
            let metadata = metadata(&store).await;
            assert_eq!(metadata.agent, agent);
            assert_eq!(metadata.version, 3.into());
        });
    }

    #[test]
    fn agent_tracks_metadata_changes_separately_from_activity() {
        let store = Store::memory().unwrap();
        let agent = agent();
        let message = message();

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();
            store
                .send_message("send", agent.id, &message)
                .await
                .unwrap();
            store
                .set_title("rename", agent.id, 1.into(), Some("Title"))
                .await
                .unwrap();
            let metadata = metadata(&store).await;
            store.consume_messages(agent.id, None, true).await.unwrap();

            let response = Queries::agent(&store, agent.id, Some(2.into()))
                .await
                .unwrap();
            assert_eq!(response.revision, 4.into());
            assert_eq!(metadata.version, 3.into());
            assert_eq!(response.value, QueryUpdate::Changed(metadata));

            let response = Queries::agent(&store, agent.id, Some(3.into()))
                .await
                .unwrap();
            assert_eq!(response.revision, 4.into());
            assert_eq!(response.value, QueryUpdate::Unchanged);

            let error = Queries::agent(&store, agent.id, Some(5.into()))
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::InvalidRevision);
        });
    }

    #[test]
    fn set_title_preserves_revisions_for_unchanged_values() {
        let store = Store::memory().unwrap();
        let agent = agent();

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();

            for (key, title) in [("empty", None), ("title", Some("Title"))] {
                store
                    .set_title(key, agent.id, 1.into(), title)
                    .await
                    .unwrap();
                let before = metadata(&store).await;
                let collection_revision = store.collection_revision().await.unwrap();

                store
                    .set_title(&format!("{key}-again"), agent.id, before.version, title)
                    .await
                    .unwrap();
                let after = Queries::agent(&store, agent.id, None).await.unwrap();
                let revision = store.collection_revision().await.unwrap();
                assert_eq!(after.revision, before.version);
                assert_eq!(after.value, QueryUpdate::Changed(before));
                assert_eq!(revision, collection_revision);
            }
        });
    }

    #[test]
    fn set_title_retries_do_not_reapply_edits() {
        let store = Store::memory().unwrap();
        let agent = agent();
        let edits = [
            ("empty", 1, None),
            ("rename", 1, Some("Title")),
            ("unchanged", 2, Some("Title")),
            ("clear", 2, None),
        ];

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();
            for (key, version, title) in edits {
                store
                    .set_title(key, agent.id, version.into(), title)
                    .await
                    .unwrap();
            }
            let before = Queries::agent(&store, agent.id, None).await.unwrap();
            let collection_revision = store.collection_revision().await.unwrap();

            for (key, version, title) in edits {
                store
                    .set_title(key, agent.id, version.into(), title)
                    .await
                    .unwrap();
            }
            let after = Queries::agent(&store, agent.id, None).await.unwrap();
            let revision = store.collection_revision().await.unwrap();
            assert_eq!(after, before);
            assert_eq!(revision, collection_revision);
        });
    }

    #[test]
    fn set_title_rejects_conflicts_without_reserving_the_key() {
        let store = Store::memory().unwrap();
        let agent = agent();

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();
            store
                .set_title("rename", agent.id, 1.into(), Some("Title"))
                .await
                .unwrap();

            let error = store
                .set_title("stale", agent.id, 1.into(), Some("Title"))
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::PreconditionFailed);

            let error = store
                .set_title("rename", agent.id, 1.into(), Some("Other"))
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::IdempotencyConflict);

            let error = store
                .set_title("missing", 99.into(), 1.into(), None)
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::AgentNotFound);

            store
                .set_title("stale", agent.id, 2.into(), None)
                .await
                .unwrap();
            let metadata = metadata(&store).await;
            assert_eq!(metadata.version, 3.into());
            assert_eq!(fleet_kit::agent_title(&metadata.agent.extensions), None);
        });
    }

    #[test]
    fn set_title_notifies_agent_and_collection_subscribers() {
        let store = Store::memory().unwrap();
        let agent = agent();

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();
            let revision = store.collection_revision().await.unwrap();
            let updates = future::join(
                store.updates(agent.id, 1.into()),
                store.agents_updates(revision),
            );
            let edit = store.set_title("rename", agent.id, 1.into(), Some("Title"));

            let ((update, collection), result) = future::join(updates, edit).await;
            result.unwrap();
            let update = update.unwrap();
            let collection = collection.unwrap();
            let metadata = metadata(&store).await;
            assert_eq!(update.revision, 2.into());
            assert_eq!(update.agent, QueryUpdate::Changed(metadata));
            assert_eq!(update.activity, QueryUpdate::Unchanged);
            assert_eq!(update.messages, []);
            assert_eq!(update.events, []);
            assert_eq!(update.tool_calls, []);
            assert_eq!(collection.agents.len(), 1);
            assert_eq!(collection.agents[0].title.as_deref(), Some("Title"));
            assert_eq!(collection.agents[0].metadata_version, 2.into());
        });
    }

    #[test]
    fn updates_coalesce_metadata_at_its_latest_revision() {
        let store = Store::memory().unwrap();
        let agent = agent();

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();
            store
                .set_title("first", agent.id, 1.into(), Some("First"))
                .await
                .unwrap();
            store
                .set_title("second", agent.id, 2.into(), Some("Second"))
                .await
                .unwrap();
            let metadata = metadata(&store).await;

            let initial = store.updates(agent.id, 0.into()).await.unwrap();
            assert_eq!(initial.revision, 1.into());
            assert_eq!(initial.agent, QueryUpdate::Unchanged);

            let update = store.updates(agent.id, initial.revision).await.unwrap();
            assert_eq!(update.revision, 3.into());
            assert_eq!(update.agent, QueryUpdate::Changed(metadata));

            let repeated = store.updates(agent.id, initial.revision).await.unwrap();
            assert_eq!(repeated, update);
        });
    }

    #[test]
    fn set_title_preserves_running_responses() {
        let store = Store::memory().unwrap();
        let agent = agent();
        let message = message();
        let response = response();
        let chunks = [
            ModelResponseChunk::TextStart(0),
            ModelResponseChunk::TextDelta(0, "Hi".into()),
        ];

        futures_executor::block_on(async {
            store
                .create_agent("create", &agent, Some(&message))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            for (sequence, chunk) in chunks.iter().enumerate() {
                store
                    .publish_model_response_chunk(
                        agent.id,
                        0.into(),
                        sequence.try_into().unwrap(),
                        chunk,
                    )
                    .await
                    .unwrap();
            }
            let state = store.agent_state(agent.id).await.unwrap();
            let preview = store
                .previews(agent.id)
                .boxed()
                .next()
                .await
                .unwrap()
                .unwrap();

            store
                .set_title("rename", agent.id, 1.into(), Some("Title"))
                .await
                .unwrap();
            let current_state = store.agent_state(agent.id).await.unwrap();
            let current_preview = store
                .previews(agent.id)
                .boxed()
                .next()
                .await
                .unwrap()
                .unwrap();
            let activity = store.activity(agent.id, Some(2.into())).await.unwrap();
            let events = Queries::events(
                &store,
                agent.id,
                EventQuery {
                    since: Some(2.into()),
                    ..EventQuery::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(current_state, state);
            assert_eq!(current_preview, preview);
            assert_eq!(activity.value, QueryUpdate::Unchanged);
            assert_eq!(events.value, []);

            store
                .append_model_response(agent.id, 0.into(), true, &response)
                .await
                .unwrap();
        });
    }

    #[test]
    fn set_title_preserves_inbox_order_and_snooze_deadline() {
        let store = Store::memory().unwrap();
        let agent = agent();

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();
            let page = store.agents(AgentsQuery::default()).await.unwrap();
            store
                .set_inbox(
                    "snooze",
                    agent.id,
                    page.agents[0].version,
                    InboxAction::Snooze(u64::MAX),
                )
                .await
                .unwrap();
            let mut before = store.agents(AgentsQuery::default()).await.unwrap();
            let before = before.agents.remove(0);

            store
                .set_title("rename", agent.id, 1.into(), Some("Title"))
                .await
                .unwrap();
            let mut after = store.agents(AgentsQuery::default()).await.unwrap();
            let after = after.agents.remove(0);
            let deadline = store.next_snooze().await.unwrap();
            assert!(after.version > before.version);
            assert_eq!(after.position, before.position);
            assert_eq!(after.since, before.since);
            assert_eq!(after.until, before.until);
            assert_eq!(after.inbox, InboxState::Snoozed);
            assert_eq!(deadline, Some(u64::MAX));

            store.expire_snoozes(u64::MAX).await.unwrap();
            let expired = store.agents(AgentsQuery::default()).await.unwrap();
            assert_eq!(expired.agents[0].inbox, InboxState::Pending);
            assert_eq!(expired.agents[0].title, after.title);
            assert_eq!(expired.agents[0].metadata_version, after.metadata_version);
        });
    }

    #[test]
    fn set_title_preserves_edits_and_retries_after_reopening() {
        let path = std::env::temp_dir().join(format!("fleet-metadata-{}.redb", std::process::id()));
        let agent = agent();

        futures_executor::block_on(async {
            let revision = {
                let store = Store::open(&path).unwrap();
                store.create_agent("create", &agent, None).await.unwrap();
                store
                    .set_title("rename", agent.id, 1.into(), Some("Title"))
                    .await
                    .unwrap();
                store
                    .set_title("clear", agent.id, 2.into(), None)
                    .await
                    .unwrap();
                store.collection_revision().await.unwrap()
            };

            let store = Store::open(&path).unwrap();
            store
                .set_title("rename", agent.id, 1.into(), Some("Title"))
                .await
                .unwrap();
            store
                .set_title("clear", agent.id, 2.into(), None)
                .await
                .unwrap();
            let metadata = metadata(&store).await;
            let collection_revision = store.collection_revision().await.unwrap();
            let update = store.updates(agent.id, 1.into()).await.unwrap();
            assert_eq!(metadata.version, 3.into());
            assert_eq!(fleet_kit::agent_title(&metadata.agent.extensions), None);
            assert_eq!(collection_revision, revision);
            assert_eq!(update.revision, 3.into());
        });

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn set_title_accepts_only_one_concurrent_edit() {
        let store = Store::memory().unwrap();
        let agent = agent();

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();
            let first = store.set_title("first", agent.id, 1.into(), Some("First"));
            let second = store.set_title("second", agent.id, 1.into(), Some("Second"));

            let results = future::join(first, second).await;
            let (title, error) = match results {
                (Ok(()), Err(error)) => ("First", error),
                (Err(error), Ok(())) => ("Second", error),
                results => panic!("expected one successful edit: {results:?}"),
            };
            assert_matches!(error, StoreError::PreconditionFailed);

            let metadata = metadata(&store).await;
            assert_eq!(metadata.version, 2.into());
            assert_eq!(
                fleet_kit::agent_title(&metadata.agent.extensions),
                Some(title)
            );
        });
    }
}
