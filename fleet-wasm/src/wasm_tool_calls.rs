use crate::{WasmCapability, WasmState, bindings::tool_calls};
use core::fmt;
use fleet_core::{AgentId, ToolCallId};
use fleet_kit::{Revision, TaskStatus, ToolCalls, ToolCallsErrorKind, ToolCallsState};
use futures_util::future::BoxFuture;
use std::sync::Arc;
use wasmtime::component::{Accessor, HasSelf};

/// Access to an agent's own tool calls, available to tool calls of WebAssembly providers.
#[derive(Clone)]
pub struct WasmToolCalls {
    factory: Arc<ToolCallsFactory>,
    access: Option<Arc<dyn ToolCallsObject>>,
}

type ToolCallsFactory = dyn Fn(AgentId, ToolCallId) -> Arc<dyn ToolCallsObject> + Send + Sync;

trait ToolCallsObject: Send + Sync {
    fn state(
        &self,
        after: Option<Revision>,
        task_handles: Vec<String>,
    ) -> BoxFuture<'_, Result<tool_calls::ToolCallsState, tool_calls::ToolCallsError>>;
    fn interrupt(
        &self,
        key: String,
        calls: Vec<ToolCallId>,
    ) -> BoxFuture<'_, Result<(), tool_calls::ToolCallsError>>;
}

impl WasmToolCalls {
    /// Supplies access to an agent's own tool calls within a driving call.
    pub fn new<T: ToolCalls + Send + Sync + 'static>(
        factory: impl Fn(AgentId, ToolCallId) -> T + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Arc::new(move |agent, call| Arc::new(factory(agent, call))),
            access: None,
        }
    }

    pub(crate) fn for_call(&self, agent: AgentId, call: ToolCallId) -> Self {
        Self {
            factory: self.factory.clone(),
            access: Some((self.factory)(agent, call)),
        }
    }
}

impl fmt::Debug for WasmToolCalls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmToolCalls").finish_non_exhaustive()
    }
}

impl<T: ToolCalls + Send + Sync> ToolCallsObject for T {
    fn state(
        &self,
        after: Option<Revision>,
        task_handles: Vec<String>,
    ) -> BoxFuture<'_, Result<tool_calls::ToolCallsState, tool_calls::ToolCallsError>> {
        Box::pin(async move {
            ToolCalls::state(self, after, &task_handles)
                .await
                .map(Into::into)
                .map_err(|source| error(&source))
        })
    }

    fn interrupt(
        &self,
        key: String,
        calls: Vec<ToolCallId>,
    ) -> BoxFuture<'_, Result<(), tool_calls::ToolCallsError>> {
        Box::pin(async move {
            ToolCalls::interrupt(self, &key, &calls)
                .await
                .map_err(|source| error(&source))
        })
    }
}

impl tool_calls::Host for WasmState {}

impl tool_calls::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn state(
        store: &Accessor<WasmState, Self>,
        after: Option<tool_calls::Revision>,
        task_handles: Vec<String>,
    ) -> Result<tool_calls::ToolCallsState, tool_calls::ToolCallsError> {
        host(store)?
            .state(after.map(Into::into), task_handles)
            .await
    }

    async fn interrupt(
        store: &Accessor<WasmState, Self>,
        key: String,
        calls: Vec<String>,
    ) -> Result<(), tool_calls::ToolCallsError> {
        host(store)?
            .interrupt(key, calls.into_iter().map(Into::into).collect())
            .await
    }
}

fn host(
    store: &Accessor<WasmState, HasSelf<WasmState>>,
) -> Result<Arc<dyn ToolCallsObject>, tool_calls::ToolCallsError> {
    store.with(|mut access| {
        let state = access.get();
        state
            .tool_calls()
            .and_then(|host| host.access)
            .ok_or_else(|| tool_calls::ToolCallsError {
                kind: tool_calls::ToolCallsErrorKind::Other,
                message: state.unavailable(
                    WasmCapability::ToolCalls,
                    "tool calls are not available in this operation",
                ),
            })
    })
}

fn error(error: &impl fleet_core::Error<ToolCallsErrorKind>) -> tool_calls::ToolCallsError {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    tool_calls::ToolCallsError {
        kind: match error.kind() {
            ToolCallsErrorKind::ScopeClosed => tool_calls::ToolCallsErrorKind::ScopeClosed,
            ToolCallsErrorKind::Conflict => tool_calls::ToolCallsErrorKind::Conflict,
            ToolCallsErrorKind::Other => tool_calls::ToolCallsErrorKind::Other,
        },
        message,
    }
}

impl From<ToolCallsState> for tool_calls::ToolCallsState {
    fn from(value: ToolCallsState) -> Self {
        Self {
            revision: value.revision.into(),
            pending_tool_calls: value
                .pending_tool_calls
                .into_iter()
                .map(|call| tool_calls::PendingToolCall {
                    id: call.id.into(),
                    tool: call.tool.into(),
                    is_async: call.is_async,
                })
                .collect(),
            task_handles: value
                .task_handles
                .into_iter()
                .map(|(handle, status)| {
                    let status = match status {
                        TaskStatus::Unknown => tool_calls::TaskStatus::Unknown,
                        TaskStatus::Pending(id) => tool_calls::TaskStatus::Pending(id.into()),
                        TaskStatus::Completed(id) => tool_calls::TaskStatus::Completed(id.into()),
                        TaskStatus::Duplicate => tool_calls::TaskStatus::Duplicate,
                    };
                    (handle, status)
                })
                .collect(),
            messages: tool_calls::MessageCounts {
                steering: value.messages.steering,
                queued: value.messages.queued,
            },
        }
    }
}

impl From<Revision> for tool_calls::Revision {
    fn from(revision: Revision) -> Self {
        let revision = u128::from(revision);
        Self {
            high: u64::try_from(revision >> 64).expect("high bits fit in u64"),
            low: u64::try_from(revision & u128::from(u64::MAX)).expect("low bits fit in u64"),
        }
    }
}

impl From<tool_calls::Revision> for Revision {
    fn from(revision: tool_calls::Revision) -> Self {
        ((u128::from(revision.high) << 64) | u128::from(revision.low)).into()
    }
}
