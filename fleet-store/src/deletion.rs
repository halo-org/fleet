//! Deletion of root agents with their descendants, maintained with all feature sets.

use crate::{
    Store, StoreError, Topic,
    records::{self, AgentRecord, Records, Request},
    release_records::{self, Release},
    scope_records::{REQUESTS, SCOPES},
};
use fleet_core::AgentId;
use redb::{ReadableTable, Table, WriteTransaction};
use std::collections::BTreeSet;

const AGENT_RECORDS: [u8; 12] = [
    records::AGENT,
    records::EVENT,
    records::MESSAGE,
    records::QUEUE,
    records::TOOL_CALL,
    records::PENDING_TOOL,
    records::TURN_END,
    records::READY,
    records::CHANGE,
    records::ACTIVITY,
    records::RESOURCE_CHANGE,
    crate::notification_records::NOTIFICATION,
];

impl Store {
    /// Deletes a root agent, its descendants and the blobs no other agent holds.
    ///
    /// Their history and pending work are removed in one transaction. Client receipts keep only a
    /// fingerprint of their request, and the agents' IDs cannot be reused. Each agent's workspace
    /// release is scheduled with the role it had. Collection updates report the agents as
    /// removed, and wakes stop their execution. Deleting an absent agent succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`AgentDenied`](StoreError::AgentDenied) for a subagent and other errors if the
    /// deletion cannot be committed.
    pub async fn delete_agent(&self, agent: AgentId) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let agents = {
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    let Some(root) = records.get::<AgentRecord>(&records::key(
                        records::AGENT,
                        agent.into(),
                        &[],
                    ))?
                    else {
                        return Ok(());
                    };
                    if crate::metadata::agent_parent(&root.agent.extensions).is_some() {
                        return Err(StoreError::AgentDenied);
                    }
                    let agents = tree(&records, agent.into())?;
                    delete(&transaction, &mut records, &agents)?;
                    agents
                };
                transaction.commit()?;
                for &id in &agents {
                    worker.previews.remove(&id);
                    worker.listeners.notify(Topic::Agent(id));
                    worker.listeners.notify(Topic::Preview(id));
                    worker.notify_wake(id, true);
                }
                worker.listeners.notify(Topic::Agents);
                worker.listeners.notify(Topic::Snoozes);
                worker.listeners.notify(Topic::AgentScopes);
                worker.listeners.notify(Topic::WorkspaceReleases);
                worker.listeners.notify(Topic::Notifications);
                Ok(())
            })
            .await
    }
}

fn tree<T: ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    root: u128,
) -> Result<BTreeSet<u128>, StoreError> {
    let mut agents = BTreeSet::from([root]);
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        for child in crate::agents::children(records, parent)? {
            let child = child?;
            if agents.insert(child) {
                pending.push(child);
            }
        }
    }
    Ok(agents)
}

fn delete(
    transaction: &WriteTransaction,
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    agents: &BTreeSet<u128>,
) -> Result<(), StoreError> {
    let now = crate::agents::now_millis();
    let ended = [release_records::RELEASE, release_records::ENDED];
    let mut releases = Vec::new();
    for entry in records
        .0
        .range(ended.as_slice()..records::prefix_end(&ended).as_slice())?
    {
        let (key, value) = entry?;
        let Release(_, agent, ..) = serde_json::from_slice(value.value())?;
        if agents.contains(&agent) {
            releases.push(key.value().to_vec());
        }
    }
    for key in releases {
        records.remove(&key)?;
    }
    for &id in agents {
        let agent = records.agent(id)?;
        release_records::schedule_deleted(records, now, id, agent.revision, agent.agent.role)?;
    }
    crate::agents::delete(records, agents)?;
    crate::blob_records::revoke(transaction, records, agents)?;
    let mut receipts = transaction.open_table(REQUESTS)?;
    let mut scopes = transaction.open_table(SCOPES)?;
    for &id in agents {
        for kind in AGENT_RECORDS {
            let prefix = records::key(kind, id, &[]);
            records.0.retain_in(
                prefix.as_slice()..records::prefix_end(&prefix).as_slice(),
                |_, _| false,
            )?;
        }
        let prefix = records::key(records::AGENT_REQUEST, id, &[]);
        let mut requests = Vec::new();
        for entry in records
            .0
            .range(prefix.as_slice()..records::prefix_end(&prefix).as_slice())?
        {
            let key = entry?.0;
            let idempotency_key = key.value().strip_prefix(prefix.as_slice());
            requests.push(idempotency_key.expect("key has the prefix").to_vec());
        }
        for idempotency_key in requests {
            let key = records::key(records::REQUEST, 0, &idempotency_key);
            if let Some(request) = records.get::<Request>(&key)? {
                records.remove(&key)?;
                let key = records::key(records::FINGERPRINT, 0, &idempotency_key);
                records.put(&key, &request.fingerprint()?)?;
            }
            records.remove(&records::key(records::AGENT_REQUEST, id, &idempotency_key))?;
        }
        for (start, end) in crate::scope_records::receipts(id) {
            receipts.retain_in(start.as_str()..end.as_str(), |_, _| false)?;
        }
        scopes.remove(id)?;
    }
    Ok(())
}

