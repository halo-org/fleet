use crate::{
    WasmState,
    bindings::{agents, types},
};
use core::{fmt, num::NonZeroUsize};
use fleet_core::{
    AgentId, Compaction, Configured, ContentBlock, Event, EventCursor, EventPayload, JsonObject,
    MessageId, RoleName, ToolCallId, TurnEnd,
};
use fleet_kit::{Agents, AgentsErrorKind, EventDirection, EventQuery, TurnEndEvent};
use futures_util::future::BoxFuture;
use std::sync::Arc;
use wasmtime::component::{Accessor, HasSelf};

/// Agent access available to WebAssembly providers.
#[derive(Clone)]
pub struct WasmAgents {
    factory: Arc<AgentsFactory>,
    access: Option<Arc<dyn AgentsObject>>,
}

type AgentsFactory = dyn Fn(AgentId, Option<ToolCallId>) -> Arc<dyn AgentsObject> + Send + Sync;

trait AgentsObject: Send + Sync {
    fn get(&self, id: AgentId) -> BoxFuture<'_, Result<agents::Agent, agents::AgentsError>>;
    fn events(
        &self,
        id: AgentId,
        query: EventQuery,
    ) -> BoxFuture<'_, Result<Vec<agents::Event>, agents::AgentsError>>;
    fn wait_for_turn_end(
        &self,
        id: AgentId,
        message: MessageId,
    ) -> BoxFuture<'_, Result<agents::TurnEndEvent, agents::AgentsError>>;
    fn create(
        &self,
        key: String,
        role: Configured<RoleName>,
        extensions: JsonObject,
    ) -> BoxFuture<'_, Result<types::AgentId, agents::AgentsError>>;
    fn send(
        &self,
        key: String,
        id: AgentId,
        content: Vec<ContentBlock>,
    ) -> BoxFuture<'_, Result<agents::MessageId, agents::AgentsError>>;
}

impl WasmAgents {
    /// Supplies caller-scoped access, with mutation rights only within a driving call.
    pub fn new<A: Agents + Send + Sync + 'static>(
        factory: impl Fn(AgentId, Option<ToolCallId>) -> A + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Arc::new(move |agent, call| Arc::new(factory(agent, call))),
            access: None,
        }
    }

    pub(crate) fn for_agent(&self, agent: AgentId) -> Self {
        Self {
            factory: self.factory.clone(),
            access: Some((self.factory)(agent, None)),
        }
    }

    pub(crate) fn for_call(&self, agent: AgentId, call: ToolCallId) -> Self {
        Self {
            factory: self.factory.clone(),
            access: Some((self.factory)(agent, Some(call))),
        }
    }
}

impl fmt::Debug for WasmAgents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmAgents").finish_non_exhaustive()
    }
}

impl<A: Agents + Send + Sync> AgentsObject for A {
    fn get(&self, id: AgentId) -> BoxFuture<'_, Result<agents::Agent, agents::AgentsError>> {
        Box::pin(async move {
            let agent = Agents::get(self, id)
                .await
                .map_err(|source| error(&source))?;
            Ok(agents::Agent {
                id: agent.id.into(),
                role: types::Configured {
                    name: agent.role.name.into(),
                    config: crate::convert::json(&agent.role.config),
                },
                extensions: crate::convert::json(&agent.extensions),
            })
        })
    }

    fn events(
        &self,
        id: AgentId,
        query: EventQuery,
    ) -> BoxFuture<'_, Result<Vec<agents::Event>, agents::AgentsError>> {
        Box::pin(async move {
            Ok(Agents::events(self, id, query)
                .await
                .map_err(|source| error(&source))?
                .into_iter()
                .map(Into::into)
                .collect())
        })
    }

    fn wait_for_turn_end(
        &self,
        id: AgentId,
        message: MessageId,
    ) -> BoxFuture<'_, Result<agents::TurnEndEvent, agents::AgentsError>> {
        Box::pin(async move {
            Agents::wait_for_turn_end(self, id, message)
                .await
                .map(Into::into)
                .map_err(|source| error(&source))
        })
    }

    fn create(
        &self,
        key: String,
        role: Configured<RoleName>,
        extensions: JsonObject,
    ) -> BoxFuture<'_, Result<types::AgentId, agents::AgentsError>> {
        Box::pin(async move {
            Agents::create(self, &key, &role, &extensions)
                .await
                .map(Into::into)
                .map_err(|source| error(&source))
        })
    }

    fn send(
        &self,
        key: String,
        id: AgentId,
        content: Vec<ContentBlock>,
    ) -> BoxFuture<'_, Result<agents::MessageId, agents::AgentsError>> {
        Box::pin(async move {
            Agents::send(self, &key, id, &content)
                .await
                .map(Into::into)
                .map_err(|source| error(&source))
        })
    }
}

impl agents::Host for WasmState {}

