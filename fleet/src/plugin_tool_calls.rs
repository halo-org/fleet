use core::{error::Error, fmt};
use fleet_core::{
    AgentId, JsonValue, Role, RoleName, Roles as _, RuntimeStore as _, ToolCall, ToolCallId,
    ToolName, Tools,
};
use fleet_kit::{
    Extension as _, PendingToolCall, Revision, TaskHandleProperty, TaskStatus, ToolCalls,
    ToolCallsErrorKind, ToolCallsState,
};
use fleet_store::{Store, StoreError, ToolCallAccess};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock, Weak},
};

/// Access to an agent's own tool calls for a plugin's tool call.
///
/// Task handles are read with the agent's current role.
pub struct PluginToolCalls<T> {
    store: Store,
    agent: AgentId,
    call: ToolCallId,
    access: ToolCallAccess,
    task_handles: Arc<TaskHandles<T>>,
}

/// Reads task handles from tool calls with the declarations of their tools.
///
/// The first declaration read for each tool is kept.
pub struct TaskHandles<T> {
    tools: OnceLock<Weak<T>>,
    properties: Mutex<BTreeMap<ToolName, Option<String>>>,
}

/// An error returned by [`PluginToolCalls`].
#[derive(Debug)]
pub enum PluginToolCallsError<E> {
    /// A store operation failed.
    Store(StoreError),
    /// The agent's role does not exist.
    RoleNotFound(RoleName),
    /// The tools are not available.
    ToolsUnavailable,
    /// A tool could not be loaded.
    LoadTool {
        /// The tool that could not be loaded.
        tool: ToolName,
        /// The error from the tools provider.
        source: E,
    },
    /// A tool's task handle declaration is invalid.
    InvalidTaskHandleProperty {
        /// The tool with the invalid declaration.
        tool: ToolName,
        /// The error reading the declaration.
        source: serde_json::Error,
    },
}

impl<T> PluginToolCalls<T> {
    /// Grants access to an agent's own tool calls within a driving tool call.
    pub fn new(
        store: Store,
        agent: AgentId,
        call: ToolCallId,
        task_handles: Arc<TaskHandles<T>>,
    ) -> Self {
        Self {
            access: store.tool_call_access(agent, call.clone()),
            store,
            agent,
            call,
            task_handles,
        }
    }
}

impl<T: Tools<Error: Send> + Send + Sync> PluginToolCalls<T> {
    async fn role(&self) -> Result<Role, PluginToolCallsError<T::Error>> {
        let agent = self
            .store
            .agent(self.agent)
            .await
            .map_err(PluginToolCallsError::Store)?
            .ok_or(PluginToolCallsError::Store(StoreError::AgentNotFound))?;
        self.store
            .get(&agent.role.name)
            .await
            .map_err(PluginToolCallsError::Store)?
            .ok_or(PluginToolCallsError::RoleNotFound(agent.role.name))
    }
}

impl<T: Tools<Error: Send> + Send + Sync> ToolCalls for PluginToolCalls<T> {
    type Error = PluginToolCallsError<T::Error>;

    async fn state(
        &self,
        after: Option<Revision>,
        task_handles: &[String],
    ) -> Result<ToolCallsState, Self::Error> {
        let snapshot = self
            .access
            .state(after)
            .await
            .map_err(PluginToolCallsError::Store)?;
        let mut statuses = task_handles
            .iter()
            .map(|handle| (handle.clone(), TaskStatus::Unknown))
            .collect::<BTreeMap<_, _>>();
        let mut pending_tool_calls = Vec::new();
        if statuses.is_empty() && snapshot.pending_tool_calls.is_empty() {
            return Ok(ToolCallsState {
                revision: snapshot.revision,
                pending_tool_calls,
                task_handles: statuses,
                messages: snapshot.messages,
            });
        }
        let role = self.role().await?;

        if !statuses.is_empty() {
            let pending = snapshot
                .pending_tool_calls
                .iter()
                .map(|call| &call.id)
                .chain([&self.call])
                .collect::<BTreeSet<_>>();
            for call in &snapshot.context {
                let Some(handle) = self.task_handles.get(&role, call).await? else {
                    continue;
                };
                let Some(status) = statuses.get_mut(&handle) else {
                    continue;
                };
                *status = match status {
                    TaskStatus::Unknown if pending.contains(&call.id) => {
                        TaskStatus::Pending(call.id.clone())
                    }
                    TaskStatus::Unknown => TaskStatus::Completed(call.id.clone()),
                    _ => TaskStatus::Duplicate,
                };
            }
        }

        for call in snapshot.pending_tool_calls {
            pending_tool_calls.push(PendingToolCall {
                task_handle: self.task_handles.get(&role, &call).await?,
                id: call.id,
                tool: call.tool,
                is_async: call.is_async,
            });
        }

        Ok(ToolCallsState {
            revision: snapshot.revision,
            pending_tool_calls,
            task_handles: statuses,
            messages: snapshot.messages,
        })
    }

