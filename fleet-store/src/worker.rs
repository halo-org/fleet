use crate::{
    Listeners, StoreError, Topic, data,
    records::{self, AgentRecord, Records},
};
use fleet_core::{ModelResponseChunk, Wake};
use futures_channel::oneshot;
use redb::ReadableDatabase as _;
use std::{collections::BTreeMap, sync::mpsc, thread};

pub struct Worker {
    pub database: redb::Database,
    pub listeners: Listeners,
    pub previews: BTreeMap<u128, crate::preview::PreviewState>,
    pub wake_sequence: u128,
    pub wakes: BTreeMap<u128, (u128, Wake)>,
}

pub struct WorkerHandle {
    sender: Option<mpsc::Sender<Job>>,
    thread: Option<thread::JoinHandle<()>>,
}

type Job = Box<dyn FnOnce(&mut Worker) + Send>;

impl WorkerHandle {
    pub fn new(database: redb::Database, listeners: Listeners) -> Result<Self, StoreError> {
        let (sender, receiver) = mpsc::channel::<Job>();
        let thread = thread::Builder::new()
            .name("fleet-store".into())
            .spawn(move || {
                let mut worker = Worker {
                    database,
                    listeners,
                    previews: BTreeMap::new(),
                    wake_sequence: 0,
                    wakes: BTreeMap::new(),
                };
                for job in receiver {
                    job(&mut worker);
                }
            })
            .map_err(StoreError::Io)?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    pub async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Worker) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(Box::new(move |worker| {
                drop(sender.send(operation(worker)));
            }))
            .map_err(|_| StoreError::Closed)?;
        receiver.await.map_err(|_| StoreError::Closed)?
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

impl Worker {
    pub fn mutate<T>(
        &mut self,
        agent_id: u128,
        operation: impl FnOnce(
            &mut Records<redb::Table<'_, &[u8], &[u8]>>,
            &mut AgentRecord,
        ) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let transaction = self.database.begin_write()?;
        let (result, latest_event, interrupted, summary_changed) = {
            let mut records = Records(transaction.open_table(records::RECORDS)?);
            let mut agent = records.agent(agent_id)?;
            let activity = (agent.turn_active, agent.interrupt_intent);
            agent.revision = agent
                .revision
                .checked_add(1)
                .expect("revisions do not overflow");
            let result = operation(&mut records, &mut agent)?;
            if activity != (agent.turn_active, agent.interrupt_intent) {
                records.change(
                    agent_id,
                    records::ACTIVITY,
                    &[],
                    Some(agent.activity_revision),
                    agent.revision,
                )?;
                agent.activity_revision = agent.revision;
            }
            records.save_agent(&agent)?;
            let summary_changed =
                crate::agents::sync_agent(&mut records, &agent, crate::agents::now_millis())?;
            (
                result,
                agent.latest_event,
                agent.interrupt_intent,
                summary_changed,
            )
        };
        transaction.commit()?;
        if summary_changed {
            self.listeners.notify(Topic::Agents);
            self.listeners.notify(Topic::Snoozes);
        }
        if let Some(preview) = self.previews.get_mut(&agent_id)
            && (Some(preview.input_cursor) != latest_event || interrupted)
        {
            preview.invalidate();
            self.listeners.notify(Topic::Preview(agent_id));
        }
        self.listeners.notify(Topic::Agent(agent_id));
        Ok(result)
    }

    pub fn notify_wake(&mut self, agent_id: u128, interrupt: bool) {
        self.wake_sequence = self
            .wake_sequence
            .checked_add(1)
            .expect("wake sequence does not overflow");
        let wake = if interrupt {
            Wake::Interrupt(agent_id.into())
        } else {
            Wake::Reconcile(agent_id.into())
        };
        self.wakes.insert(agent_id, (self.wake_sequence, wake));
        self.listeners.notify(Topic::Wakes);
    }

    pub fn publish(
        &mut self,
        agent_id: u128,
        input_cursor: u128,
        sequence: u32,
        chunk: &ModelResponseChunk,
    ) -> Result<(), StoreError> {
        if let Some(preview) = self.preview(agent_id, input_cursor)?
            && preview.apply(sequence, chunk)
        {
            self.listeners.notify(Topic::Preview(agent_id));
        }
        Ok(())
    }

    pub fn publish_compaction_started(
        &mut self,
        agent_id: u128,
        input_cursor: u128,
    ) -> Result<(), StoreError> {
        if let Some(preview) = self.preview(agent_id, input_cursor)?
            && preview.start_compaction()
        {
            self.listeners.notify(Topic::Preview(agent_id));
        }
        Ok(())
    }

    fn preview(
        &mut self,
        agent_id: u128,
        input_cursor: u128,
    ) -> Result<Option<&mut crate::preview::PreviewState>, StoreError> {
        let transaction = self.database.begin_read()?;
        let records = Records(transaction.open_table(records::RECORDS)?);
        let agent = records.agent(agent_id)?;
        if records
            .get::<records::EventRecord>(&records::key(
                records::EVENT,
                agent_id,
                &input_cursor.to_be_bytes(),
            ))?
            .is_none()
        {
            return Err(StoreError::Conflict("input cursor does not exist"));
        }
        if agent.latest_event != Some(input_cursor) || agent.interrupt_intent {
            return Ok(None);
        }
        let preview = self
            .previews
            .entry(agent_id)
            .or_insert_with(|| crate::preview::PreviewState::new(input_cursor));
        if input_cursor > preview.input_cursor {
            *preview = crate::preview::PreviewState::new(input_cursor);
        }
        Ok((input_cursor == preview.input_cursor).then_some(preview))
    }
}

pub fn append_tool_result(
    records: &mut Records<redb::Table<'_, &[u8], &[u8]>>,
    agent: &mut AgentRecord,
    result: data::ToolResult,
) -> Result<(), StoreError> {
    let key = records::key(
        records::TOOL_CALL,
        agent.agent.id,
        result.call_id.as_bytes(),
    );
    let mut call = records
        .get::<records::ToolCallRecord>(&key)?
        .ok_or(StoreError::Conflict("tool call does not exist"))?;
    if call.resolved_at.is_some() {
        return Err(StoreError::Conflict("tool call is already resolved"));
    }
    let call_id = result.call_id.clone();
    let cursor = records.append_event(agent, data::EventPayload::ToolResult(result))?;
    call.resolved_at = Some(cursor);
    records.change(
        agent.agent.id,
        records::TOOL_CALL,
        call_id.as_bytes(),
        Some(call.revision),
        agent.revision,
    )?;
    call.revision = agent.revision;
    records.put(&key, &call)?;
    records.remove(&records::key(
        records::PENDING_TOOL,
        agent.agent.id,
        call_id.as_bytes(),
    ))?;
    agent.pending_tool_calls = agent
        .pending_tool_calls
        .checked_sub(1)
        .expect("tool call is pending");
    if !call.call.is_async {
        agent.synchronous_tool_calls = agent
            .synchronous_tool_calls
            .checked_sub(1)
            .expect("synchronous call is pending");
    }
    Ok(())
}
