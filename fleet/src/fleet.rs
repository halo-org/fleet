use crate::{Authentication, Config, ErrorHandler, IdleHandler};
use core::{fmt, pin::pin, time::Duration};
use fleet_core::{Agent, AgentId, ClientStore as _, Message, MessageId, Models, Roles, Tools};
use fleet_kit::{
    AgentActivity, AgentCollection as _, AgentUpdates, AgentsPage, AgentsQuery, AgentsUpdates,
    CollectionRevision, EventQuery, EventRecord, InboxAction, MessageRecord, Preview, Queries as _,
    QueryResponse, QueryUpdate, Revision, ToolCallRecord, TurnEndEvent,
};
use fleet_store::{Store, StoreError};
use futures_util::{Stream, TryStreamExt as _, future};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// An AI agent harness.
#[derive(Clone)]
pub struct Fleet {
    state: Arc<State>,
    authentication: Option<Authentication>,
    configuration: Option<Arc<Configuration>>,
}

/// An error returned by [`Fleet`].
#[derive(Debug)]
pub enum FleetError {
    /// Shutdown has begun.
    ShuttingDown,
    /// A store operation failed.
    Store(StoreError),
    /// A query failed.
    Query(StoreError),
    /// A role could not be read.
    Role(std::io::Error),
}

struct State {
    idle_handler: IdleHandler,
    store: Store,
}

struct Configuration {
    config: Config,
    roles: crate::Roles,
}

impl Fleet {
    /// Creates a fleet with the given store.
    #[must_use]
    pub fn new(store: Store, idle_timeout: Option<Duration>) -> Self {
        Self {
            authentication: None,
            configuration: None,
            state: Arc::new(State {
                idle_handler: IdleHandler::new(idle_timeout),
                store,
            }),
        }
    }

    /// Enables provider authentication.
    pub fn with_authentication(mut self, authentication: Authentication) -> Self {
        self.authentication = Some(authentication);
        self
    }

    /// Returns provider authentication when configured.
    pub const fn authentication(&self) -> Option<&Authentication> {
        self.authentication.as_ref()
    }

    /// Sets the server configuration and role loader.
    pub fn with_configuration(mut self, config: Config, roles: crate::Roles) -> Self {
        self.configuration = Some(Arc::new(Configuration { config, roles }));
        self
    }

    /// Returns the configured providers.
    pub fn providers(&self) -> Vec<(&str, &str)> {
        self.configuration
            .as_ref()
            .map_or_else(Vec::new, |configuration| {
                configuration
                    .config
                    .providers
                    .iter()
                    .map(|(name, provider)| (name.as_str(), provider.plugin.as_str()))
                    .collect()
            })
    }

    /// Lists the available roles.
    pub async fn roles(&self) -> Result<Vec<String>, FleetError> {
        match &self.configuration {
            Some(configuration) => configuration.roles.list().await.map_err(FleetError::Role),
            None => Ok(Vec::new()),
        }
    }

    /// Returns the shared idle handler.
    pub fn idle_handler(&self) -> &IdleHandler {
        &self.state.idle_handler
    }

    /// Runs agents until shutdown and waits for pending mutations.
    pub async fn run(
        &self,
        roles: &(impl Roles<Error: Send> + Sync),
        models: &(impl Models<Error: Send> + Sync),
        tools: &(impl Tools<Error: Send> + Sync),
    ) {
        let state = &self.state;

        let agents = async {
            loop {
                let last_activity = state.idle_handler.last_activity();

                fleet_core::run(
                    &state.idle_handler,
                    ErrorHandler,
                    &state.store,
                    roles,
                    models,
                    tools,
                )
                .await;

                if state.idle_handler.try_shutdown(last_activity) {
                    return;
                }
            }
        };
        let shutdown = async {
            self.wait_for_shutdown().await;
            state.idle_handler.wait_for_mutations().await;
        };

        let work = future::join(self.run_snoozes(), agents);
        future::select(pin!(shutdown), pin!(work)).await;
    }

