use crate::{
    Store, StoreError, Topic,
    records::{self, AgentRecord, MessageRecord, Records, Request, ToolCallRecord},
    scope_records::{REQUESTS, SCOPES},
};
use fleet_core::{
    Agent, AgentId, Configured, ContentBlock, Event, JsonObject, Message, MessageId, MessageMode,
    RoleName, ToolCallId,
};
use fleet_kit::{
    AgentParent, Agents, AgentsErrorKind, EventQuery, EventRecord, Extension as _, Queries as _,
    TurnEndEvent,
};
use redb::{ReadableDatabase as _, ReadableTable as _, ReadableTableMetadata as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;

/// Agent access scoped to a caller and, optionally, its driving tool call.
#[derive(Clone)]
pub struct AgentAccess {
    store: Store,
    agent: AgentId,
    call: Option<ToolCallId>,
}

#[derive(Serialize, Deserialize)]
struct Scope {
    agent: u128,
    call: String,
    closing: bool,
}

impl Store {
    /// Grants metadata access to an agent and its ancestors and descendants.
    #[must_use]
    pub fn agent_access(&self, agent: AgentId) -> AgentAccess {
        AgentAccess {
            store: self.clone(),
            agent,
            call: None,
        }
    }

    /// Subscribes to changes relevant to scoped agent execution.
    pub fn agent_scopes_changed(&self) -> impl Future<Output = ()> + Send {
        self.listeners().listen(Topic::AgentScopes)
    }

    /// Stops execution whose driving call has ended. Returns the number of remaining scopes.
    ///
    /// Run after changes and on startup. Pending input is withdrawn before interruption. A scope
    /// remains closed until its agent settles, which schedules the release of its workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the scope records cannot be read or updated.
    pub async fn reconcile_agent_scopes(&self) -> Result<u64, StoreError> {
        self.worker()
            .call(|worker| {
                let transaction = worker.database.begin_write()?;
                let mut changed = Vec::new();
                let mut released = false;
                let remaining = {
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    let mut scopes = transaction.open_table(SCOPES)?;
                    let entries = scopes
                        .iter()?
                        .map(|entry| {
                            let (id, value) = entry?;
                            Ok((id.value(), serde_json::from_slice::<Scope>(value.value())?))
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    for (id, mut scope) in entries {
                        if !scope.closing
                            && scope_open(&records, &scopes, scope.agent, &scope.call)?
                        {
                            continue;
                        }
                        scope.closing = true;
                        let mut agent = records.agent(id)?;
                        let interrupt = !(agent.interrupt_intent
                            && agent.interrupt_includes_async_tool_calls)
                            && (agent.turn_active || agent.pending_tool_calls != 0);
                        if interrupt || agent.pending_messages != 0 {
                            let activity = agent.activity();
                            agent.revision = agent
                                .revision
                                .checked_add(1)
                                .expect("revisions do not overflow");
                            withdraw_messages(&mut records, &mut agent)?;
                            if interrupt {
                                agent.interrupt_intent = true;
                                agent.interrupt_includes_async_tool_calls = true;
                            }
                            records.update_activity(&mut agent, &activity)?;
                            records.save_agent(&agent)?;
                            crate::agents::sync_agent(
                                &mut records,
                                &agent,
                                crate::agents::now_millis(),
                            )?;
                            changed.push((id, agent.interrupt_intent));
                        }
                        if agent.idle() {
                            scopes.remove(id)?;
                            crate::release_records::schedule(
                                &mut records,
                                crate::release_records::ENDED,
                                crate::agents::now_millis(),
                                id,
                                agent.revision,
                            )?;
                            released = true;
                        } else {
                            scopes.insert(id, serde_json::to_vec(&scope)?.as_slice())?;
                        }
                    }
                    scopes.len()?
                };
                transaction.commit()?;
                if !changed.is_empty() {
                    worker.listeners.notify(Topic::Agents);
                }
                if released {
                    worker.listeners.notify(Topic::WorkspaceReleases);
                }
                for (id, interrupt) in changed {
                    worker.listeners.notify(Topic::Agent(id));
                    if interrupt {
                        if let Some(preview) = worker.previews.get_mut(&id) {
                            preview.invalidate();
                        }
                        worker.listeners.notify(Topic::Preview(id));
                    }
                    worker.notify_wake(id, interrupt);
                }
                Ok(remaining)
            })
            .await
    }
}

impl AgentAccess {
    /// Grants conversation access and execution owned by this tool call.
    #[must_use]
    pub fn for_call(mut self, call: ToolCallId) -> Self {
        self.call = Some(call);
        self
    }

    fn key(&self, key: &str) -> Result<String, StoreError> {
        let call = self.call.as_ref().ok_or(StoreError::ScopeClosed)?;
        Ok(serde_json::to_string(&(
            u128::from(self.agent),
            call.as_ref(),
            key,
        ))?)
    }

    async fn history(&self, agent: AgentId) -> Result<(), StoreError> {
        if self.call.is_none() {
            return Err(StoreError::ScopeClosed);
        }
        let caller = u128::from(self.agent);
        self.store
            .worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                if ancestor(&records, caller, agent.into())? {
                    Ok(())
                } else {
                    Err(StoreError::AgentDenied)
                }
            })
            .await
    }

    async fn mutate(&self, key: String, request: Request, id: u128) -> Result<(), StoreError> {
        let owner = u128::from(self.agent);
        let call = self
            .call
            .as_ref()
            .ok_or(StoreError::ScopeClosed)?
            .to_string();
        self.store
            .worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let wake = {
                    let mut receipts = transaction.open_table(REQUESTS)?;
                    if let Some(previous) = receipts.get(key.as_str())? {
                        let previous: Request = serde_json::from_slice(previous.value())?;
                        return if previous == request {
                            Ok(())
                        } else {
                            Err(StoreError::IdempotencyConflict)
                        };
                    }
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    let mut scopes = transaction.open_table(SCOPES)?;
                    if !scope_open(&records, &scopes, owner, &call)? {
                        return Err(StoreError::ScopeClosed);
                    }
                    if let Request::SendMessage(_, message) = &request {
                        let agent = records.agent(id)?;
                        if crate::metadata::agent_parent(&agent.agent.extensions)
                            != Some((owner, call.as_str()))
                        {
                            return Err(StoreError::AgentDenied);
                        }
                        for blob in crate::blob_records::content_ids(&message.content) {
                            if !crate::blob_records::readable(&records, owner, Some(&call), blob)? {
                                return Err(StoreError::BlobDenied);
                            }
                        }
                    }
                    let (agent, wake) = crate::store::apply_agent_request(&mut records, &request)?;
                    crate::agents::sync_agent(&mut records, &agent, crate::agents::now_millis())?;
                    scopes.insert(
                        id,
                        serde_json::to_vec(&Scope {
                            agent: owner,
                            call,
                            closing: false,
                        })?
                        .as_slice(),
                    )?;
                    receipts.insert(key.as_str(), serde_json::to_vec(&request)?.as_slice())?;
                    wake
                };
                transaction.commit()?;
                worker.listeners.notify(Topic::Agent(id));
                worker.listeners.notify(Topic::Agents);
                worker.listeners.notify(Topic::AgentScopes);
                if wake {
                    worker.notify_wake(id, false);
                }
                Ok(())
            })
            .await
    }
}