#[cfg(test)]
#[cfg(all(feature = "blobs", feature = "subagents"))]
mod tests {
    use super::*;
    use crate::{blob_records, role_records};
    use fleet_core::{
        Agent, ClientStore as _, Configured, ContentBlock, JsonObject, Message, MessageMode,
        ModelOutput, ModelProvenance, ModelResponse, ModelUsage, RuntimeStore as _, ToolCall,
    };
    use fleet_kit::{AgentCollection as _, Agents as _, AgentsQuery, Blobs as _};
    use redb::{ReadableDatabase as _, TableHandle as _};
    use std::collections::BTreeMap;

    const IMAGE: &[u8] = include_bytes!("../tests/fixtures/image.png");

    fn role() -> Configured<fleet_core::RoleName> {
        Configured {
            name: "assistant".into(),
            config: JsonObject::new(),
        }
    }

    fn agent(id: u128) -> Agent {
        Agent {
            id: id.into(),
            role: role(),
            extensions: JsonObject::new(),
        }
    }

    fn message(images: &[u128]) -> Message {
        Message {
            id: 1.into(),
            mode: MessageMode::Queue,
            content: images
                .iter()
                .map(|&id| ContentBlock::Image(id.into()))
                .collect(),
        }
    }

    /// Consumes the agent's messages and ends its turn with an async `drive` call.
    async fn drive(store: &Store, id: AgentId) {
        let call = ModelOutput::ToolCall(ToolCall {
            id: "drive".into(),
            tool: "drive".into(),
            is_async: true,
            input: JsonObject::new(),
        });
        let response = ModelResponse {
            output: vec![call],
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
        };
        store.consume_messages(id, None, true).await.unwrap();
        let state = store.agent_state(id).await.unwrap().unwrap();
        store
            .append_model_response(id, state.latest_event.unwrap(), true, &response)
            .await
            .unwrap();
    }

    /// Reads every row of every table.
    async fn rows(store: &Store) -> BTreeMap<(String, Vec<u8>), Vec<u8>> {
        store
            .worker()
            .call(|worker| {
                let transaction = worker.database.begin_read()?;
                let mut rows = BTreeMap::new();
                for table in [records::RECORDS, blob_records::CHUNKS] {
                    for entry in transaction.open_table(table)?.iter()? {
                        let (key, value) = entry?;
                        let key = (table.name().to_owned(), key.value().to_vec());
                        rows.insert(key, value.value().to_vec());
                    }
                }
                for table in [blob_records::BYTES, blob_records::UPLOADS, SCOPES] {
                    for entry in transaction.open_table(table)?.iter()? {
                        let (key, value) = entry?;
                        let key = (table.name().to_owned(), key.value().to_be_bytes().to_vec());
                        rows.insert(key, value.value().to_vec());
                    }
                }
                for table in [role_records::ROLES, REQUESTS] {
                    for entry in transaction.open_table(table)?.iter()? {
                        let (key, value) = entry?;
                        let key = (table.name().to_owned(), key.value().as_bytes().to_vec());
                        rows.insert(key, value.value().to_vec());
                    }
                }
                Ok(rows)
            })
            .await
            .unwrap()
    }

