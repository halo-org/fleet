//! Transactional summaries, inbox indexes, and resumable collection reads.

#[cfg(feature = "queries")]
use crate::{Store, Topic, records::Request};
use crate::{
    StoreError,
    records::{self, AgentRecord, Records},
};
#[cfg(feature = "queries")]
use fleet_core::AgentId;
#[cfg(feature = "queries")]
use fleet_kit::{
    AgentCollection, AgentPosition, AgentSummary, AgentsPage, AgentsQuery, AgentsUpdates,
    CollectionRevision, EventDirection, InboxAction, InboxFilter, InboxState, Revision,
};
#[cfg(feature = "queries")]
use redb::ReadableDatabase as _;
use redb::{ReadableTable, Table};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const META: u8 = 13;
const SUMMARY: u8 = 14;
const INBOX: u8 = 15;
const CHANGE: u8 = 16;
const SNOOZE: u8 = 17;

const PENDING: u8 = 0;
const BUSY: u8 = 1;
const SNOOZED: u8 = 2;
const SETTLED: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Summary {
    role: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    pinned: Option<u64>,
    #[serde(default = "records::initial_metadata_revision")]
    metadata_version: u128,
    inbox: u8,
    since: u64,
    until: Option<u64>,
    interrupt_pending: bool,
    order: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SummaryRecord {
    id: u128,
    version: u128,
    // A deletion is retained here and in CHANGE, so resumes cannot resurrect it.
    value: Option<Summary>,
}

#[cfg(feature = "queries")]
impl SummaryRecord {
    fn summary(self) -> Option<AgentSummary> {
        let value = self.value?;
        let inbox = state(value.inbox);
        Some(AgentSummary {
            id: self.id.into(),
            role: value.role.into(),
            title: value.title,
            pinned: value.pinned,
            metadata_version: value.metadata_version.into(),
            version: CollectionRevision(self.version),
            inbox,
            since: value.since,
            until: value.until,
            interrupt_pending: value.interrupt_pending,
            position: AgentPosition {
                inbox,
                order: value.order,
                id: self.id.into(),
            },
        })
    }
}

#[cfg(feature = "queries")]
const fn state(value: u8) -> InboxState {
    match value {
        PENDING => InboxState::Pending,
        BUSY => InboxState::Busy,
        SNOOZED => InboxState::Snoozed,
        SETTLED => InboxState::Settled,
        _ => unreachable!(),
    }
}

#[cfg(feature = "queries")]
const fn state_key(value: InboxState) -> u8 {
    match value {
        InboxState::Pending => PENDING,
        InboxState::Busy => BUSY,
        InboxState::Snoozed => SNOOZED,
        InboxState::Settled => SETTLED,
    }
}

fn position_key(inbox: u8, order: u128, id: u128) -> Vec<u8> {
    [
        &[INBOX, inbox],
        order.to_be_bytes().as_slice(),
        id.to_be_bytes().as_slice(),
    ]
    .concat()
}

fn inbox_key(id: u128, value: &Summary) -> Vec<u8> {
    position_key(value.inbox, value.order, id)
}

fn change_key(revision: u128, id: u128) -> Vec<u8> {
    [
        &[CHANGE],
        revision.to_be_bytes().as_slice(),
        id.to_be_bytes().as_slice(),
    ]
    .concat()
}

fn snooze_key(until: u64, id: u128) -> Vec<u8> {
    [
        &[SNOOZE],
        until.to_be_bytes().as_slice(),
        id.to_be_bytes().as_slice(),
    ]
    .concat()
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn revision<T: ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
) -> Result<u128, StoreError> {
    Ok(records.get(&[META])?.unwrap_or(0))
}

#[cfg(feature = "queries")]
fn validate_revision<T: ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    since: Option<CollectionRevision>,
) -> Result<u128, StoreError> {
    let current = revision(records)?;
    if since.is_some_and(|since| since.0 > current) {
        return Err(StoreError::InvalidRevision);
    }
    Ok(current)
}