    async fn run_snoozes(&self) {
        loop {
            let changed = self.state.store.snooze_changed();
            // Keep a pending deadline alive through idle shutdown, and protect the
            // initial deadline read from racing the runtime's idle decision.
            let Some(guard) = self.state.idle_handler.track_mutation() else {
                return;
            };
            let deadline = match self.state.store.next_snooze().await {
                Ok(deadline) => deadline,
                Err(error) => {
                    eprintln!("Could not read snooze deadlines: {error}");
                    drop(guard);
                    future::select(
                        pin!(self.wait_for_shutdown()),
                        pin!(tokio::time::sleep(Duration::from_secs(1))),
                    )
                    .await;
                    continue;
                }
            };
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            if deadline.is_some_and(|deadline| deadline <= now) {
                if let Err(error) = self.state.store.expire_snoozes(now).await {
                    eprintln!("Could not expire snoozes: {error}");
                    future::select(
                        pin!(self.wait_for_shutdown()),
                        pin!(tokio::time::sleep(Duration::from_secs(1))),
                    )
                    .await;
                }
                continue;
            }
            let guard = deadline.map(|_| guard);
            let wait = async {
                match deadline {
                    Some(deadline) => {
                        // Recheck wall time periodically in case the system clock moves.
                        let delay = Duration::from_millis(deadline.saturating_sub(now))
                            .min(Duration::from_secs(60));
                        future::select(pin!(changed), pin!(tokio::time::sleep(delay))).await;
                    }
                    None => changed.await,
                }
            };
            let stopped = matches!(
                future::select(pin!(self.wait_for_shutdown()), pin!(wait)).await,
                future::Either::Left(_)
            );
            drop(guard);
            if stopped {
                return;
            }
        }
    }

    /// Reads one collection page at a consistent collection revision.
    pub async fn agents(&self, query: AgentsQuery) -> Result<AgentsPage, FleetError> {
        self.state
            .store
            .agents(query)
            .await
            .map_err(FleetError::Query)
    }

    /// Reads the collection revision without scanning rows.
    pub async fn collection_revision(&self) -> Result<CollectionRevision, FleetError> {
        self.state
            .store
            .collection_revision()
            .await
            .map_err(FleetError::Query)
    }

    /// Waits for one complete group of changed agent summaries.
    pub async fn agents_updates(
        &self,
        after: CollectionRevision,
    ) -> Result<AgentsUpdates, FleetError> {
        self.state
            .store
            .agents_updates(after)
            .await
            .map_err(FleetError::Query)
    }