    #[test]
    fn deleting_a_tree_removes_its_rows_and_the_blobs_only_it_holds() {
        futures_executor::block_on(async {
            let store = Store::memory().unwrap();
            store.put(1.into(), "image/png", IMAGE).await.unwrap();
            store
                .create_agent("survivor", &agent(9), Some(&message(&[1])))
                .await
                .unwrap();
            let before = rows(&store).await;

            let size = u64::try_from(IMAGE.len()).unwrap();
            store.create_upload(2, "image/png", size).await.unwrap();
            store.append_upload(2, 0, IMAGE).await.unwrap();
            store.complete_upload(2).await.unwrap();
            store
                .create_agent("root", &agent(1), Some(&message(&[1, 2])))
                .await
                .unwrap();
            drive(&store, 1.into()).await;
            let access = store.agent_access(1.into()).for_call("drive".into());
            let child = access
                .create("child", &role(), &JsonObject::new())
                .await
                .unwrap();
            access
                .send("send", child, &message(&[2]).content)
                .await
                .unwrap();
            drive(&store, child).await;
            let grandchild = store
                .agent_access(child)
                .for_call("drive".into())
                .create("grandchild", &role(), &JsonObject::new())
                .await
                .unwrap();
            store
                .agent_blobs(grandchild)
                .put(3.into(), "image/png", IMAGE)
                .await
                .unwrap();
            let root = store.agents(AgentsQuery::default()).await.unwrap().agents;
            let version = root
                .iter()
                .find(|agent| agent.id == 1.into())
                .unwrap()
                .version;
            store
                .request(
                    "settle".into(),
                    Request::SetInbox {
                        id: 1,
                        expected: version.0,
                        inbox: crate::agents::SETTLED,
                        until: None,
                    },
                )
                .await
                .unwrap();

            store.delete_agent(1.into()).await.unwrap();

            let (releases, _) = store.workspace_releases(0, None).await.unwrap();
            let releases = releases
                .iter()
                .map(|release| (release.agent(), release.deleted_role().unwrap().clone()))
                .collect::<Vec<_>>();
            let mut tree = [1.into(), child, grandchild];
            tree.sort();
            assert_eq!(releases, tree.map(|agent| (agent, role())));

            let tree = tree.map(u128::from);
            let deleted = |table: &str, key: &[u8]| {
                let counter = key == [13] || key == [15];
                let tombstone = matches!((key.first(), key.len()), (Some(14), 17) | (Some(16), 33))
                    && tree.contains(&u128::from_be_bytes(*key.last_chunk().unwrap()));
                let fingerprint = key.first() == Some(&records::FINGERPRINT);
                let release =
                    key.starts_with(&[release_records::RELEASE, release_records::DELETED]);
                table == "records" && (counter || tombstone || fingerprint || release)
            };
            let mut after = rows(&store).await;
            after.retain(|(table, key), _| !deleted(table, key));
            let mut before = before;
            before.retain(|(table, key), _| !deleted(table, key));
            assert_eq!(after, before);
        });
    }

    #[test]
    fn upgrading_from_format_5_indexes_grants_and_receipts() {
        let path = std::env::temp_dir().join(format!(
            "fleet-deletion-upgrade-{}.redb",
            std::process::id()
        ));
        futures_executor::block_on(async {
            {
                let store = Store::open(&path).unwrap();
                store.put(1.into(), "image/png", IMAGE).await.unwrap();
                store.put(2.into(), "image/png", IMAGE).await.unwrap();
                store
                    .create_agent("survivor", &agent(9), Some(&message(&[1])))
                    .await
                    .unwrap();
                store
                    .create_agent("root", &agent(1), Some(&message(&[1, 2])))
                    .await
                    .unwrap();
                store
                    .worker()
                    .call(|worker| {
                        let transaction = worker.database.begin_write()?;
                        {
                            let mut records = Records(transaction.open_table(records::RECORDS)?);
                            let indexes = [blob_records::HOLDER].as_slice()
                                ..[records::AGENT_REQUEST + 1].as_slice();
                            records.0.retain_in(indexes, |_, _| false)?;
                            records.put(&[], &5_u32)?;
                        }
                        transaction.commit()?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            let store = Store::open(&path).unwrap();

            store.delete_agent(1.into()).await.unwrap();

            assert!(store.get(1.into()).await.unwrap().is_some());
            assert_eq!(store.get(2.into()).await.unwrap(), None);
            let receipt = records::key(records::REQUEST, 0, b"root");
            assert!(
                !rows(&store)
                    .await
                    .contains_key(&("records".into(), receipt))
            );
        });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn deleting_keeps_unfinished_uploads_that_reuse_a_blob_id() {
        futures_executor::block_on(async {
            let store = Store::memory().unwrap();
            store.put(1.into(), "image/png", IMAGE).await.unwrap();
            store
                .create_agent("root", &agent(1), Some(&message(&[1])))
                .await
                .unwrap();
            let size = u64::try_from(IMAGE.len()).unwrap();
            store.create_upload(1, "image/png", size).await.unwrap();
            store.append_upload(1, 0, IMAGE).await.unwrap();

            store.delete_agent(1.into()).await.unwrap();

            assert_eq!(store.get(1.into()).await.unwrap(), None);
            assert_eq!(store.complete_upload(1).await.unwrap(), 1.into());
            assert_eq!(store.read(1.into(), u64::MAX).await.unwrap(), IMAGE);
        });
    }
}