impl Agents for AgentAccess {
    type Error = StoreError;

    async fn get(&self, agent: AgentId) -> Result<Agent, StoreError> {
        let caller = u128::from(self.agent);
        self.store
            .worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                let id = agent.into();
                if !ancestor(&records, caller, id)? && !ancestor(&records, id, caller)? {
                    return Err(StoreError::AgentDenied);
                }
                Ok(records.agent(id)?.agent.into())
            })
            .await
    }

    async fn events(&self, agent: AgentId, query: EventQuery) -> Result<Vec<Event>, StoreError> {
        self.history(agent).await?;
        Ok(self
            .store
            .events(agent, query)
            .await?
            .value
            .into_iter()
            .filter_map(|record| match record {
                EventRecord::Settled(event) => Some(event),
                EventRecord::Deleted(_) => None,
            })
            .collect())
    }

    async fn wait_for_turn_end(
        &self,
        agent: AgentId,
        message: MessageId,
    ) -> Result<TurnEndEvent, StoreError> {
        self.history(agent).await?;
        self.store.wait_for_turn_end(agent, message).await
    }

    async fn create(
        &self,
        key: &str,
        role: &Configured<RoleName>,
        extensions: &JsonObject,
    ) -> Result<AgentId, StoreError> {
        let key = self.key(key)?;
        let id = identity(&key, b'a');
        let mut extensions = extensions.clone();
        AgentParent {
            agent: self.agent,
            call: self.call.clone().ok_or(StoreError::ScopeClosed)?,
        }
        .insert(&mut extensions)?;
        let agent = Agent {
            id: id.into(),
            role: role.clone(),
            extensions,
        };
        self.mutate(key, Request::CreateAgent(agent.into(), None), id)
            .await?;
        Ok(id.into())
    }

    async fn send(
        &self,
        key: &str,
        agent: AgentId,
        content: &[ContentBlock],
    ) -> Result<MessageId, StoreError> {
        let key = self.key(key)?;
        let id = identity(&key, b'm');
        let message = Message {
            id: id.into(),
            mode: MessageMode::Queue,
            content: content.to_vec(),
        };
        self.mutate(
            key,
            Request::SendMessage(agent.into(), message.into()),
            agent.into(),
        )
        .await?;
        Ok(id.into())
    }
}