// Change entries reference only current versions, just like per-agent resource changes.
// Their cardinality is bounded by the number of agents, not the number of events.
fn write_summary(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    id: u128,
    old: Option<&SummaryRecord>,
    mut value: Summary,
) -> Result<bool, StoreError> {
    if old.and_then(|row| row.value.as_ref()) == Some(&value) {
        return Ok(false);
    }
    let version = revision(records)?
        .checked_add(1)
        .expect("collection revisions do not overflow");
    if old
        .and_then(|row| row.value.as_ref())
        .is_none_or(|previous| previous.inbox != value.inbox || previous.until != value.until)
    {
        value.order = u128::from(value.until.unwrap_or(value.since));
    }
    if let Some(old) = old {
        records.remove(&change_key(old.version, id))?;
        if let Some(previous) = &old.value {
            records.remove(&inbox_key(id, previous))?;
            if let Some(until) = previous.until {
                records.remove(&snooze_key(until, id))?;
            }
        }
    }
    records.put(&inbox_key(id, &value), &id)?;
    if let Some(until) = value.until {
        records.put(&snooze_key(until, id), &(id, version))?;
    }
    records.put(
        &records::key(SUMMARY, id, &[]),
        &SummaryRecord {
            id,
            version,
            value: Some(value),
        },
    )?;
    records.put(&change_key(version, id), &id)?;
    records.put(&[META], &version)?;
    Ok(true)
}

/// Called by both runtime writes and client writes before their transaction commits.
pub fn sync_agent(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    agent: &AgentRecord,
    now: u64,
) -> Result<bool, StoreError> {
    let id = agent.agent.id;
    let old = records.get::<SummaryRecord>(&records::key(SUMMARY, id, &[]))?;
    let mut value = old
        .as_ref()
        .and_then(|row| row.value.clone())
        .unwrap_or_else(|| Summary {
            role: agent.agent.role.name.clone(),
            title: None,
            pinned: None,
            metadata_version: agent.metadata_revision,
            inbox: PENDING,
            since: now,
            until: None,
            interrupt_pending: agent.interrupt_intent,
            order: 0,
        });
    let inbox = if agent.turn_active || agent.pending_messages != 0 {
        BUSY
    } else if value.inbox == BUSY {
        PENDING
    } else {
        value.inbox
    };
    if inbox != value.inbox {
        value.inbox = inbox;
        value.since = now;
        value.until = None;
    }
    value.role.clone_from(&agent.agent.role.name);
    value.title = crate::metadata::agent_title(&agent.agent.extensions).map(str::to_owned);
    value.pinned = crate::metadata::agent_pinned(&agent.agent.extensions);
    value.metadata_version = agent.metadata_revision;
    value.interrupt_pending = agent.interrupt_intent;
    write_summary(records, id, old.as_ref(), value)
}

pub fn set_inbox(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    id: u128,
    expected: u128,
    inbox: u8,
    until: Option<u64>,
    now: u64,
) -> Result<(bool, bool), StoreError> {
    let mut agent = records.agent(id)?;
    let old = records
        .get::<SummaryRecord>(&records::key(SUMMARY, id, &[]))?
        .ok_or(StoreError::UnsupportedFormat)?;
    if old.version != expected {
        return Err(StoreError::PreconditionFailed);
    }
    if agent.turn_active || agent.pending_messages != 0 {
        return Err(StoreError::Conflict(
            "only idle agents can be settled or snoozed",
        ));
    }
    if until.is_some_and(|until| until <= now) {
        return Err(StoreError::Conflict(
            "snooze deadline must be in the future",
        ));
    }
    let mut value = old.value.clone().ok_or(StoreError::AgentNotFound)?;
    let changed = inbox == SETTLED && crate::metadata::unpin(records, &mut agent)?;
    value.pinned = crate::metadata::agent_pinned(&agent.agent.extensions);
    value.metadata_version = agent.metadata_revision;
    if value.inbox != inbox || value.until != until {
        value.inbox = inbox;
        value.until = until;
        value.since = now;
    }
    Ok((changed, write_summary(records, id, Some(&old), value)?))
}

pub fn settled<T: ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    id: u128,
) -> Result<bool, StoreError> {
    Ok(records
        .get::<SummaryRecord>(&records::key(SUMMARY, id, &[]))?
        .and_then(|row| row.value)
        .is_some_and(|value| value.inbox == SETTLED))
}

pub fn expire(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    now: u64,
) -> Result<bool, StoreError> {
    let end = if now == u64::MAX {
        vec![SNOOZE + 1]
    } else {
        [&[SNOOZE], now.saturating_add(1).to_be_bytes().as_slice()].concat()
    };
    let due = records
        .range::<(u128, u128)>(&[SNOOZE], &end)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut changed = false;
    for (id, expected) in due {
        let old = records
            .get::<SummaryRecord>(&records::key(SUMMARY, id, &[]))?
            .ok_or(StoreError::UnsupportedFormat)?;
        let Some(mut value) = old.value.clone() else {
            continue;
        };
        if old.version == expected
            && value.inbox == SNOOZED
            && value.until.is_some_and(|until| until <= now)
        {
            value.inbox = PENDING;
            value.since = value.until.take().expect("snoozed agent has deadline");
            changed |= write_summary(records, id, Some(&old), value)?;
        }
    }
    Ok(changed)
}

