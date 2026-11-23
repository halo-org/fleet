use crate::{
    Store, StoreError, Topic, data,
    records::{self, AgentRecord, EventRecord, MessageRecord, Records, Request, ToolCallRecord},
    scope_records::REQUESTS,
};
use fleet_core::{AgentId, ToolCall, ToolCallId};
use fleet_kit::{MessageCounts, Revision, ToolCallsErrorKind};
use redb::{ReadableDatabase as _, ReadableTable as _};

/// Access to an agent's own tool calls within a driving tool call.
///
/// Access closes once the driving call is absent, resolved or being interrupted.
#[derive(Clone)]
pub struct ToolCallAccess {
    store: Store,
    agent: AgentId,
    call: ToolCallId,
}

/// An agent's tool calls, seen from a driving tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallSnapshot {
    /// The revision at which this snapshot was read.
    pub revision: Revision,
    /// The tool calls in the model's context when it made the driving call, in event order.
    ///
    /// The context starts with the pending tool calls of the latest compaction before the driving
    /// call's response and ends with that response. It includes the driving call.
    pub context: Vec<ToolCall>,
    /// The agent's other unresolved tool calls, ordered by ID.
    pub pending_tool_calls: Vec<ToolCall>,
    /// Messages the agent had not seen when the driving call was made, including those sent since.
    ///
    /// Consuming a message does not change these counts.
    pub messages: MessageCounts,
}

type ReadRecords = Records<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>;

impl Store {
    /// Grants access to an agent's own tool calls within a driving tool call.
    #[must_use]
    pub fn tool_call_access(&self, agent: AgentId, call: ToolCallId) -> ToolCallAccess {
        ToolCallAccess {
            store: self.clone(),
            agent,
            call,
        }
    }
}

impl ToolCallAccess {
    /// Returns a snapshot once the agent's revision is newer than `after`.
    ///
    /// Without `after`, returns the current snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ScopeClosed`] once access closes and
    /// [`StoreError::InvalidRevision`] when `after` is newer than the agent's revision.
    pub async fn state(&self, after: Option<Revision>) -> Result<ToolCallSnapshot, StoreError> {
        let agent_id = u128::from(self.agent);
        loop {
            let notified = self.store.listeners().listen(Topic::Agent(agent_id));
            let call = self.call.to_string();
            let state = self
                .store
                .worker()
                .call(move |worker| {
                    let transaction = worker.database.begin_read()?;
                    let records = Records(transaction.open_table(records::RECORDS)?);
                    let agent = records.agent(agent_id)?;
                    let driving_call = driving_call(&records, &agent, &call)?;
                    match after.map(u128::from) {
                        Some(after) if after > agent.revision => Err(StoreError::InvalidRevision),
                        Some(after) if after == agent.revision => Ok(None),
                        _ => read_state(&records, &agent, &driving_call).map(Some),
                    }
                })
                .await?;
            if let Some(state) = state {
                return Ok(state);
            }
            notified.await;
        }
    }

    /// Interrupts unresolved tool calls of the agent.
    ///
    /// Resolved calls are ignored. Keys identify operations within the agent and driving call. A
    /// retry never repeats an operation and succeeds even after access closes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ScopeClosed`] for new operations once access closes,
    /// [`StoreError::IdempotencyConflict`] when a key is reused with different calls and
    /// [`StoreError::Conflict`] when the calls include the driving call.
    pub async fn interrupt(&self, key: &str, calls: &[ToolCallId]) -> Result<(), StoreError> {
        let agent_id = u128::from(self.agent);
        let call = self.call.to_string();
        let receipt = serde_json::to_string(&("tool_calls", agent_id, &call, key))?;
        let request =
            Request::InterruptToolCalls(agent_id, calls.iter().map(ToString::to_string).collect());
        self.store
            .worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let (interrupt, summary_changed) = {
                    let mut receipts = transaction.open_table(REQUESTS)?;
                    if let Some(previous) = receipts.get(receipt.as_str())? {
                        let previous: Request = serde_json::from_slice(previous.value())?;
                        return if previous == request {
                            Ok(())
                        } else {
                            Err(StoreError::IdempotencyConflict)
                        };
                    }
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    driving_call(&records, &records.agent(agent_id)?, &call)?;
                    if let Request::InterruptToolCalls(_, ids) = &request
                        && ids.contains(&call)
                    {
                        return Err(StoreError::Conflict(
                            "the driving call cannot be interrupted",
                        ));
                    }
                    let (agent, _) = crate::store::apply_agent_request(&mut records, &request)?;
                    let summary_changed = crate::agents::sync_agent(
                        &mut records,
                        &agent,
                        crate::agents::now_millis(),
                    )?;
                    receipts.insert(receipt.as_str(), serde_json::to_vec(&request)?.as_slice())?;
                    (agent.interrupt_intent, summary_changed)
                };
                transaction.commit()?;
                if summary_changed {
                    worker.listeners.notify(Topic::Agents);
                    worker.listeners.notify(Topic::Snoozes);
                }
                worker.listeners.notify(Topic::Agent(agent_id));
                worker.listeners.notify(Topic::AgentScopes);
                worker.notify_wake(agent_id, interrupt);
                Ok(())
            })
            .await
    }
}