    /// Applies a version-checked, idempotent inbox action.
    pub async fn set_inbox(
        &self,
        key: String,
        agent: AgentId,
        expected: CollectionRevision,
        action: InboxAction,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| {
            fleet
                .state
                .store
                .set_inbox(&key, agent, expected, action)
                .await
        })
        .await
    }

    /// Begins shutdown.
    pub fn shutdown(&self) {
        self.state.idle_handler.shutdown();
    }

    /// Waits for shutdown to begin.
    pub async fn wait_for_shutdown(&self) {
        self.state.idle_handler.wait_for_shutdown().await;
    }

    /// Reads the agent's metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent or revision is invalid, or the store fails.
    pub async fn agent(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> Result<QueryResponse<QueryUpdate<Agent>>, FleetError> {
        self.state
            .store
            .agent(agent_id, since)
            .await
            .map_err(FleetError::Query)
    }

    /// Reads pending or changed messages.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent or revision is invalid, or the store fails.
    pub async fn messages(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> Result<QueryResponse<Vec<MessageRecord>>, FleetError> {
        self.state
            .store
            .messages(agent_id, since)
            .await
            .map_err(FleetError::Query)
    }

    /// Reads the agent's activity.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent or revision is invalid, or the store fails.
    pub async fn activity(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> Result<QueryResponse<QueryUpdate<AgentActivity>>, FleetError> {
        self.state
            .store
            .activity(agent_id, since)
            .await
            .map_err(FleetError::Query)
    }

    /// Reads pending or changed tool calls.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent or revision is invalid, or the store fails.
    pub async fn tool_calls(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> Result<QueryResponse<Vec<ToolCallRecord>>, FleetError> {
        self.state
            .store
            .tool_calls(agent_id, since)
            .await
            .map_err(FleetError::Query)
    }

    /// Streams the current preview and its replacements.
    ///
    /// # Errors
    ///
    /// Yields an error if the agent is missing or the store fails.
    pub fn previews(
        &self,
        agent_id: AgentId,
    ) -> impl Stream<Item = Result<Option<Preview>, FleetError>> + Send + use<> {
        self.state
            .store
            .previews(agent_id)
            .map_err(FleetError::Query)
    }

    /// Reads an event history page.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent or revision is invalid, or the store fails.
    pub async fn events(
        &self,
        agent_id: AgentId,
        query: EventQuery,
    ) -> Result<QueryResponse<Vec<EventRecord>>, FleetError> {
        self.state
            .store
            .events(agent_id, query)
            .await
            .map_err(FleetError::Query)
    }

    /// Waits for the next resource revision after the given revision.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent or revision is invalid, or the store fails.
    pub async fn updates(
        &self,
        agent_id: AgentId,
        after: Revision,
    ) -> Result<AgentUpdates, FleetError> {
        self.state
            .store
            .updates(agent_id, after)
            .await
            .map_err(FleetError::Query)
    }

    /// Creates an agent with an optional initial message.
    ///
    /// # Errors
    ///
    /// Returns an error if shutdown has begun or the mutation fails.
    pub async fn create_agent(
        &self,
        idempotency_key: String,
        agent: Agent,
        message: Option<Message>,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| {
            fleet
                .state
                .store
                .create_agent(&idempotency_key, &agent, message.as_ref())
                .await
        })
        .await
    }

    /// Interrupts an agent with an optional message.
    ///
    /// # Errors
    ///
    /// Returns an error if shutdown has begun or the mutation fails.
    pub async fn interrupt_agent(
        &self,
        idempotency_key: String,
        agent_id: AgentId,
        message: Option<Message>,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| {
            fleet
                .state
                .store
                .interrupt_agent(&idempotency_key, agent_id, message.as_ref())
                .await
        })
        .await
    }

    /// Sends a message to an agent.
    ///
    /// # Errors
    ///
    /// Returns an error if shutdown has begun or the mutation fails.
    pub async fn send_message(
        &self,
        idempotency_key: String,
        agent_id: AgentId,
        message: Message,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| {
            fleet
                .state
                .store
                .send_message(&idempotency_key, agent_id, &message)
                .await
        })
        .await
    }

    /// Waits for the turn containing the message to end.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent or message is missing, or the store fails.
    #[expect(dead_code, reason = "turn queries are not implemented yet")]
    pub async fn wait_for_turn_end(
        &self,
        agent_id: AgentId,
        message_id: MessageId,
    ) -> Result<TurnEndEvent, FleetError> {
        self.state
            .store
            .wait_for_turn_end(agent_id, message_id)
            .await
            .map_err(FleetError::Query)
    }

    async fn mutate<F>(
        &self,
        operation: impl FnOnce(Self) -> F + Send + 'static,
    ) -> Result<(), FleetError>
    where
        F: Future<Output = Result<(), StoreError>> + Send + 'static,
    {
        let guard = self
            .state
            .idle_handler
            .track_mutation()
            .ok_or(FleetError::ShuttingDown)?;

        let fleet = self.clone();

        let task = tokio::spawn(async move {
            let _guard = guard;
            operation(fleet).await
        });

        task.await
            .unwrap_or_else(|error| {
                let panic = error
                    .try_into_panic()
                    .expect("mutation task was not cancelled");

                std::panic::resume_unwind(panic)
            })
            .map_err(FleetError::Store)
    }
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => f.write_str("shutting down"),
            Self::Store(_) => f.write_str("store operation failed"),
            Self::Query(_) => f.write_str("query failed"),
            Self::Role(_) => f.write_str("could not read roles"),
        }
    }
}

