use crate::{StoreError, data};
use redb::{ReadableTable, Table, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const RECORDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("records");
pub const AGENT: u8 = 1;
pub const EVENT: u8 = 2;
pub const MESSAGE: u8 = 3;
pub const QUEUE: u8 = 4;
pub const TOOL_CALL: u8 = 5;
pub const PENDING_TOOL: u8 = 6;
pub const TURN_END: u8 = 7;
pub const READY: u8 = 8;
pub const CHANGE: u8 = 9;
pub const REQUEST: u8 = 10;
pub const ACTIVITY: u8 = 11;
pub const RESOURCE_CHANGE: u8 = 12;

pub struct Records<T>(pub T);

#[derive(Serialize, Deserialize)]
pub struct AgentRecord {
    pub agent: data::Agent,
    pub latest_event: Option<u128>,
    pub parked_at: Option<u128>,
    pub interrupt_intent: bool,
    pub revision: u128,
    #[serde(default = "initial_metadata_revision")]
    pub metadata_revision: u128,
    pub activity_revision: u128,
    pub turn_active: bool,
    pub next_position: u128,
    pub pending_messages: u64,
    pub pending_tool_calls: u64,
    pub synchronous_tool_calls: u64,
}

pub const fn initial_metadata_revision() -> u128 {
    1
}

#[derive(Serialize, Deserialize)]
pub struct EventRecord {
    pub event: data::Event,
    pub revision: u128,
}

#[derive(Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: u128,
    pub message: Option<data::Message>,
    pub position: u128,
    pub input_cursor: Option<u128>,
    pub revision: u128,
}

#[derive(Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub call: data::ToolCall,
    pub created_at: u128,
    pub resolved_at: Option<u128>,
    pub prepared: bool,
    pub revision: u128,
}

#[derive(Serialize, Deserialize)]
pub struct Change {
    pub revision: u128,
    pub kind: u8,
    pub suffix: Vec<u8>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    CreateAgent(data::Agent, Option<data::Message>),
    SendMessage(u128, data::Message),
    InterruptAgent(u128, Option<data::Message>),
    SetTitle {
        id: u128,
        expected: u128,
        title: Option<String>,
    },
    SetInbox {
        id: u128,
        expected: u128,
        inbox: u8,
        until: Option<u64>,
    },
}

impl<T: ReadableTable<&'static [u8], &'static [u8]>> Records<T> {
    pub fn get<V: DeserializeOwned>(&self, key: &[u8]) -> Result<Option<V>, StoreError> {
        self.0
            .get(key)?
            .map(|value| serde_json::from_slice(value.value()).map_err(Into::into))
            .transpose()
    }

    pub fn range<'a, V: DeserializeOwned + 'a>(
        &'a self,
        start: &[u8],
        end: &[u8],
    ) -> Result<
        impl DoubleEndedIterator<Item = Result<V, StoreError>> + 'a + use<'a, T, V>,
        StoreError,
    > {
        Ok(self.0.range(start..end)?.map(|entry| {
            let (_, value) = entry?;
            Ok(serde_json::from_slice(value.value())?)
        }))
    }

    pub fn prefix<'a, V: DeserializeOwned + 'a>(
        &'a self,
        prefix: &[u8],
    ) -> Result<
        impl DoubleEndedIterator<Item = Result<V, StoreError>> + 'a + use<'a, T, V>,
        StoreError,
    > {
        self.range(prefix, &prefix_end(prefix))
    }

    pub fn agent(&self, id: u128) -> Result<AgentRecord, StoreError> {
        self.get(&key(AGENT, id, &[]))?
            .ok_or(StoreError::AgentNotFound)
    }
}

