use crate::{
    Listeners, StoreError, Topic, WorkerHandle, data,
    records::{self, AgentRecord, EventRecord, MessageRecord, Records, Request, ToolCallRecord},
};
use fleet_core::{
    Agent, AgentId, AgentState, ClientStore, Compaction, Event, EventCursor, Message,
    ModelResponse, ModelResponseChunk, RuntimeStore, ToolCallId, ToolResult, Wake,
};
use futures_util::{Stream, stream};
use redb::ReadableDatabase as _;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    sync::Arc,
};

/// A transactional store for agents.
///
/// Clones share a database and worker thread. The database is owned by this process.
/// Dropping an operation's future does not cancel work already sent to the worker.
#[derive(Clone)]
pub struct Store {
    worker: Arc<WorkerHandle>,
    listeners: Listeners,
}

struct WakeStream {
    store: Store,
    sequence: Option<u128>,
    pending: VecDeque<Wake>,
    idle: bool,
}

impl Store {
    /// Opens a store, creating it if needed.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedFormat`](StoreError::UnsupportedFormat) for databases in another
    /// format. Returns other errors if the database cannot be opened or the worker cannot be
    /// started.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::new(redb::Database::create(path)?)
    }

    /// Creates an in-memory store.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be created or the worker cannot be started.
    pub fn memory() -> Result<Self, StoreError> {
        Self::new(
            redb::Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?,
        )
    }

    fn new(database: redb::Database) -> Result<Self, StoreError> {
        let transaction = database.begin_write()?;
        {
            let mut records = Records(transaction.open_table(records::RECORDS)?);
            match records.get::<u32>(&[])? {
                Some(5) => {}
                None => records.put(&[], &5_u32)?,
                Some(_) => return Err(StoreError::UnsupportedFormat),
            }
            crate::agents::expire(&mut records, crate::agents::now_millis())?;
        }
        transaction.open_table(crate::blob_records::BYTES)?;
        transaction.open_table(crate::blob_records::UPLOADS)?;
        transaction.open_table(crate::blob_records::CHUNKS)?;
        transaction.open_table(crate::role_records::ROLES)?;
        transaction.commit()?;
        let listeners = Listeners::default();
        let worker = Arc::new(WorkerHandle::new(database, listeners.clone())?);
        Ok(Self { worker, listeners })
    }

    pub(crate) const fn worker(&self) -> &Arc<WorkerHandle> {
        &self.worker
    }

    pub(crate) const fn listeners(&self) -> &Listeners {
        &self.listeners
    }

    pub(crate) async fn request(
        &self,
        idempotency_key: String,
        request: Request,
    ) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let request_key = records::key(records::REQUEST, 0, idempotency_key.as_bytes());
                let (agent_id, interrupt, wake, summary_changed) = {
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    if let Some(previous) = records.get::<Request>(&request_key)? {
                        return if previous == request {
                            Ok(())
                        } else {
                            Err(StoreError::IdempotencyConflict)
                        };
                    }
                    if crate::role_records::apply(&transaction, &mut records, &request)? {
                        records.put(&request_key, &request)?;
                        drop(records);
                        transaction.commit()?;
                        return Ok(());
                    }
                    let edit = apply_collection_request(&mut records, &request)?;
                    if let Some((id, changed, summary_changed)) = edit {
                        records.put(&request_key, &request)?;
                        drop(records);
                        transaction.commit()?;
                        if changed {
                            worker.listeners.notify(Topic::Agent(id));
                        }
                        if summary_changed {
                            worker.listeners.notify(Topic::Agents);
                            worker.listeners.notify(Topic::Snoozes);
                        }
                        return Ok(());
                    }
                    let (agent, wake) = apply_agent_request(&mut records, &request)?;
                    let id = agent.agent.id;
                    let summary_changed = crate::agents::sync_agent(
                        &mut records,
                        &agent,
                        crate::agents::now_millis(),
                    )?;
                    records.put(&request_key, &request)?;
                    (id, agent.interrupt_intent, wake, summary_changed)
                };
                transaction.commit()?;
                if summary_changed {
                    worker.listeners.notify(Topic::Agents);
                    worker.listeners.notify(Topic::Snoozes);
                }
                worker.listeners.notify(Topic::Agent(agent_id));
                if interrupt {
                    if let Some(preview) = worker.previews.get_mut(&agent_id) {
                        preview.invalidate();
                    }
                    worker.listeners.notify(Topic::Preview(agent_id));
                }
                if wake {
                    worker.notify_wake(agent_id, interrupt);
                }
                Ok(())
            })
            .await
    }
}