impl core::error::Error for FleetError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::ShuttingDown => None,
            Self::Store(source) | Self::Query(source) => Some(source),
            Self::Role(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::{
        Compaction, Configured, ContentBlock, JsonObject, MessageMode, ModelOutput,
        ModelProvenance, ModelResponse, ModelResponseChunk, ModelUsage, RuntimeStore as _,
        ToolCall, ToolOutput, ToolResult,
    };
    use http_body_util::{BodyExt as _, Full};
    use hyper::{
        Request, StatusCode,
        body::{Body, Bytes},
    };
    use serde_json::{Value, json};

    async fn fleet() -> Fleet {
        let fleet = Fleet::new(Store::memory().unwrap(), None);
        let agent = Agent {
            id: 1.into(),
            role: Configured {
                name: "role-0".into(),
                config: JsonObject::new(),
            },
            extensions: JsonObject::new(),
        };
        let message = Message {
            id: 2.into(),
            mode: MessageMode::Steer,
            content: vec![ContentBlock::Text("Hello".into())],
        };
        fleet
            .create_agent("create".into(), agent, Some(message))
            .await
            .unwrap();
        fleet
    }

    async fn idle_fleet() -> Fleet {
        let fleet = fleet().await;
        let store = &fleet.state.store;
        store.consume_messages(1.into(), None, true).await.unwrap();
        store
            .append_model_response(1.into(), 0.into(), true, &model_response("Done"))
            .await
            .unwrap();
        fleet
    }

    fn request(path: &str) -> Request<Full<Bytes>> {
        Request::builder()
            .uri(path)
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    #[test]
    fn collection_http_scans_mutations_and_resumes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fleet = idle_fleet().await;
            let read =
                crate::api::handle(fleet.clone(), request("/agents?inbox=unsettled&limit=1"))
                    .await
                    .unwrap();
            assert_eq!(read.status(), StatusCode::OK);
            let revision = read.headers()["read-revision"].to_str().unwrap().to_owned();
            let page: Value =
                serde_json::from_slice(&read.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(page["agents"][0]["inbox"], "pending");
            assert_eq!(page["agents"][0]["version"], revision);
            assert!(page["next"].is_null());

            let mut subscribe = request("/agents/subscription");
            subscribe
                .headers_mut()
                .insert("last-event-id", revision.parse().unwrap());
            let stream = crate::api::handle(fleet.clone(), subscribe).await.unwrap();
            assert_eq!(stream.status(), StatusCode::OK);
            let mut stream = stream.into_body();
            let settle = |key: &str| {
                Request::builder()
                    .method("PUT")
                    .uri("/agents/1/inbox")
                    .header("content-type", "application/json")
                    .header("idempotency-key", key)
                    .header("if-match", format!("\"{revision}\""))
                    .body(Full::new(Bytes::from_static(br#"{"state":"settled"}"#)))
                    .unwrap()
            };
            assert_eq!(
                crate::api::handle(fleet.clone(), settle("settle"))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NO_CONTENT
            );
            let frame = next_frame(&mut stream).await;
            assert!(frame.contains("\"inbox\":\"settled\""));
            assert!(!frame.contains("messages"));
            assert_eq!(
                crate::api::handle(fleet.clone(), settle("settle"))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                crate::api::handle(fleet.clone(), settle("stale"))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::PRECONDITION_FAILED
            );

            let read = crate::api::handle(
                fleet.clone(),
                request("/agents?inbox=settled&direction=backward&limit=1"),
            )
            .await
            .unwrap();
            let settled_revision = read.headers()["read-revision"].to_str().unwrap().to_owned();
            let page: Value =
                serde_json::from_slice(&read.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            let position = page["agents"][0]["position"].as_str().unwrap();
            let last = crate::api::handle(
                fleet.clone(),
                request(&format!(
                    "/agents?inbox=settled&direction=backward&from={position}"
                )),
            )
            .await
            .unwrap();
            assert_eq!(last.status(), StatusCode::OK);
            assert_eq!(last.headers()["read-revision"], settled_revision);
            let last: Value =
                serde_json::from_slice(&last.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(last["agents"], json!([]));

            fleet
                .send_message(
                    "send".into(),
                    1.into(),
                    Message {
                        id: 3.into(),
                        mode: MessageMode::Steer,
                        content: vec![ContentBlock::Text("Continue".into())],
                    },
                )
                .await
                .unwrap();
            assert!(next_frame(&mut stream).await.contains("\"inbox\":\"busy\""));
        });
    }

    #[test]
    fn snooze_worker_wakes_without_clients_and_stops_on_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fleet = idle_fleet().await;
            let worker_fleet = fleet.clone();
            let worker = tokio::spawn(async move { worker_fleet.run_snoozes().await });
            let now: u64 = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
                .try_into()
                .unwrap();
            let version = fleet.collection_revision().await.unwrap();
            fleet
                .set_inbox(
                    "later".into(),
                    1.into(),
                    version,
                    InboxAction::Snooze(now + 60_000),
                )
                .await
                .unwrap();
            let version = fleet.collection_revision().await.unwrap();
            let deadline = now + 100;
            fleet
                .set_inbox(
                    "earlier".into(),
                    1.into(),
                    version,
                    InboxAction::Snooze(deadline),
                )
                .await
                .unwrap();
            // No collection subscribers are connected while the timer runs.
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let changed = fleet.state.store.snooze_changed();
                    if fleet.state.store.next_snooze().await.unwrap().is_none() {
                        break;
                    }
                    changed.await;
                }
            })
            .await
            .unwrap();
            let page = fleet.agents(AgentsQuery::default()).await.unwrap();
            assert_eq!(page.agents[0].inbox, fleet_kit::InboxState::Pending);
            assert_eq!(page.agents[0].since, deadline);
            // Shutdown must also interrupt a wait on a future deadline.
            fleet
                .set_inbox(
                    "later-again".into(),
                    1.into(),
                    page.agents[0].version,
                    InboxAction::Snooze(now + 60_000),
                )
                .await
                .unwrap();
            fleet.shutdown();
            tokio::time::timeout(Duration::from_secs(1), worker)
                .await
                .unwrap()
                .unwrap();
            fleet.state.idle_handler.wait_for_mutations().await;
        });
    }

    #[test]
    fn collection_rejects_ambiguous_resume_and_invalid_scans() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fleet = fleet().await;
            for path in [
                "/agents?inbox=unknown",
                "/agents?limit=0",
                "/agents?inbox=settled&inbox=pending",
                "/agents?from=bad",
                "/agents?from=busy:1:2:3",
                "/agents?since=999",
                "/agents/subscription",
            ] {
                assert_eq!(
                    crate::api::handle(fleet.clone(), request(path))
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::BAD_REQUEST,
                    "{path}"
                );
            }
            for (path, cursor) in [
                ("/agents/subscription?inbox=settled", "1"),
                ("/agents/subscription", "999"),
                ("/agents/subscription", "bad"),
            ] {
                let mut req = request(path);
                req.headers_mut()
                    .insert("last-event-id", cursor.parse().unwrap());
                assert_eq!(
                    crate::api::handle(fleet.clone(), req)
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::BAD_REQUEST
                );
            }
            let mut req = request("/agents/subscription");
            req.headers_mut()
                .append("last-event-id", "1".parse().unwrap());
            req.headers_mut()
                .append("last-event-id", "1".parse().unwrap());
            assert_eq!(
                crate::api::handle(fleet.clone(), req)
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
            for body in [
                br#"{"state":"busy"}"#.as_slice(),
                br#"{"state":"settled","until":1}"#.as_slice(),
            ] {
                let req = Request::builder()
                    .method("PUT")
                    .uri("/agents/1/inbox")
                    .header("content-type", "application/json")
                    .header("idempotency-key", "invalid")
                    .header("if-match", "\"1\"")
                    .body(Full::new(Bytes::copy_from_slice(body)))
                    .unwrap();
                assert_eq!(
                    crate::api::handle(fleet.clone(), req)
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::BAD_REQUEST
                );
            }
        });
    }

    fn model_response(text: &str) -> ModelResponse {
        ModelResponse {
            output: vec![ModelOutput::Text(text.into())],
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

    fn tool_call(id: &str, is_async: bool) -> ToolCall {
        ToolCall {
            id: id.into(),
            tool: "shell".into(),
            is_async,
            input: JsonObject::new(),
        }
    }

    fn tool_result(id: &str) -> ToolResult {
        ToolResult {
            call_id: id.into(),
            output: ToolOutput {
                content: vec![ContentBlock::Text("Done".into())],
                structured_content: None,
                is_error: false,
            },
        }
    }

    async fn next_frame<B>(body: &mut B) -> String
    where
        B: Body<Data = Bytes> + Unpin,
        B::Error: core::fmt::Debug,
    {
        let bytes = body.frame().await.unwrap().unwrap().into_data().unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn subscriptions_bootstrap_from_zero_by_default() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;

            for revision in [None, Some("0")] {
                let mut subscription = request("/agents/1/subscription");
                if let Some(revision) = revision {
                    subscription
                        .headers_mut()
                        .insert("last-event-id", revision.parse().unwrap());
                }
                let response = crate::api::handle(fleet.clone(), subscription)
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);

                let text = next_frame(&mut response.into_body()).await;
                assert!(text.starts_with("id: 1\nevent: update\ndata: "));
                assert!(text.contains("\"agent\":{"));
                assert!(text.contains("\"messages\":["));
                assert!(text.contains("\"status\":\"pending\""));
                assert!(text.contains("\"activity\":{"));
                assert!(!text.contains("\"preview\""));
            }
        });
    }

    #[test]
    fn subscriptions_send_and_resume_complete_revision_groups() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            let store = &fleet.state.store;
            let message = Message {
                id: 3.into(),
                mode: MessageMode::Steer,
                content: vec![ContentBlock::Text("More".into())],
            };
            fleet
                .send_message("send".into(), 1.into(), message)
                .await
                .unwrap();
            store.consume_messages(1.into(), None, true).await.unwrap();
            store
                .append_model_response(1.into(), 1.into(), true, &model_response("Hello"))
                .await
                .unwrap();
            let response = crate::api::handle(fleet.clone(), request("/agents/1/subscription"))
                .await
                .unwrap();
            let mut body = response.into_body();

            let text = next_frame(&mut body).await;
            assert!(text.starts_with("id: 1\nevent: update\ndata: "));
            assert!(!text.contains("\"events\""));

            let text = next_frame(&mut body).await;
            let data = text.strip_prefix("id: 3\nevent: update\ndata: ").unwrap();
            let updates: Value = serde_json::from_str(data).unwrap();
            assert_eq!(updates["messages"].as_array().unwrap().len(), 2);
            assert_eq!(updates["events"].as_array().unwrap().len(), 2);
            assert!(updates.get("activity").is_none());

            let text = next_frame(&mut body).await;
            let data = text.strip_prefix("id: 4\nevent: update\ndata: ").unwrap();
            let updates: Value = serde_json::from_str(data).unwrap();
            assert_eq!(updates["events"].as_array().unwrap().len(), 2);
            assert_eq!(updates["activity"]["turn"], "idle");
            assert!(updates.get("messages").is_none());

            let mut subscription = request("/agents/1/subscription");
            subscription
                .headers_mut()
                .insert("last-event-id", "3".parse().unwrap());
            let response = crate::api::handle(fleet.clone(), subscription)
                .await
                .unwrap();
            let replay = next_frame(&mut response.into_body()).await;
            assert_eq!(replay, text);

            tokio::time::pause();
            let text = next_frame(&mut body).await;
            assert_eq!(text, ": keep-alive\n\n");
        });
    }

    #[test]
    fn reads_and_subscriptions_follow_message_consumption() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            let store = &fleet.state.store;
            let response = crate::api::handle(fleet.clone(), request("/agents/1/messages"))
                .await
                .unwrap();
            let revision = response.headers()["read-revision"]
                .to_str()
                .unwrap()
                .to_owned();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let messages: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                messages,
                json!([
                    {
                        "status": "pending",
                        "position": "0",
                        "id": "2",
                        "mode": "steer",
                        "content": [
                            {
                                "type": "text",
                                "text": "Hello",
                            },
                        ],
                    },
                ])
            );

            let mut subscription = request("/agents/1/subscription");
            subscription
                .headers_mut()
                .insert("last-event-id", revision.parse().unwrap());
            let response = crate::api::handle(fleet.clone(), subscription)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-type"], "text/event-stream");
            let mut body = response.into_body();

            store.consume_messages(1.into(), None, true).await.unwrap();
            let text = next_frame(&mut body).await;
            assert!(text.starts_with("id: 2\nevent: update\ndata: "));
            assert!(text.contains("\"messages\":[{\"status\":\"consumed\",\"id\":\"2\"}]"));
            assert!(text.contains("\"events\":["));
            assert!(text.contains("\"message_id\":\"2\""));

            let path = format!("/agents/1/messages?since={revision}");
            let response = crate::api::handle(fleet.clone(), request(&path))
                .await
                .unwrap();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let messages: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                messages,
                json!([
                    {
                        "status": "consumed",
                        "id": "2",
                    },
                ])
            );

            let mut subscription = request("/agents/1/subscription");
            subscription
                .headers_mut()
                .insert("last-event-id", revision.parse().unwrap());
            let response = crate::api::handle(fleet.clone(), subscription)
                .await
                .unwrap();
            let replay = next_frame(&mut response.into_body()).await;
            assert_eq!(replay, text);

            fleet.shutdown();
            assert!(body.frame().await.is_none());
        });
    }

    #[test]
    fn tool_call_queries_return_pending_or_changed_records() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            let store = &fleet.state.store;
            let response = ModelResponse {
                output: vec![
                    ModelOutput::ToolCall(tool_call("a", false)),
                    ModelOutput::ToolCall(tool_call("b", true)),
                ],
                ..model_response("")
            };
            store.consume_messages(1.into(), None, true).await.unwrap();
            store
                .append_model_response(1.into(), 0.into(), false, &response)
                .await
                .unwrap();
            let pending_a = json!({
                "status": "pending",
                "id": "a",
                "tool": "shell",
                "is_async": false,
            });
            let pending_b = json!({
                "status": "pending",
                "id": "b",
                "tool": "shell",
                "is_async": true,
            });

            let response = crate::api::handle(fleet.clone(), request("/agents/1/tool-calls"))
                .await
                .unwrap();
            assert_eq!(response.headers()["read-revision"], "3");
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let tool_calls: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(tool_calls, vec![pending_a, pending_b.clone()]);

            store
                .append_tool_result(1.into(), &tool_result("a"))
                .await
                .unwrap();
            let resolved_a = json!({
                "status": "resolved",
                "id": "a",
            });

            for (query, expected) in [
                ("", vec![pending_b]),
                ("?since=3", vec![resolved_a]),
                ("?since=4", vec![]),
            ] {
                let path = format!("/agents/1/tool-calls{query}");
                let response = crate::api::handle(fleet.clone(), request(&path))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["read-revision"], "4");
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let tool_calls: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(tool_calls, expected);
            }

            let updates = fleet.updates(1.into(), 3.into()).await.unwrap();
            assert_eq!(
                updates.tool_calls,
                vec![ToolCallRecord::Resolved("a".into())]
            );
            assert_eq!(updates.activity, QueryUpdate::Unchanged);
            assert_eq!(updates.events.len(), 1);
        });
    }

    #[test]
    fn tool_call_subscriptions_preserve_calls_across_compaction_and_interrupts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            let store = &fleet.state.store;
            let tool_call = tool_call("a", true);
            let response = ModelResponse {
                output: vec![ModelOutput::ToolCall(tool_call.clone())],
                ..model_response("")
            };
            store.consume_messages(1.into(), None, true).await.unwrap();
            store
                .append_model_response(1.into(), 0.into(), true, &response)
                .await
                .unwrap();
            let pending = fleet.tool_calls(1.into(), None).await.unwrap();
            let compaction = Compaction {
                last_event: 2.into(),
                input: vec![],
                pending_tool_calls: vec![tool_call],
                usage: response.usage,
            };

            store
                .append_compaction(1.into(), 2.into(), &compaction)
                .await
                .unwrap();
            let current = fleet.tool_calls(1.into(), None).await.unwrap();
            assert_eq!(current.value, pending.value);
            let updates = fleet.updates(1.into(), 3.into()).await.unwrap();
            assert_eq!(updates.tool_calls, []);

            fleet
                .interrupt_agent("interrupt".into(), 1.into(), None)
                .await
                .unwrap();
            let updates = fleet.updates(1.into(), 4.into()).await.unwrap();
            assert_eq!(updates.tool_calls, []);
            store
                .settle_interrupt(1.into(), Some(3.into()), &[tool_result("a")])
                .await
                .unwrap();
            let mut subscription = request("/agents/1/subscription");
            subscription
                .headers_mut()
                .insert("last-event-id", "5".parse().unwrap());
            let response = crate::api::handle(fleet.clone(), subscription)
                .await
                .unwrap();
            let text = next_frame(&mut response.into_body()).await;
            let data = text.strip_prefix("id: 6\nevent: update\ndata: ").unwrap();
            let updates: Value = serde_json::from_str(data).unwrap();

            assert_eq!(
                updates["tool_calls"],
                json!([
                    {
                        "status": "resolved",
                        "id": "a",
                    },
                ])
            );
            assert_eq!(updates["events"].as_array().unwrap().len(), 2);
            assert_eq!(
                updates["activity"],
                json!({
                    "turn": "idle",
                    "interrupt_pending": false,
                })
            );
            let current = fleet.tool_calls(1.into(), None).await.unwrap();
            assert_eq!(current.value, []);
        });
    }

    #[test]
    fn preview_subscriptions_stream_without_advancing_state_revisions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            let store = &fleet.state.store;
            store.consume_messages(1.into(), None, true).await.unwrap();
            let response =
                crate::api::handle(fleet.clone(), request("/agents/1/preview/subscription"))
                    .await
                    .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-type"], "text/event-stream");
            let mut body = response.into_body();

            let text = next_frame(&mut body).await;
            assert_eq!(text, "event: preview\ndata: {\"status\":\"idle\"}\n\n");

            for (sequence, chunk) in [
                (0, ModelResponseChunk::TextStart(0)),
                (1, ModelResponseChunk::TextDelta(0, "Hello".into())),
            ] {
                store
                    .publish_model_response_chunk(1.into(), 0.into(), sequence, &chunk)
                    .await
                    .unwrap();
            }
            let text = next_frame(&mut body).await;
            assert!(text.starts_with("event: preview\ndata: {\"status\":\"streaming\","));
            assert!(text.contains("\"input_cursor\":\"0\""));
            assert!(text.contains("\"text\":\"Hello\""));

            store
                .publish_model_response_chunk(
                    1.into(),
                    0.into(),
                    2,
                    &ModelResponseChunk::TextDelta(0, " there.".into()),
                )
                .await
                .unwrap();
            let text = next_frame(&mut body).await;
            assert!(text.contains("event: preview_text\ndata:  there.\n\n"));
            assert!(!text.contains("id:"));
            let agent = fleet.agent(1.into(), None).await.unwrap();
            assert_eq!(agent.revision, 2.into());

            let response =
                crate::api::handle(fleet.clone(), request("/agents/1/preview/subscription"))
                    .await
                    .unwrap();
            let text = next_frame(&mut response.into_body()).await;
            assert!(text.starts_with("event: preview\n"));
            assert!(text.contains("\"text\":\"Hello there.\""));

            store
                .append_model_response(1.into(), 0.into(), true, &model_response("Hello there."))
                .await
                .unwrap();
            let updates = fleet.updates(1.into(), 2.into()).await.unwrap();
            assert_eq!(updates.revision, 3.into());
            assert_eq!(updates.events.len(), 2);
            let text = next_frame(&mut body).await;
            assert_eq!(text, "event: preview\ndata: {\"status\":\"idle\"}\n\n");

            fleet.shutdown();
            assert!(body.frame().await.is_none());
        });
    }

    #[test]
    fn preview_subscriptions_reject_resume_parameters() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            for (path, status) in [
                ("/agents/1/preview", StatusCode::NOT_FOUND),
                ("/agents/99/preview/subscription", StatusCode::NOT_FOUND),
                (
                    "/agents/1/preview/subscription?since=1",
                    StatusCode::BAD_REQUEST,
                ),
            ] {
                let response = crate::api::handle(fleet.clone(), request(path))
                    .await
                    .unwrap();
                assert_eq!(response.status(), status);
            }

            let mut subscription = request("/agents/1/preview/subscription");
            subscription
                .headers_mut()
                .insert("last-event-id", "0".parse().unwrap());
            let response = crate::api::handle(fleet, subscription).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        });
    }

    #[test]
    fn events_use_exclusive_cursor_bounds_and_independent_revisions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            let store = &fleet.state.store;
            store.consume_messages(1.into(), None, true).await.unwrap();
            fleet
                .interrupt_agent("interrupt".into(), 1.into(), None)
                .await
                .unwrap();
            store
                .settle_interrupt(1.into(), Some(0.into()), &[])
                .await
                .unwrap();

            for (query, expected) in [
                ("direction=backward&limit=1", vec!["1"]),
                ("direction=backward&from=1", vec!["0"]),
                ("from=0", vec!["1"]),
                ("until=1", vec!["0"]),
                ("since=2", vec!["1"]),
                ("from=0&until=1", vec![]),
            ] {
                let path = format!("/agents/1/events?{query}");
                let response = crate::api::handle(fleet.clone(), request(&path))
                    .await
                    .unwrap();
                assert_eq!(response.headers()["read-revision"], "4");

                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let events: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
                let cursors = events
                    .iter()
                    .map(|event| event["cursor"].as_str().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(cursors, expected);
            }
        });
    }

    #[test]
    fn reads_validate_parameters_and_preserve_empty_response_revisions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            for path in [
                "/agents/1",
                "/agents/1/messages",
                "/agents/1/activity",
                "/agents/1/tool-calls",
                "/agents/1/events",
            ] {
                let response = crate::api::handle(fleet.clone(), request(path))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["read-revision"], "1");
            }

            for path in ["/agents/1?since=1", "/agents/1/activity?since=1"] {
                let response = crate::api::handle(fleet.clone(), request(path))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NO_CONTENT);
                assert_eq!(response.headers()["read-revision"], "1");
            }
            for path in [
                "/agents/1?since=2",
                "/agents/1?since=0&since=0",
                "/agents/1/events?limit=0",
                "/agents/1/events?view=chat",
                "/agents/1/tool-calls?since=2",
                "/agents/1/tool-calls?since=0&since=0",
            ] {
                let response = crate::api::handle(fleet.clone(), request(path))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            }

            for revision in ["", "invalid", "2"] {
                let mut subscription = request("/agents/1/subscription");
                subscription
                    .headers_mut()
                    .insert("last-event-id", revision.parse().unwrap());
                let response = crate::api::handle(fleet.clone(), subscription)
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            }

            let response = crate::api::handle(fleet, request("/agents/99"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        });
    }
}