#[cfg(feature = "queries")]
impl AgentCollection for Store {
    type Error = StoreError;

    async fn agents(&self, query: AgentsQuery) -> Result<AgentsPage, StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                let revision = CollectionRevision(validate_revision(&records, query.since)?);
                let (low, high) = match query.inbox {
                    InboxFilter::All => (PENDING, SETTLED + 1),
                    InboxFilter::Unsettled => (PENDING, SETTLED),
                    InboxFilter::State(state) => {
                        (state_key(state), state_key(state).saturating_add(1))
                    }
                };
                let (lower, upper) = match query.direction {
                    EventDirection::Forward => (query.from, query.until),
                    EventDirection::Backward => (query.until, query.from),
                };
                let mut start = vec![INBOX, low];
                let mut end = vec![INBOX, high];
                if let Some(lower) = lower {
                    let mut key =
                        position_key(state_key(lower.inbox), lower.order, lower.id.into());
                    key.push(0);
                    start = start.max(key);
                }
                if let Some(upper) = upper {
                    end = end.min(position_key(
                        state_key(upper.inbox),
                        upper.order,
                        upper.id.into(),
                    ));
                }
                let mut agents = Vec::new();
                let mut next = None;
                if start < end {
                    let rows = records.range::<u128>(&start, &end)?;
                    let rows: Box<dyn Iterator<Item = Result<u128, StoreError>> + '_> =
                        match query.direction {
                            EventDirection::Forward => Box::new(rows),
                            EventDirection::Backward => Box::new(rows.rev()),
                        };
                    for id in rows {
                        let row = records
                            .get::<SummaryRecord>(&records::key(SUMMARY, id?, &[]))?
                            .ok_or(StoreError::UnsupportedFormat)?;
                        if query.since.is_some_and(|since| row.version <= since.0) {
                            continue;
                        }
                        if let Some(summary) = row.summary() {
                            if agents.len() == query.limit.get() {
                                next = agents.last().map(|row: &AgentSummary| row.position);
                                break;
                            }
                            agents.push(summary);
                        }
                    }
                }
                Ok(AgentsPage {
                    revision,
                    agents,
                    next,
                })
            })
            .await
    }

    async fn agents_updates(&self, after: CollectionRevision) -> Result<AgentsUpdates, StoreError> {
        loop {
            let notified = self.listeners().listen(Topic::Agents);
            let update = self
                .worker()
                .call(move |worker| {
                    let transaction = worker.database.begin_read()?;
                    let records = Records(transaction.open_table(records::RECORDS)?);
                    validate_revision(&records, Some(after))?;
                    let Some(next) = after.0.checked_add(1) else {
                        return Ok(None);
                    };
                    let start = [&[CHANGE], next.to_be_bytes().as_slice()].concat();
                    let mut rows = records.range::<u128>(&start, &[CHANGE + 1])?;
                    let Some(id) = rows.next().transpose()? else {
                        return Ok(None);
                    };
                    let first = records
                        .get::<SummaryRecord>(&records::key(SUMMARY, id, &[]))?
                        .ok_or(StoreError::UnsupportedFormat)?;
                    let version = first.version;
                    let mut update = AgentsUpdates {
                        revision: CollectionRevision(version),
                        agents: Vec::new(),
                        removed: Vec::new(),
                    };
                    for row in core::iter::once(Ok(first)).chain(rows.map(|id| {
                        records
                            .get::<SummaryRecord>(&records::key(SUMMARY, id?, &[]))?
                            .ok_or(StoreError::UnsupportedFormat)
                    })) {
                        let row = row?;
                        if row.version != version {
                            break;
                        }
                        let id = row.id;
                        if let Some(summary) = row.summary() {
                            update.agents.push(summary);
                        } else {
                            update.removed.push(id.into());
                        }
                    }
                    Ok(Some(update))
                })
                .await?;
            if let Some(update) = update {
                return Ok(update);
            }
            notified.await;
        }
    }

    async fn set_title(
        &self,
        idempotency_key: &str,
        agent: AgentId,
        expected: Revision,
        title: Option<&str>,
    ) -> Result<(), StoreError> {
        self.request(
            idempotency_key.into(),
            Request::SetTitle {
                id: agent.into(),
                expected: expected.into(),
                title: title.map(str::to_owned),
            },
        )
        .await
    }

    async fn set_pinned(
        &self,
        idempotency_key: &str,
        agent: AgentId,
        expected: Revision,
        pinned: bool,
    ) -> Result<(), StoreError> {
        self.request(
            idempotency_key.into(),
            Request::SetPinned {
                id: agent.into(),
                expected: expected.into(),
                pinned,
            },
        )
        .await
    }

    async fn set_inbox(
        &self,
        idempotency_key: &str,
        agent: AgentId,
        expected: CollectionRevision,
        action: InboxAction,
    ) -> Result<(), StoreError> {
        let (inbox, until) = match action {
            InboxAction::Pending => (PENDING, None),
            InboxAction::Settle => (SETTLED, None),
            InboxAction::Snooze(until) => (SNOOZED, Some(until)),
        };
        self.request(
            idempotency_key.into(),
            Request::SetInbox {
                id: agent.into(),
                expected: expected.0,
                inbox,
                until,
            },
        )
        .await
    }
}