    async fn interrupt(&self, key: &str, calls: &[ToolCallId]) -> Result<(), Self::Error> {
        self.access
            .interrupt(key, calls)
            .await
            .map_err(PluginToolCallsError::Store)
    }
}

impl<T> TaskHandles<T> {
    /// Creates task handles whose tools are bound later.
    pub const fn new() -> Self {
        Self {
            tools: OnceLock::new(),
            properties: Mutex::new(BTreeMap::new()),
        }
    }

    /// Binds the tools whose declarations are read.
    ///
    /// # Panics
    ///
    /// Panics if tools are already bound.
    pub fn bind(&self, tools: &Arc<T>) {
        assert!(
            self.tools.set(Arc::downgrade(tools)).is_ok(),
            "tools are bound once"
        );
    }
}

impl<T: Tools<Error: Send> + Send + Sync> TaskHandles<T> {
    async fn get(
        &self,
        role: &Role,
        call: &ToolCall,
    ) -> Result<Option<String>, PluginToolCallsError<T::Error>> {
        let Some(tool) = role.tools.get(&call.tool) else {
            return Ok(None);
        };
        let property = self.property(&tool.name).await?;
        Ok(
            property.and_then(|property| match call.input.get(&property) {
                Some(JsonValue::String(handle)) => Some(handle.clone()),
                _ => None,
            }),
        )
    }

    async fn property(
        &self,
        name: &ToolName,
    ) -> Result<Option<String>, PluginToolCallsError<T::Error>> {
        if let Some(property) = self.properties.lock().expect("never poisoned").get(name) {
            return Ok(property.clone());
        }
        let tools = self
            .tools
            .get()
            .and_then(Weak::upgrade)
            .ok_or(PluginToolCallsError::ToolsUnavailable)?;
        let tool = tools
            .get(name)
            .await
            .map_err(|source| PluginToolCallsError::LoadTool {
                tool: name.clone(),
                source,
            })?;
        let property = tool
            .map(|tool| TaskHandleProperty::get(&tool.extensions))
            .transpose()
            .map_err(|source| PluginToolCallsError::InvalidTaskHandleProperty {
                tool: name.clone(),
                source,
            })?
            .flatten()
            .map(|property| property.0);
        Ok(self
            .properties
            .lock()
            .expect("never poisoned")
            .entry(name.clone())
            .or_insert(property)
            .clone())
    }
}

impl<E> fmt::Display for PluginToolCallsError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(_) => f.write_str("tool call store operation failed"),
            Self::RoleNotFound(role) => write!(f, "role not found: {role}"),
            Self::ToolsUnavailable => f.write_str("tools are not available"),
            Self::LoadTool { tool, source: _ } => write!(f, "could not load tool {tool}"),
            Self::InvalidTaskHandleProperty { tool, source: _ } => {
                write!(f, "invalid task handle property for tool {tool}")
            }
        }
    }
}

impl<E: Error + 'static> Error for PluginToolCallsError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(source) => Some(source),
            Self::LoadTool { source, .. } => Some(source),
            Self::InvalidTaskHandleProperty { source, .. } => Some(source),
            Self::RoleNotFound(_) | Self::ToolsUnavailable => None,
        }
    }
}