impl Records<Table<'_, &'static [u8], &'static [u8]>> {
    pub fn push_message(
        &mut self,
        agent: &mut AgentRecord,
        message: &data::Message,
    ) -> Result<(), StoreError> {
        let id = agent.agent.id;
        let message_key = key(MESSAGE, id, &message.id.to_be_bytes());
        if self.get::<MessageRecord>(&message_key)?.is_some() {
            return Err(StoreError::Conflict("message ID already exists"));
        }
        let position = agent.next_position;
        agent.next_position = position
            .checked_add(1)
            .expect("message positions do not overflow");
        agent.pending_messages = agent
            .pending_messages
            .checked_add(1)
            .expect("message count does not overflow");
        self.put(
            &message_key,
            &MessageRecord {
                id: message.id,
                message: Some(message.clone()),
                position,
                input_cursor: None,
                revision: agent.revision,
            },
        )?;
        self.put(&key(QUEUE, id, &position.to_be_bytes()), &message.id)?;
        self.change(id, MESSAGE, &message.id.to_be_bytes(), None, agent.revision)?;
        Ok(())
    }

    pub fn put<V: Serialize>(&mut self, key: &[u8], value: &V) -> Result<(), StoreError> {
        self.0.insert(key, serde_json::to_vec(value)?.as_slice())?;
        Ok(())
    }

    pub fn remove(&mut self, key: &[u8]) -> Result<(), StoreError> {
        self.0.remove(key)?;
        Ok(())
    }

    pub fn change(
        &mut self,
        agent_id: u128,
        kind: u8,
        suffix: &[u8],
        previous: Option<u128>,
        revision: u128,
    ) -> Result<(), StoreError> {
        let change_keys = |revision: u128| {
            [
                key(
                    CHANGE,
                    agent_id,
                    &[revision.to_be_bytes().as_slice(), &[kind], suffix].concat(),
                ),
                key(
                    RESOURCE_CHANGE,
                    agent_id,
                    &[&[kind], revision.to_be_bytes().as_slice(), suffix].concat(),
                ),
            ]
        };
        if let Some(previous) = previous {
            for key in change_keys(previous) {
                self.remove(&key)?;
            }
        }
        let change = Change {
            revision,
            kind,
            suffix: suffix.to_vec(),
        };
        for key in change_keys(revision) {
            self.put(&key, &change)?;
        }
        Ok(())
    }

    /// Commits a metadata-only edit within the caller's transaction.
    pub fn update_metadata(
        &mut self,
        agent: &mut AgentRecord,
        metadata: data::Agent,
    ) -> Result<bool, StoreError> {
        if metadata.id != agent.agent.id {
            return Err(StoreError::Conflict("agent ID cannot change"));
        }
        if metadata == agent.agent {
            return Ok(false);
        }
        agent.revision = agent
            .revision
            .checked_add(1)
            .expect("revisions do not overflow");
        self.change(
            agent.agent.id,
            AGENT,
            &[],
            Some(agent.metadata_revision),
            agent.revision,
        )?;
        agent.metadata_revision = agent.revision;
        agent.agent = metadata;
        self.save_agent(agent)?;
        Ok(true)
    }

    pub fn save_agent(&mut self, agent: &AgentRecord) -> Result<(), StoreError> {
        let id = agent.agent.id;
        self.put(&key(AGENT, id, &[]), agent)?;
        let ready = agent.latest_event != agent.parked_at
            || agent.interrupt_intent
            || agent.pending_messages != 0
            || agent.pending_tool_calls != 0;
        if ready {
            self.put(&key(READY, id, &[]), &(id, agent.interrupt_intent))?;
        } else {
            self.remove(&key(READY, id, &[]))?;
        }
        Ok(())
    }

    pub fn append_event(
        &mut self,
        agent: &mut AgentRecord,
        payload: data::EventPayload,
    ) -> Result<u128, StoreError> {
        let cursor = agent.latest_event.map_or(0, |cursor| {
            cursor
                .checked_add(1)
                .expect("event cursors do not overflow")
        });
        match &payload {
            data::EventPayload::Input(..)
            | data::EventPayload::ModelResponse(_)
            | data::EventPayload::ToolResult(_) => agent.turn_active = true,
            data::EventPayload::TurnEnd(turn_end) => {
                agent.turn_active = false;
                self.put(
                    &key(TURN_END, agent.agent.id, &cursor.to_be_bytes()),
                    &(cursor, turn_end),
                )?;
            }
            data::EventPayload::Compaction(_) => {}
        }
        self.put(
            &key(EVENT, agent.agent.id, &cursor.to_be_bytes()),
            &EventRecord {
                event: data::Event { cursor, payload },
                revision: agent.revision,
            },
        )?;
        self.change(
            agent.agent.id,
            EVENT,
            &cursor.to_be_bytes(),
            None,
            agent.revision,
        )?;
        agent.latest_event = Some(cursor);
        Ok(cursor)
    }
}

impl AgentRecord {
    pub fn check_latest_event(&self, expected: Option<u128>) -> Result<(), StoreError> {
        if self.latest_event != expected {
            return Err(StoreError::Conflict("latest event does not match"));
        }
        Ok(())
    }
}

pub fn key(kind: u8, agent_id: u128, suffix: &[u8]) -> Vec<u8> {
    [&[kind], agent_id.to_be_bytes().as_slice(), suffix].concat()
}

pub fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let length = prefix
        .iter()
        .rposition(|byte| *byte != u8::MAX)
        .expect("record prefix has a successor");
    let mut end = prefix
        .iter()
        .take(length.checked_add(1).expect("prefix length fits usize"))
        .copied()
        .collect::<Vec<_>>();
    let last = end.last_mut().expect("prefix is not empty");
    *last = last.checked_add(1).expect("prefix has a successor");
    end
}