impl agents::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn get(
        store: &Accessor<WasmState, Self>,
        id: types::AgentId,
    ) -> Result<agents::Agent, agents::AgentsError> {
        host(store)?.get(id.into()).await
    }

    async fn events(
        store: &Accessor<WasmState, Self>,
        id: types::AgentId,
        query: agents::EventQuery,
    ) -> Result<Vec<agents::Event>, agents::AgentsError> {
        host(store)?.events(id.into(), query.try_into()?).await
    }

    async fn wait_for_turn_end(
        store: &Accessor<WasmState, Self>,
        id: types::AgentId,
        message: agents::MessageId,
    ) -> Result<agents::TurnEndEvent, agents::AgentsError> {
        host(store)?
            .wait_for_turn_end(id.into(), message.into())
            .await
    }

    async fn create(
        store: &Accessor<WasmState, Self>,
        key: String,
        role: types::Configured,
        extensions: String,
    ) -> Result<types::AgentId, agents::AgentsError> {
        let role = Configured {
            name: role.name.into(),
            config: object(&role.config)?,
        };
        host(store)?.create(key, role, object(&extensions)?).await
    }

    async fn send(
        store: &Accessor<WasmState, Self>,
        key: String,
        id: types::AgentId,
        content: Vec<types::ContentBlock>,
    ) -> Result<agents::MessageId, agents::AgentsError> {
        host(store)?
            .send(
                key,
                id.into(),
                content.into_iter().map(Into::into).collect(),
            )
            .await
    }
}

fn host(
    store: &Accessor<WasmState, HasSelf<WasmState>>,
) -> Result<Arc<dyn AgentsObject>, agents::AgentsError> {
    store
        .with(|mut access| access.get().agents())
        .and_then(|host| host.access)
        .ok_or_else(|| other("agents are not available in this operation".into()))
}

fn object(value: &str) -> Result<JsonObject, agents::AgentsError> {
    serde_json::from_str(value).map_err(|source| other(source.to_string()))
}

const fn other(message: String) -> agents::AgentsError {
    agents::AgentsError {
        kind: agents::AgentsErrorKind::Other,
        message,
    }
}

fn error(error: &impl fleet_core::Error<AgentsErrorKind>) -> agents::AgentsError {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    agents::AgentsError {
        kind: match error.kind() {
            AgentsErrorKind::AgentNotFound => agents::AgentsErrorKind::AgentNotFound,
            AgentsErrorKind::MessageNotFound => agents::AgentsErrorKind::MessageNotFound,
            AgentsErrorKind::AccessDenied => agents::AgentsErrorKind::AccessDenied,
            AgentsErrorKind::ScopeClosed => agents::AgentsErrorKind::ScopeClosed,
            AgentsErrorKind::Conflict => agents::AgentsErrorKind::Conflict,
            AgentsErrorKind::Other => agents::AgentsErrorKind::Other,
        },
        message,
    }
}
impl TryFrom<agents::EventQuery> for EventQuery {
    type Error = agents::AgentsError;

    fn try_from(value: agents::EventQuery) -> Result<Self, Self::Error> {
        Ok(Self {
            from: value.from.map(Into::into),
            until: value.until.map(Into::into),
            direction: match value.direction {
                agents::EventDirection::Forward => EventDirection::Forward,
                agents::EventDirection::Backward => EventDirection::Backward,
            },
            limit: usize::try_from(value.limit)
                .ok()
                .and_then(NonZeroUsize::new)
                .ok_or_else(|| other("event limit must be positive".into()))?,
            since: None,
        })
    }
}

impl From<Event> for agents::Event {
    fn from(value: Event) -> Self {
        Self {
            cursor: value.cursor.into(),
            payload: match value.payload {
                EventPayload::Input(id, content) => agents::EventPayload::Input((
                    id.into(),
                    content.iter().map(Into::into).collect(),
                )),
                EventPayload::ModelResponse(response) => {
                    agents::EventPayload::ModelResponse((&response).into())
                }
                EventPayload::ToolResult(result) => {
                    agents::EventPayload::ToolResult((&result).into())
                }
                EventPayload::Compaction(compaction) => {
                    agents::EventPayload::Compaction(compaction.into())
                }
                EventPayload::TurnEnd(end) => agents::EventPayload::TurnEnd(end.into()),
            },
        }
    }
}

impl From<Compaction> for agents::Compaction {
    fn from(value: Compaction) -> Self {
        Self {
            last_event: value.last_event.into(),
            input: value.input.iter().map(Into::into).collect(),
            pending_tool_calls: value.pending_tool_calls.iter().map(Into::into).collect(),
            usage: (&value.usage).into(),
        }
    }
}

impl From<TurnEndEvent> for agents::TurnEndEvent {
    fn from(value: TurnEndEvent) -> Self {
        Self {
            cursor: value.cursor.into(),
            payload: value.payload.into(),
        }
    }
}

impl From<TurnEnd> for agents::TurnEnd {
    fn from(value: TurnEnd) -> Self {
        match value {
            TurnEnd::Completed => Self::Completed,
            TurnEnd::Interrupted => Self::Interrupted,
        }
    }
}

impl From<MessageId> for agents::MessageId {
    fn from(id: MessageId) -> Self {
        let id = u128::from(id);
        Self {
            high: u64::try_from(id >> 64).expect("high bits fit in u64"),
            low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit in u64"),
        }
    }
}

impl From<agents::MessageId> for MessageId {
    fn from(id: agents::MessageId) -> Self {
        ((u128::from(id.high) << 64) | u128::from(id.low)).into()
    }
}

impl From<EventCursor> for agents::EventCursor {
    fn from(cursor: EventCursor) -> Self {
        let cursor = u128::from(cursor);
        Self {
            high: u64::try_from(cursor >> 64).expect("high bits fit in u64"),
            low: u64::try_from(cursor & u128::from(u64::MAX)).expect("low bits fit in u64"),
        }
    }
}

impl From<agents::EventCursor> for EventCursor {
    fn from(cursor: agents::EventCursor) -> Self {
        ((u128::from(cursor.high) << 64) | u128::from(cursor.low)).into()
    }
}