impl<E: Error + 'static> fleet_core::Error<ToolCallsErrorKind> for PluginToolCallsError<E> {
    fn kind(&self) -> ToolCallsErrorKind {
        match self {
            Self::Store(error) => fleet_core::Error::<ToolCallsErrorKind>::kind(error),
            Self::RoleNotFound(_)
            | Self::ToolsUnavailable
            | Self::LoadTool { .. }
            | Self::InvalidTaskHandleProperty { .. } => ToolCallsErrorKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::{
        Agent, ClientStore as _, Configured, ContentBlock, JsonObject, Message, MessageMode,
        ModelOutput, ModelProvenance, ModelResponse, ModelUsage, Tool, ToolContext, ToolOutput,
        ToolResult, ToolsErrorKind,
    };
    use fleet_kit::RoleStore as _;
    use futures_util::future;
    use serde_json::json;

    /// Tools where `tasks:run` declares `task_handle` as its task handle property.
    struct TaskTools;

    #[derive(Debug)]
    struct Unavailable;

    impl Tools for TaskTools {
        type Error = Unavailable;

        fn get(
            &self,
            name: &ToolName,
        ) -> impl Future<Output = Result<Option<Tool>, Unavailable>> + Send {
            let mut extensions = JsonObject::new();
            if name.as_ref() == "tasks:run" {
                TaskHandleProperty("task_handle".into())
                    .insert(&mut extensions)
                    .unwrap();
            }
            future::ready(Ok(Some(Tool {
                name: name.clone(),
                description: None,
                is_async: true,
                input_schema: JsonObject::new(),
                output_schema: None,
                extensions,
            })))
        }

        fn execute(
            &self,
            _: &ToolContext,
            _: bool,
            _: &JsonObject,
        ) -> impl Future<Output = Result<ToolOutput, Unavailable>> + Send {
            future::ready(Err(Unavailable))
        }

        fn interrupted_output(
            &self,
            _: &ToolContext,
            _: &JsonObject,
        ) -> impl Future<Output = Result<ToolOutput, Unavailable>> + Send {
            future::ready(Err(Unavailable))
        }

        fn unknown_tool_output(
            &self,
            _: AgentId,
            _: &ToolCall,
        ) -> impl Future<Output = Result<ToolOutput, Unavailable>> + Send {
            future::ready(Err(Unavailable))
        }
    }

    impl fmt::Display for Unavailable {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("unavailable")
        }
    }

    impl Error for Unavailable {}

    impl fleet_core::Error<ToolsErrorKind> for Unavailable {
        fn kind(&self) -> ToolsErrorKind {
            ToolsErrorKind::Other
        }
    }

    fn configured<N: for<'a> From<&'a str>>(name: &str) -> Configured<N> {
        Configured {
            name: name.into(),
            config: JsonObject::new(),
        }
    }

    /// Creates an agent whose role runs `tasks:run` as `run` and `tasks:wait` as `wait`.
    async fn agent(store: &Store) {
        let role = Role {
            name: "assistant".into(),
            parameters: BTreeMap::new(),
            model: configured("model"),
            tools: BTreeMap::from([
                ("run".into(), configured("tasks:run")),
                ("wait".into(), configured("tasks:wait")),
            ]),
            system_prompt: String::new(),
            compaction: None,
            extensions: JsonObject::new(),
        };
        let agent = Agent {
            id: 1.into(),
            role: configured("assistant"),
            extensions: JsonObject::new(),
        };
        let message = Message {
            id: 1.into(),
            mode: MessageMode::Queue,
            content: vec![ContentBlock::Text("Hello".into())],
        };
        store.put_role("role", &role, None).await.unwrap();
        store
            .create_agent("create", &agent, Some(&message))
            .await
            .unwrap();
        store.consume_messages(agent.id, None, true).await.unwrap();
    }

    fn response(calls: &[(&str, &str, serde_json::Value)]) -> ModelResponse {
        ModelResponse {
            output: calls
                .iter()
                .map(|(id, tool, input)| {
                    ModelOutput::ToolCall(ToolCall {
                        id: (*id).into(),
                        tool: (*tool).into(),
                        is_async: true,
                        input: input.as_object().unwrap().clone(),
                    })
                })
                .collect(),
            usage: ModelUsage {
                input: 1,
                output: 1,
                cache_read: 0,
                cache_write: 0,
            },
            provenance: ModelProvenance {
                provider_id: "provider".into(),
                model_id: "model".into(),
            },
            continuation_token: None,
        }
    }

    #[test]
    fn task_handles_resolve_within_the_driving_call_context() {
        let store = Store::memory().unwrap();
        let tools = Arc::new(TaskTools);
        let task_handles = Arc::new(TaskHandles::new());
        task_handles.bind(&tools);
        let state = async |call: &str| {
            PluginToolCalls::new(store.clone(), 1.into(), call.into(), task_handles.clone())
                .state(None, &["first".into(), "second".into(), "third".into()])
                .await
                .unwrap()
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            agent(&store).await;
            store
                .append_model_response(
                    1.into(),
                    0.into(),
                    false,
                    &response(&[
                        ("a", "run", json!({ "task_handle": "first" })),
                        ("b", "run", json!({ "task_handle": "second" })),
                        ("c", "wait", json!({ "task_handle": "third" })),
                        ("early", "wait", json!({})),
                    ]),
                )
                .await
                .unwrap();
            store
                .append_tool_result(
                    1.into(),
                    &ToolResult {
                        call_id: "a".into(),
                        output: ToolOutput {
                            content: vec![],
                            structured_content: None,
                            is_error: false,
                        },
                    },
                )
                .await
                .unwrap();
            store
                .append_model_response(
                    1.into(),
                    2.into(),
                    false,
                    &response(&[
                        ("d", "run", json!({ "task_handle": "second" })),
                        ("late", "wait", json!({})),
                    ]),
                )
                .await
                .unwrap();

            let early = state("early").await;
            let late = state("late").await;

            assert_eq!(
                early.task_handles,
                BTreeMap::from([
                    ("first".into(), TaskStatus::Completed("a".into())),
                    ("second".into(), TaskStatus::Pending("b".into())),
                    ("third".into(), TaskStatus::Unknown),
                ])
            );
            assert_eq!(
                late.task_handles,
                BTreeMap::from([
                    ("first".into(), TaskStatus::Completed("a".into())),
                    ("second".into(), TaskStatus::Duplicate),
                    ("third".into(), TaskStatus::Unknown),
                ])
            );
            let pending = early
                .pending_tool_calls
                .iter()
                .map(|call| (call.id.as_ref(), call.task_handle.as_deref()))
                .collect::<Vec<_>>();
            assert_eq!(
                pending,
                [
                    ("b", Some("second")),
                    ("c", None),
                    ("d", Some("second")),
                    ("late", None),
                ]
            );
        });
    }
}