fn read_state(
    records: &ReadRecords,
    agent: &AgentRecord,
    driving_call: &ToolCallRecord,
) -> Result<ToolCallSnapshot, StoreError> {
    let agent_id = agent.agent.id;

    let mut pending_tool_calls = Vec::new();
    for id in records.prefix::<String>(&records::key(records::PENDING_TOOL, agent_id, &[]))? {
        let id = id?;
        if id != driving_call.call.id {
            let record = tool_call(records, agent_id, &id)?.ok_or(StoreError::UnsupportedFormat)?;
            pending_tool_calls.push(record.call.into());
        }
    }

    let mut messages = MessageCounts {
        steering: consumed_since(
            agent.consumed_messages.steering,
            driving_call.consumed_messages.steering,
        )?,
        queued: consumed_since(
            agent.consumed_messages.queued,
            driving_call.consumed_messages.queued,
        )?,
    };
    for id in records.prefix::<u128>(&records::key(records::QUEUE, agent_id, &[]))? {
        let key = records::key(records::MESSAGE, agent_id, &id?.to_be_bytes());
        let record = records
            .get::<MessageRecord>(&key)?
            .ok_or(StoreError::UnsupportedFormat)?;
        let message = record.message.ok_or(StoreError::UnsupportedFormat)?;
        let count = match message.mode {
            crate::data::MessageMode::Steer => &mut messages.steering,
            crate::data::MessageMode::Queue => &mut messages.queued,
        };
        *count = count
            .checked_add(1)
            .expect("message count does not overflow");
    }

    Ok(ToolCallSnapshot {
        revision: agent.revision.into(),
        context: context(records, agent_id, driving_call.created_at)?,
        pending_tool_calls,
        messages,
    })
}

fn context(
    records: &ReadRecords,
    agent_id: u128,
    created_at: u128,
) -> Result<Vec<ToolCall>, StoreError> {
    let end = created_at
        .checked_add(1)
        .expect("event cursor does not overflow");
    let events = records.range::<EventRecord>(
        &records::key(records::EVENT, agent_id, &[]),
        &records::key(records::EVENT, agent_id, &end.to_be_bytes()),
    )?;
    let mut calls = Vec::new();
    let mut compaction = None::<data::Compaction>;
    for record in events.rev() {
        let event = record?.event;
        if compaction
            .as_ref()
            .is_some_and(|compaction| compaction.last_event >= event.cursor)
        {
            break;
        }
        match event.payload {
            data::EventPayload::ModelResponse(response) => {
                calls.extend(
                    response
                        .output
                        .into_iter()
                        .rev()
                        .filter_map(|output| match output {
                            data::ModelOutput::ToolCall(call) => Some(call),
                            _ => None,
                        }),
                );
            }
            data::EventPayload::Compaction(value) if compaction.is_none() => {
                compaction = Some(value);
            }
            _ => {}
        }
    }
    if let Some(compaction) = compaction {
        calls.extend(compaction.pending_tool_calls.into_iter().rev());
    }
    Ok(calls.into_iter().rev().map(Into::into).collect())
}

fn driving_call<T: redb::ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    agent: &AgentRecord,
    call: &str,
) -> Result<ToolCallRecord, StoreError> {
    tool_call(records, agent.agent.id, call)?
        .filter(|record| record.resolved_at.is_none() && !agent.interrupts_tool_call(record))
        .ok_or(StoreError::ScopeClosed)
}

fn tool_call<T: redb::ReadableTable<&'static [u8], &'static [u8]>>(
    records: &Records<T>,
    agent_id: u128,
    call: &str,
) -> Result<Option<ToolCallRecord>, StoreError> {
    records.get::<ToolCallRecord>(&records::key(records::TOOL_CALL, agent_id, call.as_bytes()))
}

fn consumed_since(now: u64, before: u64) -> Result<u64, StoreError> {
    now.checked_sub(before).ok_or(StoreError::UnsupportedFormat)
}

impl fleet_core::Error<ToolCallsErrorKind> for StoreError {
    fn kind(&self) -> ToolCallsErrorKind {
        match self {
            Self::ScopeClosed | Self::AgentNotFound => ToolCallsErrorKind::ScopeClosed,
            Self::Conflict(_) | Self::IdempotencyConflict | Self::InvalidRevision => {
                ToolCallsErrorKind::Conflict
            }
            _ => ToolCallsErrorKind::Other,
        }
    }
}