fn withdraw_messages(
    records: &mut Records<redb::Table<'_, &'static [u8], &'static [u8]>>,
    agent: &mut AgentRecord,
) -> Result<(), StoreError> {
    let id = agent.agent.id;
    let messages = records
        .prefix::<u128>(&records::key(records::QUEUE, id, &[]))?
        .collect::<Result<Vec<_>, _>>()?;
    for message in messages {
        let key = records::key(records::MESSAGE, id, &message.to_be_bytes());
        let mut record = records
            .get::<MessageRecord>(&key)?
            .ok_or(StoreError::UnsupportedFormat)?;
        records.change(
            id,
            records::MESSAGE,
            &message.to_be_bytes(),
            Some(record.revision),
            agent.revision,
        )?;
        record.message = None;
        record.deleted = true;
        record.revision = agent.revision;
        records.remove(&records::key(
            records::QUEUE,
            id,
            &record.position.to_be_bytes(),
        ))?;
        records.put(&key, &record)?;
    }
    agent.pending_messages = 0;
    Ok(())
}

fn identity(key: &str, kind: u8) -> u128 {
    let digest = Sha256::new()
        .chain_update([kind])
        .chain_update(key)
        .finalize();
    u128::from_be_bytes(*digest.first_chunk().expect("digest contains 16 bytes"))
}

fn ancestor<T: redb::ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    ancestor: u128,
    mut agent: u128,
) -> Result<bool, StoreError> {
    let mut visited = BTreeSet::new();
    loop {
        let record = records.agent(agent)?;
        if agent == ancestor {
            return Ok(true);
        }
        if !visited.insert(agent) {
            return Err(StoreError::UnsupportedFormat);
        }
        let Some(parent) = crate::metadata::agent_parent(&record.agent.extensions) else {
            return Ok(false);
        };
        agent = parent.0;
    }
}

fn scope_open<T: redb::ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    scopes: &impl redb::ReadableTable<u128, &'static [u8]>,
    mut agent: u128,
    call: &str,
) -> Result<bool, StoreError> {
    let mut call = call.to_owned();
    let mut visited = BTreeSet::new();
    loop {
        let record = records.agent(agent)?;
        let Some(call_record) = records
            .get::<ToolCallRecord>(&records::key(records::TOOL_CALL, agent, call.as_bytes()))?
            .filter(|record| record.resolved_at.is_none())
        else {
            return Ok(false);
        };
        if record.interrupts_tool_call(&call_record) {
            return Ok(false);
        }
        if !visited.insert(agent) {
            return Err(StoreError::UnsupportedFormat);
        }
        let Some(scope) = scopes.get(agent)? else {
            return Ok(crate::metadata::agent_parent(&record.agent.extensions).is_none());
        };
        let scope: Scope = serde_json::from_slice(scope.value())?;
        if scope.closing {
            return Ok(false);
        }
        agent = scope.agent;
        call = scope.call;
    }
}

impl fleet_core::Error<AgentsErrorKind> for StoreError {
    fn kind(&self) -> AgentsErrorKind {
        match self {
            Self::AgentNotFound => AgentsErrorKind::AgentNotFound,
            Self::MessageNotFound => AgentsErrorKind::MessageNotFound,
            Self::AgentDenied | Self::BlobDenied => AgentsErrorKind::AccessDenied,
            Self::ScopeClosed => AgentsErrorKind::ScopeClosed,
            Self::Conflict(_) | Self::IdempotencyConflict => AgentsErrorKind::Conflict,
            _ => AgentsErrorKind::Other,
        }
    }
}
