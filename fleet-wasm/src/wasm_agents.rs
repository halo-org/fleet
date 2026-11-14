use crate::{
    WasmState,
    bindings::{agents, types},
};
use core::{fmt, num::NonZeroUsize};
use fleet_core::{
    AgentId, Compaction, Configured, ContentBlock, Event, EventCursor, EventPayload, JsonObject,
    MessageId, RoleName, ToolCallId, TurnEnd,
};
use fleet_kit::{
    EventDirection, EventQuery, EventRecord, Queries, QueriesErrorKind, QueryUpdate, Subagents,
    TurnEndEvent,
};
use futures_util::future::BoxFuture;
use std::sync::Arc;
use wasmtime::component::{Accessor, HasSelf};

/// Agents available to a WebAssembly provider.
///
/// Every call can read agents. Tool calls can create agents through the call factory.
#[derive(Clone)]
pub struct WasmAgents {
    provider: Arc<dyn AgentsObject>,
    calls: Option<Arc<SubagentsFactory>>,
    call: Option<Arc<dyn SubagentsObject>>,
}

type SubagentsFactory = dyn Fn(AgentId, ToolCallId) -> Arc<dyn SubagentsObject> + Send + Sync;

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
}

trait SubagentsObject: Send + Sync {
    fn create(
        &self,
        role: Configured<RoleName>,
        extensions: JsonObject,
        content: Vec<ContentBlock>,
    ) -> BoxFuture<'_, Result<agents::CreatedAgent, agents::AgentsError>>;
}

impl WasmAgents {
    /// Wraps agent queries. This does not allow agents to be created.
    pub fn new(queries: impl Queries + Send + Sync + 'static) -> Self {
        Self {
            provider: Arc::new(queries),
            calls: None,
            call: None,
        }
    }

    /// Supplies a factory for creating agents on behalf of tool calls.
    #[must_use]
    pub fn with_calls<S: Subagents + Send + Sync + 'static>(
        mut self,
        factory: impl Fn(AgentId, ToolCallId) -> S + Send + Sync + 'static,
    ) -> Self {
        self.calls = Some(Arc::new(move |agent, call| {
            Arc::new(factory(agent, call)) as Arc<dyn SubagentsObject>
        }));
        self
    }

    pub(crate) fn for_call(&self, agent: AgentId, call: ToolCallId) -> Self {
        Self {
            provider: self.provider.clone(),
            calls: None,
            call: self.calls.as_ref().map(|factory| factory(agent, call)),
        }
    }
}

impl fmt::Debug for WasmAgents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmAgents").finish_non_exhaustive()
    }
}

impl<Q: Queries + Send + Sync> AgentsObject for Q {
    fn get(&self, id: AgentId) -> BoxFuture<'_, Result<agents::Agent, agents::AgentsError>> {
        Box::pin(async move {
            let agent = match Queries::agent(self, id, None)
                .await
                .map_err(|source| error(&source))?
                .value
            {
                QueryUpdate::Changed(metadata) => metadata.agent,
                QueryUpdate::Unchanged => {
                    return Err(other("agent metadata is unavailable".into()));
                }
            };
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
            Ok(Queries::events(self, id, query)
                .await
                .map_err(|source| error(&source))?
                .value
                .into_iter()
                .filter_map(|record| match record {
                    EventRecord::Settled(event) => Some(event.into()),
                    EventRecord::Deleted(_) => None,
                })
                .collect())
        })
    }

    fn wait_for_turn_end(
        &self,
        id: AgentId,
        message: MessageId,
    ) -> BoxFuture<'_, Result<agents::TurnEndEvent, agents::AgentsError>> {
        Box::pin(async move {
            Queries::wait_for_turn_end(self, id, message)
                .await
                .map(Into::into)
                .map_err(|source| error(&source))
        })
    }
}

impl<S: Subagents + Send + Sync> SubagentsObject for S {
    fn create(
        &self,
        role: Configured<RoleName>,
        extensions: JsonObject,
        content: Vec<ContentBlock>,
    ) -> BoxFuture<'_, Result<agents::CreatedAgent, agents::AgentsError>> {
        Box::pin(async move {
            let (agent, message) = Subagents::create(self, &role, &extensions, &content)
                .await
                .map_err(|source| other(message(&source)))?;
            Ok(agents::CreatedAgent {
                agent: agent.into(),
                message: message.into(),
            })
        })
    }
}

impl agents::Host for WasmState {}

impl agents::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn get(
        store: &Accessor<WasmState, Self>,
        id: types::AgentId,
    ) -> Result<agents::Agent, agents::AgentsError> {
        host(store)?.provider.get(id.into()).await
    }

    async fn events(
        store: &Accessor<WasmState, Self>,
        id: types::AgentId,
        query: agents::EventQuery,
    ) -> Result<Vec<agents::Event>, agents::AgentsError> {
        let query = query.try_into()?;
        host(store)?.provider.events(id.into(), query).await
    }

    async fn wait_for_turn_end(
        store: &Accessor<WasmState, Self>,
        id: types::AgentId,
        message: agents::MessageId,
    ) -> Result<agents::TurnEndEvent, agents::AgentsError> {
        host(store)?
            .provider
            .wait_for_turn_end(id.into(), message.into())
            .await
    }

    async fn create(
        store: &Accessor<WasmState, Self>,
        role: types::Configured,
        extensions: String,
        content: Vec<types::ContentBlock>,
    ) -> Result<agents::CreatedAgent, agents::AgentsError> {
        let call = host(store)?
            .call
            .ok_or_else(|| other("agents can only be created by tool calls".into()))?;
        let role = Configured {
            name: role.name.into(),
            config: object(&role.config)?,
        };
        let content = content.into_iter().map(Into::into).collect();
        call.create(role, object(&extensions)?, content).await
    }
}

fn host(
    store: &Accessor<WasmState, HasSelf<WasmState>>,
) -> Result<WasmAgents, agents::AgentsError> {
    store
        .with(|mut access| access.get().agents())
        .ok_or_else(|| other("agents are not available".into()))
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

fn error(error: &impl fleet_core::Error<QueriesErrorKind>) -> agents::AgentsError {
    agents::AgentsError {
        kind: match error.kind() {
            QueriesErrorKind::AgentNotFound => agents::AgentsErrorKind::AgentNotFound,
            QueriesErrorKind::MessageNotFound => agents::AgentsErrorKind::MessageNotFound,
            QueriesErrorKind::InvalidRevision
            | QueriesErrorKind::RevisionExpired
            | QueriesErrorKind::Other => agents::AgentsErrorKind::Other,
        },
        message: message(error),
    }
}

fn message(error: &dyn core::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
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