#[cfg(feature = "queries")]
impl Store {
    /// Reads the collection's current revision without scanning summaries.
    ///
    /// # Errors
    /// Returns an error if the store cannot be read.
    pub async fn collection_revision(&self) -> Result<CollectionRevision, StoreError> {
        self.worker()
            .call(|worker| {
                let transaction = worker.database.begin_read()?;
                revision(&Records(transaction.open_table(records::RECORDS)?))
                    .map(CollectionRevision)
            })
            .await
    }

    /// Expires due snoozes transactionally, including while no clients are connected.
    ///
    /// # Errors
    /// Returns an error if the store cannot commit the expirations.
    pub async fn expire_snoozes(&self, now: u64) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let changed = expire(&mut Records(transaction.open_table(records::RECORDS)?), now)?;
                transaction.commit()?;
                if changed {
                    worker.listeners.notify(Topic::Agents);
                    worker.listeners.notify(Topic::Snoozes);
                }
                Ok(())
            })
            .await
    }

    /// Reads the next persisted deadline in Unix milliseconds.
    ///
    /// # Errors
    /// Returns an error if the store cannot be read.
    pub async fn next_snooze(&self) -> Result<Option<u64>, StoreError> {
        self.worker()
            .call(|worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                let first = records
                    .prefix::<(u128, u128)>(&[SNOOZE])?
                    .next()
                    .transpose()?;
                match first {
                    None => Ok(None),
                    Some((id, _)) => Ok(records
                        .get::<SummaryRecord>(&records::key(SUMMARY, id, &[]))?
                        .and_then(|row| row.value)
                        .and_then(|value| value.until)),
                }
            })
            .await
    }

    /// Registers immediately for deadline changes. Register before reading `next_snooze`.
    pub fn snooze_changed(&self) -> impl Future<Output = ()> + Send + use<> {
        self.listeners().listen(Topic::Snoozes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, records::Request};
    use core::assert_matches;
    use fleet_core::{
        Agent, ClientStore as _, Configured, ContentBlock, JsonObject, Message, MessageMode,
        RuntimeStore,
    };
    #[cfg(not(feature = "queries"))]
    use redb::ReadableDatabase as _;

    fn agent() -> Agent {
        Agent {
            id: 1.into(),
            role: Configured {
                name: "test".into(),
                config: JsonObject::new(),
            },
            extensions: JsonObject::new(),
        }
    }

    #[test]
    fn sync_agent_preserves_metadata_after_reopening() {
        let path =
            std::env::temp_dir().join(format!("fleet-core-metadata-{}.redb", std::process::id()));
        let agent = agent();
        let message = Message {
            id: 1.into(),
            mode: MessageMode::Steer,
            content: vec![ContentBlock::Text("Hello".into())],
        };
        let title_request = || Request::SetTitle {
            id: 1,
            expected: 1,
            title: Some("Title".into()),
        };

        futures_executor::block_on(async {
            {
                let store = Store::open(&path).unwrap();
                store.create_agent("create", &agent, None).await.unwrap();
                store
                    .request("title".into(), title_request())
                    .await
                    .unwrap();
            }

            let store = Store::open(&path).unwrap();
            let error = store
                .send_message("title", agent.id, &message)
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::IdempotencyConflict);

            store
                .send_message("send", agent.id, &message)
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .request("title".into(), title_request())
                .await
                .unwrap();
            let stored = RuntimeStore::agent(&store, agent.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.extensions["fleet.title"], "Title");

            let (agent, summary) = store
                .worker()
                .call(|worker| {
                    let transaction = worker.database.begin_read()?;
                    let records = Records(transaction.open_table(records::RECORDS)?);
                    let agent = records.agent(1)?;
                    let summary = records
                        .get::<SummaryRecord>(&records::key(SUMMARY, 1, &[]))?
                        .unwrap();
                    Ok((agent, summary))
                })
                .await
                .unwrap();
            let value = summary.value.unwrap();
            assert_eq!(agent.revision, 4);
            assert_eq!(agent.metadata_revision, 2);
            assert_eq!(summary.version, 3);
            assert_eq!(value.title.as_deref(), Some("Title"));
            assert_eq!(value.metadata_version, 2);
            assert_eq!(value.inbox, BUSY);
        });

        std::fs::remove_file(path).unwrap();
    }

    #[cfg(feature = "queries")]
    #[test]
    fn overdue_snoozes_expire_before_reads_after_reopening() {
        let path =
            std::env::temp_dir().join(format!("fleet-inbox-reopen-{}.redb", std::process::id()));
        futures_executor::block_on(async {
            let cursor;
            {
                let store = Store::open(&path).unwrap();
                store.create_agent("create", &agent(), None).await.unwrap();
                let page = store.agents(AgentsQuery::default()).await.unwrap();
                assert_eq!(page.agents.len(), 1);
                assert_eq!(page.agents[0].inbox, InboxState::Pending);
                cursor = page.revision;
                store
                    .worker()
                    .call(move |worker| {
                        let tx = worker.database.begin_write()?;
                        set_inbox(
                            &mut Records(tx.open_table(records::RECORDS)?),
                            1,
                            cursor.0,
                            SNOOZED,
                            Some(1),
                            0,
                        )?;
                        tx.commit()?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            let store = Store::open(&path).unwrap();
            let update = store.agents_updates(cursor).await.unwrap();
            assert_eq!(update.agents[0].inbox, InboxState::Pending);
            assert_eq!(update.agents[0].since, 1);
            assert_eq!(store.next_snooze().await.unwrap(), None);
            assert_eq!(
                store.agents(AgentsQuery::default()).await.unwrap().agents,
                update.agents
            );
        });
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(feature = "queries")]
    #[test]
    fn deletion_tombstones_are_repeatable_and_absent_from_scans() {
        futures_executor::block_on(async {
            let store = Store::memory().unwrap();
            store.create_agent("create", &agent(), None).await.unwrap();
            let delayed_page = store.agents(AgentsQuery::default()).await.unwrap();
            // There is no public agent deletion operation yet. Exercise the
            // persisted tombstone representation used by the collection stream.
            store
                .worker()
                .call(|worker| {
                    let tx = worker.database.begin_write()?;
                    {
                        let mut records = Records(tx.open_table(records::RECORDS)?);
                        let old = records
                            .get::<SummaryRecord>(&records::key(SUMMARY, 1, &[]))?
                            .unwrap();
                        let version = revision(&records)? + 1;
                        records.remove(&inbox_key(1, old.value.as_ref().unwrap()))?;
                        records.remove(&change_key(old.version, 1))?;
                        records.remove(&records::key(records::AGENT, 1, &[]))?;
                        records.put(
                            &records::key(SUMMARY, 1, &[]),
                            &SummaryRecord {
                                id: 1,
                                version,
                                value: None,
                            },
                        )?;
                        records.put(&change_key(version, 1), &1_u128)?;
                        records.put(&[META], &version)?;
                    }
                    tx.commit()?;
                    Ok(())
                })
                .await
                .unwrap();
            let deleted = store.agents_updates(delayed_page.revision).await.unwrap();
            assert_eq!(deleted.removed, vec![1.into()]);
            assert!(deleted.revision > delayed_page.agents[0].version);
            assert_eq!(
                store.agents_updates(delayed_page.revision).await.unwrap(),
                deleted
            );
            assert_eq!(
                store.agents(AgentsQuery::default()).await.unwrap().agents,
                []
            );
        });
    }
}