fn apply_collection_request(
    records: &mut Records<redb::Table<'_, &'static [u8], &'static [u8]>>,
    request: &Request,
) -> Result<Option<(u128, bool, bool)>, StoreError> {
    let (id, (changed, summary_changed)) = match request {
        Request::SetTitle {
            id,
            expected,
            title,
        } => (
            *id,
            crate::metadata::set_title(records, *id, *expected, title.as_deref())?,
        ),
        Request::SetPinned {
            id,
            expected,
            pinned,
        } => (
            *id,
            crate::metadata::set_pinned(
                records,
                *id,
                *expected,
                *pinned,
                crate::agents::now_millis(),
            )?,
        ),
        Request::SetInbox {
            id,
            expected,
            inbox,
            until,
        } => (
            *id,
            crate::agents::set_inbox(
                records,
                *id,
                *expected,
                *inbox,
                *until,
                crate::agents::now_millis(),
            )?,
        ),
        Request::CreateAgent(..)
        | Request::SendMessage(..)
        | Request::InterruptAgent(..)
        | Request::PutRole { .. }
        | Request::DeleteRole { .. } => {
            return Ok(None);
        }
    };
    Ok(Some((id, changed, summary_changed)))
}

fn apply_agent_request(
    records: &mut Records<redb::Table<'_, &'static [u8], &'static [u8]>>,
    request: &Request,
) -> Result<(AgentRecord, bool), StoreError> {
    let (mut agent, message, interrupt, create) = match request {
        Request::CreateAgent(agent, message) => {
            if records
                .get::<AgentRecord>(&records::key(records::AGENT, agent.id, &[]))?
                .is_some()
            {
                return Err(StoreError::Conflict("agent already exists"));
            }
            let agent = AgentRecord {
                agent: agent.clone(),
                latest_event: None,
                parked_at: None,
                interrupt_intent: false,
                revision: 1,
                metadata_revision: records::initial_metadata_revision(),
                activity_revision: 1,
                turn_active: false,
                next_position: 0,
                pending_messages: 0,
                pending_tool_calls: 0,
                synchronous_tool_calls: 0,
            };
            (agent, message.as_ref(), false, true)
        }
        Request::SendMessage(id, message) => (records.agent(*id)?, Some(message), false, false),
        Request::InterruptAgent(id, message) => {
            (records.agent(*id)?, message.as_ref(), true, false)
        }
        Request::SetTitle { .. }
        | Request::SetPinned { .. }
        | Request::SetInbox { .. }
        | Request::PutRole { .. }
        | Request::DeleteRole { .. } => {
            unreachable!()
        }
    };
    let id = agent.agent.id;
    if !create {
        agent.revision = agent
            .revision
            .checked_add(1)
            .expect("revisions do not overflow");
    }
    if create {
        records.change(id, records::AGENT, &[], None, agent.revision)?;
        records.change(id, records::ACTIVITY, &[], None, agent.revision)?;
    }
    if let Some(message) = message {
        records.push_message(&mut agent, message)?;
    }
    if interrupt && !agent.interrupt_intent {
        agent.interrupt_intent = true;
        records.change(
            id,
            records::ACTIVITY,
            &[],
            Some(agent.activity_revision),
            agent.revision,
        )?;
        agent.activity_revision = agent.revision;
    }
    records.save_agent(&agent)?;
    Ok((agent, interrupt || message.is_some()))
}

impl ClientStore for Store {
    type Error = StoreError;

    async fn create_agent(
        &self,
        idempotency_key: &str,
        agent: &Agent,
        message: Option<&Message>,
    ) -> Result<(), StoreError> {
        self.request(
            idempotency_key.into(),
            Request::CreateAgent(agent.clone().into(), message.cloned().map(Into::into)),
        )
        .await
    }

    async fn interrupt_agent(
        &self,
        idempotency_key: &str,
        agent_id: AgentId,
        message: Option<&Message>,
    ) -> Result<(), StoreError> {
        self.request(
            idempotency_key.into(),
            Request::InterruptAgent(agent_id.into(), message.cloned().map(Into::into)),
        )
        .await
    }

    async fn send_message(
        &self,
        idempotency_key: &str,
        agent_id: AgentId,
        message: &Message,
    ) -> Result<(), StoreError> {
        self.request(
            idempotency_key.into(),
            Request::SendMessage(agent_id.into(), message.clone().into()),
        )
        .await
    }
}

impl RuntimeStore for Store {
    type Error = StoreError;

    async fn agent(&self, agent_id: AgentId) -> Result<Option<Agent>, StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                Ok(records
                    .get::<AgentRecord>(&records::key(records::AGENT, agent_id.into(), &[]))?
                    .map(|record| record.agent.into()))
            })
            .await
    }

    async fn agent_state(&self, agent_id: AgentId) -> Result<Option<AgentState>, StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                Ok(records
                    .get::<AgentRecord>(&records::key(records::AGENT, agent_id.into(), &[]))?
                    .map(|agent| AgentState {
                        latest_event: agent.latest_event.map(Into::into),
                        parked_at: agent.parked_at.map(Into::into),
                        interrupt_intent: agent.interrupt_intent,
                        has_pending_messages: agent.pending_messages != 0,
                    }))
            })
            .await
    }

    fn wakes(&self) -> impl Stream<Item = Result<Wake, StoreError>> + Send {
        let state = WakeStream {
            store: self.clone(),
            sequence: None,
            pending: VecDeque::new(),
            idle: false,
        };
        stream::try_unfold(state, async move |mut state| {
            loop {
                if let Some(wake) = state.pending.pop_front() {
                    return Ok(Some((wake, state)));
                }
                let notified = state.store.listeners().listen(Topic::Wakes);
                let sequence = state.sequence;
                let (latest, wakes) = state
                    .store
                    .worker
                    .call(move |worker| {
                        let wakes = if let Some(sequence) = sequence {
                            worker
                                .wakes
                                .values()
                                .filter(|(updated, _)| *updated > sequence)
                                .map(|(_, wake)| *wake)
                                .collect::<VecDeque<_>>()
                        } else {
                            let transaction = worker.database.begin_read()?;
                            let records = Records(transaction.open_table(records::RECORDS)?);
                            records
                                .prefix::<(u128, bool)>(&[records::READY])?
                                .map(|item| {
                                    item.map(|(id, interrupt)| {
                                        if interrupt {
                                            Wake::Interrupt(id.into())
                                        } else {
                                            Wake::Reconcile(id.into())
                                        }
                                    })
                                })
                                .collect::<Result<_, _>>()?
                        };
                        Ok((worker.wake_sequence, wakes))
                    })
                    .await?;
                state.sequence = Some(latest);
                if !wakes.is_empty() {
                    state.pending = wakes;
                    state.idle = false;
                } else if !state.idle {
                    state.idle = true;
                    return Ok(Some((Wake::Idle, state)));
                } else {
                    notified.await;
                }
            }
        })
    }

    fn events(
        &self,
        agent_id: AgentId,
        from: EventCursor,
        until: Option<EventCursor>,
    ) -> impl Stream<Item = Result<Event, StoreError>> + Send {
        stream::try_unfold(
            (self.clone(), Some(u128::from(from)), VecDeque::new()),
            move |(store, mut from, mut pending)| async move {
                loop {
                    if let Some(event) = pending.pop_front() {
                        return Ok(Some((event, (store, from, pending))));
                    }
                    let Some(cursor) = from else {
                        return Ok(None);
                    };
                    pending = store
                        .worker
                        .call(move |worker| {
                            let transaction = worker.database.begin_read()?;
                            let records = Records(transaction.open_table(records::RECORDS)?);
                            records.agent(agent_id.into())?;
                            let prefix = records::key(records::EVENT, agent_id.into(), &[]);
                            let start = match until.map(u128::from) {
                                Some(until) if until >= cursor => return Ok(VecDeque::new()),
                                Some(until) => records::key(
                                    records::EVENT,
                                    agent_id.into(),
                                    &until
                                        .checked_add(1)
                                        .expect("cursor is below upper bound")
                                        .to_be_bytes(),
                                ),
                                None => prefix.clone(),
                            };
                            let end = cursor.checked_add(1).map_or_else(
                                || records::prefix_end(&prefix),
                                |next| {
                                    records::key(
                                        records::EVENT,
                                        agent_id.into(),
                                        &next.to_be_bytes(),
                                    )
                                },
                            );
                            records
                                .range::<EventRecord>(&start, &end)?
                                .rev()
                                .take(128)
                                .map(|event| event.map(|record| record.event.into()))
                                .collect::<Result<VecDeque<Event>, StoreError>>()
                        })
                        .await?;
                    from = pending
                        .back()
                        .and_then(|event| u128::from(event.cursor).checked_sub(1));
                    if pending.is_empty() {
                        return Ok(None);
                    }
                }
            },
        )
    }

    async fn settle_interrupt(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
        tool_results: &[ToolResult],
    ) -> Result<(), StoreError> {
        let results = tool_results
            .iter()
            .cloned()
            .map(Into::into)
            .collect::<Vec<data::ToolResult>>();
        self.worker()
            .call(move |worker| {
                worker.mutate(agent_id.into(), |records, agent| {
                    agent.check_latest_event(expected_latest_event.map(Into::into))?;
                    if !agent.interrupt_intent {
                        return Err(StoreError::Conflict("agent has no interrupt intent"));
                    }
                    let pending = records
                        .prefix::<String>(&records::key(
                            records::PENDING_TOOL,
                            agent_id.into(),
                            &[],
                        ))?
                        .collect::<Result<BTreeSet<_>, _>>()?;
                    let provided = results
                        .iter()
                        .map(|result| result.call_id.clone())
                        .collect::<BTreeSet<_>>();
                    if provided.len() != results.len() || pending != provided {
                        return Err(StoreError::Conflict(
                            "results must match all unresolved tool calls",
                        ));
                    }
                    for result in results {
                        crate::worker::append_tool_result(records, agent, result)?;
                    }
                    records.append_event(
                        agent,
                        data::EventPayload::TurnEnd(data::TurnEnd::Interrupted),
                    )?;
                    agent.interrupt_intent = false;
                    agent.parked_at = agent.latest_event;
                    Ok(())
                })
            })
            .await
    }

    async fn park(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
    ) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                worker.mutate(agent_id.into(), |_, agent| {
                    agent.check_latest_event(expected_latest_event.map(Into::into))?;
                    agent.parked_at = agent.latest_event;
                    Ok(())
                })
            })
            .await
    }

    async fn consume_messages(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
        allow_queued: bool,
    ) -> Result<bool, StoreError> {
        self.worker()
            .call(move |worker| {
                worker.mutate(agent_id.into(), |records, agent| {
                    agent.check_latest_event(expected_latest_event.map(Into::into))?;
                    if agent.interrupt_intent {
                        return Err(StoreError::Conflict("agent has an interrupt intent"));
                    }
                    let ids = records
                        .prefix::<u128>(&records::key(records::QUEUE, agent.agent.id, &[]))?
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut messages = Vec::new();
                    let mut queued = None;
                    for id in ids {
                        let message_key =
                            records::key(records::MESSAGE, agent.agent.id, &id.to_be_bytes());
                        let record = records
                            .get::<MessageRecord>(&message_key)?
                            .expect("queued message exists");
                        let message = record.message.as_ref().expect("queued message is pending");

                        match message.mode {
                            data::MessageMode::Steer => messages.push(record),
                            data::MessageMode::Queue if allow_queued && queued.is_none() => {
                                queued = Some(record);
                            }
                            data::MessageMode::Queue => {}
                        }
                    }
                    if messages.is_empty() {
                        messages.extend(queued);
                    }
                    let consumed = !messages.is_empty();
                    for mut record in messages {
                        let id = record.id;
                        let message_key =
                            records::key(records::MESSAGE, agent.agent.id, &id.to_be_bytes());
                        let message = record.message.take().expect("queued message is pending");
                        let cursor = records
                            .append_event(agent, data::EventPayload::Input(id, message.content))?;
                        records.change(
                            agent.agent.id,
                            records::MESSAGE,
                            &id.to_be_bytes(),
                            Some(record.revision),
                            agent.revision,
                        )?;
                        record.revision = agent.revision;
                        record.input_cursor = Some(cursor);
                        records.put(&message_key, &record)?;
                        records.remove(&records::key(
                            records::QUEUE,
                            agent.agent.id,
                            &record.position.to_be_bytes(),
                        ))?;
                        agent.pending_messages = agent
                            .pending_messages
                            .checked_sub(1)
                            .expect("message is pending");
                    }
                    Ok(consumed)
                })
            })
            .await
    }

    async fn publish_model_response_chunk(
        &self,
        agent_id: AgentId,
        input_cursor: EventCursor,
        sequence: u32,
        chunk: &ModelResponseChunk,
    ) -> Result<(), StoreError> {
        let chunk = chunk.clone();
        self.worker()
            .call(move |worker| {
                worker.publish(agent_id.into(), input_cursor.into(), sequence, &chunk)
            })
            .await
    }

    async fn publish_compaction_started(
        &self,
        agent_id: AgentId,
        input_cursor: EventCursor,
    ) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                worker.publish_compaction_started(agent_id.into(), input_cursor.into())
            })
            .await
    }

    async fn append_model_response(
        &self,
        agent_id: AgentId,
        expected_latest_event: EventCursor,
        ends_turn: bool,
        response: &ModelResponse,
    ) -> Result<(), StoreError> {
        let response: data::ModelResponse = response.clone().into();
        self.worker()
            .call(move |worker| {
                let notified = worker.mutate(agent_id.into(), |records, agent| {
                    agent.check_latest_event(Some(expected_latest_event.into()))?;
                    if agent.parked_at == agent.latest_event {
                        return Err(StoreError::Conflict("agent is parked at the input cursor"));
                    }
                    if agent.synchronous_tool_calls != 0 {
                        return Err(StoreError::Conflict(
                            "agent has unresolved synchronous tool calls",
                        ));
                    }
                    let mut ids = BTreeSet::new();
                    let mut tool_calls = Vec::new();
                    for output in &response.output {
                        if let data::ModelOutput::ToolCall(call) = output {
                            tool_calls.push(call.clone());
                            if !ids.insert(&call.id) {
                                return Err(StoreError::Conflict(
                                    "duplicate tool call ID in response",
                                ));
                            }
                            if records
                                .get::<ToolCallRecord>(&records::key(
                                    records::TOOL_CALL,
                                    agent.agent.id,
                                    call.id.as_bytes(),
                                ))?
                                .is_some()
                            {
                                return Err(StoreError::Conflict("tool call ID already exists"));
                            }
                        }
                    }
                    let cursor =
                        records.append_event(agent, data::EventPayload::ModelResponse(response))?;
                    for call in tool_calls {
                        let id = call.id.clone();
                        agent.pending_tool_calls = agent
                            .pending_tool_calls
                            .checked_add(1)
                            .expect("tool call count does not overflow");
                        if !call.is_async {
                            agent.synchronous_tool_calls = agent
                                .synchronous_tool_calls
                                .checked_add(1)
                                .expect("tool call count does not overflow");
                        }
                        records.put(
                            &records::key(records::TOOL_CALL, agent.agent.id, id.as_bytes()),
                            &ToolCallRecord {
                                call,
                                created_at: cursor,
                                resolved_at: None,
                                prepared: false,
                                revision: agent.revision,
                            },
                        )?;
                        records.put(
                            &records::key(records::PENDING_TOOL, agent.agent.id, id.as_bytes()),
                            &id,
                        )?;
                        records.change(
                            agent.agent.id,
                            records::TOOL_CALL,
                            id.as_bytes(),
                            None,
                            agent.revision,
                        )?;
                    }
                    if ends_turn {
                        let cursor = records.append_event(
                            agent,
                            data::EventPayload::TurnEnd(data::TurnEnd::Completed),
                        )?;
                        if agent.pending_messages == 0
                            && crate::metadata::agent_parent(&agent.agent.extensions).is_none()
                        {
                            crate::notification_records::record(records, agent.agent.id, cursor)?;
                            return Ok(true);
                        }
                    }
                    Ok(false)
                })?;
                if notified {
                    worker.listeners.notify(Topic::Notifications);
                }
                Ok(())
            })
            .await
    }

    async fn append_compaction(
        &self,
        agent_id: AgentId,
        expected_latest_event: EventCursor,
        compaction: &Compaction,
    ) -> Result<(), StoreError> {
        let compaction: data::Compaction = compaction.clone().into();
        self.worker()
            .call(move |worker| {
                worker.mutate(agent_id.into(), |records, agent| {
                    agent.check_latest_event(Some(expected_latest_event.into()))?;
                    if records
                        .get::<EventRecord>(&records::key(
                            records::EVENT,
                            agent.agent.id,
                            &compaction.last_event.to_be_bytes(),
                        ))?
                        .is_none()
                    {
                        return Err(StoreError::Conflict("compaction cursor does not exist"));
                    }
                    let pending = records
                        .prefix::<ToolCallRecord>(&records::key(
                            records::TOOL_CALL,
                            agent.agent.id,
                            &[],
                        ))?
                        .filter_map(|record| match record {
                            Ok(record)
                                if record.created_at <= compaction.last_event
                                    && record
                                        .resolved_at
                                        .is_none_or(|cursor| cursor > compaction.last_event) =>
                            {
                                Some(Ok((record.call.id.clone(), record.call)))
                            }
                            Ok(_) => None,
                            Err(error) => Some(Err(error)),
                        })
                        .collect::<Result<BTreeMap<_, _>, StoreError>>()?;
                    if pending.values().any(|call| !call.is_async) {
                        return Err(StoreError::Conflict(
                            "agent has unresolved synchronous tool calls",
                        ));
                    }
                    let provided = compaction
                        .pending_tool_calls
                        .iter()
                        .map(|call| (call.id.clone(), call.clone()))
                        .collect::<BTreeMap<_, _>>();
                    if provided.len() != compaction.pending_tool_calls.len() || provided != pending
                    {
                        return Err(StoreError::Conflict("pending tool calls do not match"));
                    }
                    let parked = agent.parked_at == agent.latest_event;
                    records.append_event(agent, data::EventPayload::Compaction(compaction))?;
                    if parked {
                        agent.parked_at = agent.latest_event;
                    }
                    Ok(())
                })
            })
            .await
    }

    async fn prepare_tool_call(
        &self,
        agent_id: AgentId,
        call_id: &ToolCallId,
    ) -> Result<bool, StoreError> {
        let call_id = call_id.clone();
        self.worker()
            .call(move |worker| {
                worker.mutate(agent_id.into(), |records, agent| {
                    let key = records::key(
                        records::TOOL_CALL,
                        agent.agent.id,
                        call_id.as_ref().as_bytes(),
                    );
                    let mut record = records
                        .get::<ToolCallRecord>(&key)?
                        .ok_or(StoreError::Conflict("tool call does not exist"))?;
                    if record.resolved_at.is_some() {
                        return Err(StoreError::Conflict("tool call is already resolved"));
                    }
                    let prepared = core::mem::replace(&mut record.prepared, true);
                    records.put(&key, &record)?;
                    Ok(prepared)
                })
            })
            .await
    }

    async fn append_tool_result(
        &self,
        agent_id: AgentId,
        result: &ToolResult,
    ) -> Result<(), StoreError> {
        let result: data::ToolResult = result.clone().into();
        self.worker()
            .call(move |worker| {
                worker.mutate(agent_id.into(), |records, agent| {
                    crate::worker::append_tool_result(records, agent, result)
                })
            })
            .await
    }
}
