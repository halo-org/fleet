use crate::{
    Authentication, Config, ErrorChain, ErrorHandler, IdleHandler, Notifications, SandboxOAuth,
    WorkspaceLocks,
};
use core::{error::Error, fmt, pin::pin, time::Duration};
use fleet_core::{
    Agent, AgentId, ClientStore as _, Error as _, ErrorHandler as _, JsonObject, Message,
    MessageId, Models, Role, RoleName, ToolCallId, Tools,
};
use fleet_kit::{
    AgentActivity, AgentCollection as _, AgentMetadata, AgentUpdates, AgentsPage, AgentsQuery,
    AgentsUpdates, CollectionRevision, EventQuery, EventRecord, InboxAction, MessageRecord,
    Preview, Queries as _, QueryResponse, QueryUpdate, Revision, RoleStore as _, RoleVersion,
    StoredRole, ToolCallRecord, TurnEndEvent, WorkspaceResolvers, Workspaces, WorkspacesErrorKind,
};
use fleet_store::{Store, StoreError, WorkspaceRelease, WorkspaceReleaseState};
use fleet_workspaces::AgentWorkspaces;
use futures_util::{Stream, StreamExt as _, TryStreamExt as _, future, stream::FuturesUnordered};
use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const NOTIFICATION_LEASE: u64 = 120_000;

/// An AI agent harness.
#[derive(Clone)]
pub struct Fleet {
    state: Arc<State>,
    authentication: Option<Authentication>,
    notifications: Option<Notifications>,
    configuration: Option<Arc<Configuration>>,
    sandbox: Option<SandboxOAuth>,
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
}

struct State {
    idle_handler: IdleHandler,
    store: Store,
    workspace_locks: WorkspaceLocks,
}

/// The outcome of an attempt to release a workspace.
enum ReleaseAttempt {
    /// The workspace was released, or the release was cancelled or dropped.
    Finished,
    /// The agent or its environment is in use.
    Busy,
}

#[derive(Debug)]
struct WorkspaceReleaseError {
    agent: AgentId,
    source: Box<dyn Error + Send>,
}

struct Configuration {
    config: Config,
}

impl Fleet {
    /// Creates a fleet with the given store.
    #[must_use]
    pub fn new(store: Store, idle_timeout: Option<Duration>) -> Self {
        Self {
            authentication: None,
            notifications: None,
            configuration: None,
            sandbox: None,
            state: Arc::new(State {
                idle_handler: IdleHandler::new(idle_timeout),
                store,
                workspace_locks: WorkspaceLocks::new(),
            }),
        }
    }

    pub(crate) fn blob_store(&self) -> &Store {
        &self.state.store
    }

    pub(crate) fn workspace_locks(&self) -> &WorkspaceLocks {
        &self.state.workspace_locks
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

    /// Enables notifications.
    pub fn with_notifications(mut self, notifications: Notifications) -> Self {
        self.notifications = Some(notifications);
        self
    }

    /// Returns notifications when configured.
    pub const fn notifications(&self) -> Option<&Notifications> {
        self.notifications.as_ref()
    }

    /// Serves the sandbox's OAuth provider.
    pub fn with_sandbox(mut self, provider: SandboxOAuth) -> Self {
        self.sandbox = Some(provider);
        self
    }

    /// Returns the sandbox's OAuth provider when running in the sandbox.
    pub const fn sandbox(&self) -> Option<&SandboxOAuth> {
        self.sandbox.as_ref()
    }

    /// Creates or replaces a device that receives notifications.
    pub async fn put_device(&self, id: u128, device: JsonObject) -> Result<(), FleetError> {
        self.mutate(async move |fleet| fleet.state.store.put_device(id, device).await)
            .await
    }

    /// Deletes a device if it exists.
    pub async fn delete_device(&self, id: u128) -> Result<(), FleetError> {
        self.mutate(async move |fleet| fleet.state.store.delete_device(id).await)
            .await
    }

    /// Sets the server configuration.
    pub fn with_configuration(mut self, config: Config) -> Self {
        self.configuration = Some(Arc::new(Configuration { config }));
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

    /// Lists the stored roles.
    pub async fn roles(&self) -> Result<Vec<StoredRole>, FleetError> {
        self.state.store.roles().await.map_err(FleetError::Store)
    }

    /// Returns a stored role.
    pub async fn role(&self, name: &RoleName) -> Result<Option<StoredRole>, FleetError> {
        self.state.store.role(name).await.map_err(FleetError::Store)
    }

    /// Creates or replaces a role if its version matches.
    ///
    /// An `expected` version of `None` creates the role only if it does not exist.
    pub async fn put_role(
        &self,
        key: String,
        role: Role,
        expected: Option<RoleVersion>,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| fleet.state.store.put_role(&key, &role, expected).await)
            .await
    }

    /// Deletes a role if its version matches.
    pub async fn delete_role(
        &self,
        key: String,
        name: RoleName,
        expected: RoleVersion,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| fleet.state.store.delete_role(&key, &name, expected).await)
            .await
    }

    /// Returns the shared idle handler.
    pub fn idle_handler(&self) -> &IdleHandler {
        &self.state.idle_handler
    }

    /// Runs agents and releases their workspaces until shutdown, then waits for pending mutations.
    pub async fn run(
        &self,
        models: &(impl Models<Error: Send> + Sync),
        tools: &(impl Tools<Error: Send> + Sync),
        workspaces: &(impl Workspaces<Error: Send> + WorkspaceResolvers<Error: Send> + Clone + Sync),
    ) {
        let state = &self.state;

        let agents = async {
            loop {
                let last_activity = state.idle_handler.last_activity();

                fleet_core::run(
                    &state.idle_handler,
                    ErrorHandler,
                    &state.store,
                    &state.store,
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

        let work = future::join(
            future::join4(
                self.run_snoozes(),
                self.run_notifications(),
                self.run_agent_scopes(),
                self.run_workspace_releases(workspaces),
            ),
            agents,
        );
        future::select(pin!(shutdown), pin!(work)).await;
    }

    async fn run_agent_scopes(&self) {
        loop {
            let changed = self.state.store.agent_scopes_changed();
            let Some(guard) = self.state.idle_handler.track_mutation() else {
                return;
            };
            let remaining = match self.state.store.reconcile_agent_scopes().await {
                Ok(remaining) => remaining,
                Err(error) => {
                    eprintln!("Could not settle agent scopes: {}", ErrorChain(&error));
                    future::select(
                        pin!(self.wait_for_shutdown()),
                        pin!(tokio::time::sleep(Duration::from_secs(1))),
                    )
                    .await;
                    continue;
                }
            };
            let guard = (remaining != 0).then_some(guard);
            let stopped = matches!(
                future::select(pin!(self.wait_for_shutdown()), pin!(changed)).await,
                future::Either::Left(_)
            );
            drop(guard);
            if stopped {
                return;
            }
        }
    }

    async fn run_snoozes(&self) {
        loop {
            let changed = self.state.store.snooze_changed();
            // Pending deadlines allow idle shutdown since opening the store expires them.
            let deadline = match self.state.store.next_snooze().await {
                Ok(deadline) => deadline,
                Err(error) => {
                    eprintln!("Could not read snooze deadlines: {}", ErrorChain(&error));
                    let stopped = matches!(
                        future::select(
                            pin!(self.wait_for_shutdown()),
                            pin!(tokio::time::sleep(Duration::from_secs(1))),
                        )
                        .await,
                        future::Either::Left(_)
                    );
                    if stopped {
                        return;
                    }
                    continue;
                }
            };
            let now = crate::fleet::now();
            if deadline.is_some_and(|deadline| deadline <= now) {
                let Some(_guard) = self.state.idle_handler.track_mutation() else {
                    return;
                };
                if let Err(error) = self.state.store.expire_snoozes(now).await {
                    eprintln!("Could not expire snoozes: {}", ErrorChain(&error));
                    future::select(
                        pin!(self.wait_for_shutdown()),
                        pin!(tokio::time::sleep(Duration::from_secs(1))),
                    )
                    .await;
                }
                continue;
            }
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
            if stopped {
                return;
            }
        }
    }

    async fn run_workspace_releases(
        &self,
        workspaces: &(impl Workspaces<Error: Send> + WorkspaceResolvers<Error: Send> + Clone + Sync),
    ) {
        let settled_for = self
            .configuration
            .as_ref()
            .and_then(|configuration| configuration.config.release_settled_workspaces_after_ms);
        let mut releases = FuturesUnordered::new();
        let mut releasing = BTreeSet::new();
        loop {
            let changed = self.state.store.workspace_releases_changed();
            // Pending deadlines allow idle shutdown since due releases are listed on startup.
            let now = crate::fleet::now();
            let deadline = match self.state.store.workspace_releases(now, settled_for).await {
                Ok((due, deadline)) => {
                    for release in due {
                        if releasing.insert(release.agent()) {
                            releases.push(self.release_workspace(workspaces, release));
                        }
                    }
                    deadline
                }
                Err(error) => {
                    eprintln!("Could not read workspace releases: {}", ErrorChain(&error));
                    Some(now.saturating_add(1000))
                }
            };
            let finished = async {
                match releases.next().await {
                    Some(agent) => {
                        releasing.remove(&agent);
                    }
                    None => future::pending().await,
                }
            };
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
                future::select(
                    pin!(self.wait_for_shutdown()),
                    pin!(future::select(pin!(wait), pin!(finished))),
                )
                .await,
                future::Either::Left(_)
            );
            if stopped {
                while releases.next().await.is_some() {}
                return;
            }
        }
    }

    /// Releases an agent's workspace while it is idle, retrying failures until it is cancelled.
    ///
    /// Waiting stops at shutdown. An attempt that has started finishes first.
    async fn release_workspace(
        &self,
        workspaces: &(impl Workspaces<Error: Send> + WorkspaceResolvers<Error: Send> + Clone + Sync),
        release: WorkspaceRelease,
    ) -> AgentId {
        let agent = release.agent();
        let mut retries = 0;
        loop {
            let Some(guard) = self.state.idle_handler.track_mutation() else {
                return agent;
            };
            let result = self.try_release_workspace(workspaces, release).await;
            drop(guard);
            let wait = match result {
                Ok(ReleaseAttempt::Finished) => return agent,
                Ok(ReleaseAttempt::Busy) => {
                    future::Either::Left(tokio::time::sleep(Duration::from_secs(60)))
                }
                Err(source) => {
                    let error = WorkspaceReleaseError { agent, source };
                    let backoff = ErrorHandler.handle_error_with_backoff(error, retries);
                    retries = retries.saturating_add(1);
                    future::Either::Right(backoff)
                }
            };
            if let future::Either::Left(_) =
                future::select(pin!(self.wait_for_shutdown()), pin!(wait)).await
            {
                return agent;
            }
        }
    }

    async fn try_release_workspace(
        &self,
        workspaces: &(impl Workspaces<Error: Send> + WorkspaceResolvers<Error: Send> + Clone + Sync),
        release: WorkspaceRelease,
    ) -> Result<ReleaseAttempt, Box<dyn Error + Send>> {
        let agent = release.agent();
        let store = &self.state.store;
        let owned = AgentWorkspaces::new(
            agent,
            store.agent_access(agent),
            store.clone(),
            workspaces.clone(),
        )
        .owned_environment()
        .await;
        let context = match owned {
            Ok(context) => context,
            Err(error) if error.kind() == WorkspacesErrorKind::InvalidConfiguration => {
                eprintln!(
                    "Dropped the workspace release of agent {agent}. Release it by hand: {}",
                    ErrorChain(&error),
                );
                None
            }
            Err(error) => return Err(boxed(error)),
        };
        let Some(context) = context else {
            store
                .complete_workspace_release(release)
                .await
                .map_err(boxed)?;
            return Ok(ReleaseAttempt::Finished);
        };
        let Some(_lock) = self.state.workspace_locks.try_release(&context) else {
            return Ok(ReleaseAttempt::Busy);
        };
        match store
            .workspace_release_state(release)
            .await
            .map_err(boxed)?
        {
            WorkspaceReleaseState::Ready => {}
            WorkspaceReleaseState::Busy => return Ok(ReleaseAttempt::Busy),
            WorkspaceReleaseState::Cancelled => return Ok(ReleaseAttempt::Finished),
        }
        match Workspaces::release(workspaces, &context).await {
            Ok(()) => {}
            Err(error) if error.kind() == WorkspacesErrorKind::InvalidConfiguration => {
                eprintln!(
                    "Dropped the workspace release of agent {agent}. Release it by hand: {}",
                    ErrorChain(&error),
                );
            }
            Err(error) => return Err(boxed(error)),
        }
        store
            .complete_workspace_release(release)
            .await
            .map_err(boxed)?;
        Ok(ReleaseAttempt::Finished)
    }

    async fn run_notifications(&self) {
        let Some(notifications) = &self.notifications else {
            return;
        };
        loop {
            let changed = self.state.store.notification_changed();
            // Keep pending notifications alive through idle shutdown.
            let Some(guard) = self.state.idle_handler.track_mutation() else {
                return;
            };
            match self
                .state
                .store
                .claim_notification(crate::fleet::now(), NOTIFICATION_LEASE)
                .await
            {
                Ok(Some(claim)) => {
                    let notifications = notifications.clone();
                    let store = self.state.store.clone();
                    tokio::spawn(async move {
                        let _guard = guard;
                        if let Err(error) = notifications.deliver(&store, claim).await {
                            eprintln!("Could not deliver notification: {}", ErrorChain(&error));
                        }
                    });
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("Could not claim notification: {}", ErrorChain(&error));
                    drop(guard);
                    future::select(
                        pin!(self.wait_for_shutdown()),
                        pin!(tokio::time::sleep(Duration::from_secs(1))),
                    )
                    .await;
                    continue;
                }
            }
            let deadline = match self.state.store.next_notification().await {
                Ok(deadline) => deadline,
                Err(error) => {
                    eprintln!(
                        "Could not read notification deadlines: {}",
                        ErrorChain(&error)
                    );
                    drop(guard);
                    future::select(
                        pin!(self.wait_for_shutdown()),
                        pin!(tokio::time::sleep(Duration::from_secs(1))),
                    )
                    .await;
                    continue;
                }
            };
            let now = crate::fleet::now();
            let guard = deadline.map(|_| guard);
            let wait = async {
                match deadline {
                    Some(deadline) => {
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

    /// Sets or clears an agent's title if its metadata has not changed.
    pub async fn set_title(
        &self,
        key: String,
        agent: AgentId,
        expected: Revision,
        title: Option<String>,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| {
            fleet
                .state
                .store
                .set_title(&key, agent, expected, title.as_deref())
                .await
        })
        .await
    }

    /// Pins or unpins an agent if its metadata has not changed.
    pub async fn set_pinned(
        &self,
        key: String,
        agent: AgentId,
        expected: Revision,
        pinned: bool,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| {
            fleet
                .state
                .store
                .set_pinned(&key, agent, expected, pinned)
                .await
        })
        .await
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
    ) -> Result<QueryResponse<QueryUpdate<AgentMetadata>>, FleetError> {
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
        include_async_tool_calls: bool,
        message: Option<Message>,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| {
            fleet
                .state
                .store
                .interrupt_agent(
                    &idempotency_key,
                    agent_id,
                    include_async_tool_calls,
                    message.as_ref(),
                )
                .await
        })
        .await
    }

    /// Interrupts tool calls without interrupting the agent.
    ///
    /// # Errors
    ///
    /// Returns an error if shutdown has begun or the mutation fails.
    pub async fn interrupt_tool_calls(
        &self,
        idempotency_key: String,
        agent_id: AgentId,
        call_ids: Vec<ToolCallId>,
    ) -> Result<(), FleetError> {
        self.mutate(async move |fleet| {
            fleet
                .state
                .store
                .interrupt_tool_calls(&idempotency_key, agent_id, &call_ids)
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "turn queries are not implemented yet")
    )]
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

    pub(crate) async fn mutate<T, F>(
        &self,
        operation: impl FnOnce(Self) -> F + Send + 'static,
    ) -> Result<T, FleetError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, StoreError>> + Send + 'static,
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

fn boxed(error: impl Error + Send + 'static) -> Box<dyn Error + Send> {
    Box::new(error)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => f.write_str("shutting down"),
            Self::Store(_) => f.write_str("store operation failed"),
            Self::Query(_) => f.write_str("query failed"),
        }
    }
}

impl core::error::Error for FleetError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::ShuttingDown => None,
            Self::Store(source) | Self::Query(source) => Some(source),
        }
    }
}

impl fmt::Display for WorkspaceReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not release the workspace of agent {}", self.agent)
    }
}

impl Error for WorkspaceReleaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
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
    use std::{collections::BTreeMap, sync::Mutex};

    /// Records released agents and fails the given number of releases for each agent.
    ///
    /// Releases wait while `paused` is locked for writing, after notifying `entered`.
    #[derive(Clone, Default)]
    struct Releases {
        released: Arc<Mutex<Vec<AgentId>>>,
        failures: Arc<Mutex<BTreeMap<AgentId, u32>>>,
        paused: Arc<tokio::sync::RwLock<()>>,
        entered: Arc<tokio::sync::Notify>,
    }

    #[derive(Debug)]
    struct Unavailable;

    impl WorkspaceResolvers for Releases {
        type Error = Unavailable;

        fn get(
            &self,
            _: &fleet_kit::WorkspaceName,
        ) -> impl Future<Output = Result<Option<fleet_kit::Workspace>, Unavailable>> + Send
        {
            future::ready(Ok(None))
        }

        fn resolve(
            &self,
            _: AgentId,
            _: &Configured<fleet_kit::WorkspaceName>,
        ) -> impl Future<Output = Result<fleet_kit::WorkspaceResolution, Unavailable>> + Send
        {
            future::ready(Err(Unavailable))
        }
    }

    impl Workspaces for Releases {
        type Error = Unavailable;

        fn get(
            &self,
            _: &fleet_kit::WorkspaceName,
        ) -> impl Future<Output = Result<Option<fleet_kit::Workspace>, Unavailable>> + Send
        {
            future::ready(Ok(None))
        }

        fn open(
            &self,
            _: &fleet_kit::WorkspaceContext,
        ) -> impl Future<Output = Result<(), Unavailable>> + Send {
            future::ready(Err(Unavailable))
        }

        fn release(
            &self,
            context: &fleet_kit::WorkspaceContext,
        ) -> impl Future<Output = Result<(), Unavailable>> + Send {
            let releases = self.clone();
            let agent = context.agent_id;
            async move {
                releases.entered.notify_one();
                let _paused = releases.paused.read().await;
                let failed = releases
                    .failures
                    .lock()
                    .unwrap()
                    .get_mut(&agent)
                    .is_some_and(|remaining| {
                        let failed = *remaining != 0;
                        *remaining = remaining.saturating_sub(1);
                        failed
                    });
                if failed {
                    return Err(Unavailable);
                }
                releases.released.lock().unwrap().push(agent);
                Ok(())
            }
        }

        fn read(
            &self,
            _: &fleet_kit::WorkspaceContext,
            _: &str,
            _: u64,
        ) -> impl Future<Output = Result<Vec<u8>, Unavailable>> + Send {
            future::ready(Err(Unavailable))
        }

        fn write(
            &self,
            _: &fleet_kit::WorkspaceContext,
            _: &fleet_kit::WorkspaceWrite,
        ) -> impl Future<Output = Result<(), Unavailable>> + Send {
            future::ready(Err(Unavailable))
        }

        fn exec(
            &self,
            _: &fleet_kit::WorkspaceContext,
            _: bool,
            _: &fleet_kit::WorkspaceCommand,
        ) -> impl Future<Output = Result<fleet_kit::WorkspaceOutput, Unavailable>> + Send {
            future::ready(Err(Unavailable))
        }
    }

    impl fmt::Display for Unavailable {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("workspace provider is unavailable")
        }
    }

    impl Error for Unavailable {}

    impl fleet_core::Error<WorkspacesErrorKind> for Unavailable {
        fn kind(&self) -> WorkspacesErrorKind {
            WorkspacesErrorKind::Other
        }
    }

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

    fn title_request(key: &str, expected: &str, body: &Value) -> Request<Full<Bytes>> {
        Request::builder()
            .method("PUT")
            .uri("/agents/1/title")
            .header("content-type", "application/json")
            .header("idempotency-key", key)
            .header("if-match", format!("\"{expected}\""))
            .body(Full::new(Bytes::from(serde_json::to_vec(body).unwrap())))
            .unwrap()
    }

    #[test]
    fn set_title_updates_reads_and_subscriptions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let body = json!({
            "title": "Review 🚀",
        });

        runtime.block_on(async {
            let fleet = fleet().await;
            let revision = fleet.collection_revision().await.unwrap();
            let mut subscription = request("/agents/subscription");
            subscription
                .headers_mut()
                .insert("last-event-id", revision.to_string().parse().unwrap());
            let mut collection = crate::api::handle(fleet.clone(), subscription)
                .await
                .unwrap()
                .into_body();
            let mut subscription = request("/agents/1/subscription");
            subscription
                .headers_mut()
                .insert("last-event-id", "1".parse().unwrap());
            let mut updates = crate::api::handle(fleet.clone(), subscription)
                .await
                .unwrap()
                .into_body();

            let rename = title_request("rename", "1", &body);
            let response = crate::api::handle(fleet.clone(), rename).await.unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);

            let frame = next_frame(&mut updates).await;
            let update: Value =
                serde_json::from_str(frame.strip_prefix("id: 2\nevent: update\ndata: ").unwrap())
                    .unwrap();
            assert_eq!(update["agent"]["title"], "Review 🚀");
            assert_eq!(update["agent"]["metadata_version"], "2");
            assert_eq!(update.get("events"), None);
            assert_eq!(update.get("activity"), None);

            let frame = next_frame(&mut collection).await;
            let update: Value =
                serde_json::from_str(frame.split_once("data: ").unwrap().1).unwrap();
            assert_eq!(update["agents"][0]["title"], "Review 🚀");
            assert_eq!(update["agents"][0]["metadata_version"], "2");

            fleet
                .state
                .store
                .consume_messages(1.into(), None, true)
                .await
                .unwrap();
            let response = crate::api::handle(fleet.clone(), request("/agents/1?since=1"))
                .await
                .unwrap();
            assert_eq!(response.headers()["read-revision"], "3");
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let agent: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(agent["title"], "Review 🚀");
            assert_eq!(agent["metadata_version"], "2");
            assert_eq!(agent.get("extensions"), None);

            let response = crate::api::handle(fleet, request("/agents/1?since=2"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert_eq!(response.headers()["read-revision"], "3");
        });
    }

    #[test]
    fn set_title_checks_preconditions_and_retries() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cases = [
            ("rename", "1", Some("Title"), StatusCode::NO_CONTENT),
            ("rename", "1", Some("Title"), StatusCode::NO_CONTENT),
            ("unchanged", "2", Some("Title"), StatusCode::NO_CONTENT),
            ("stale", "1", Some("Other"), StatusCode::PRECONDITION_FAILED),
            ("rename", "1", Some("Other"), StatusCode::CONFLICT),
            ("clear", "2", None, StatusCode::NO_CONTENT),
        ];

        runtime.block_on(async {
            let fleet = fleet().await;

            for (key, version, title, status) in cases {
                let body = json!({
                    "title": title,
                });
                let request = title_request(key, version, &body);

                let response = crate::api::handle(fleet.clone(), request).await.unwrap();
                assert_eq!(response.status(), status);
            }

            let response = crate::api::handle(fleet, request("/agents/1"))
                .await
                .unwrap();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let agent: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(agent.get("title"), Some(&Value::Null));
            assert_eq!(agent["metadata_version"], "3");
        });
    }

    #[test]
    fn set_title_rejects_invalid_bodies() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bodies = [
            json!({}),
            json!({
                "title": 1,
            }),
            json!({
                "title": "Title",
                "extensions": {},
            }),
        ];

        runtime.block_on(async {
            let fleet = fleet().await;

            for body in bodies {
                let request = title_request("invalid", "1", &body);

                let response = crate::api::handle(fleet.clone(), request).await.unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            }
        });
    }

    #[test]
    fn set_title_requires_mutation_headers() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let body = json!({
            "title": "Title",
        });

        runtime.block_on(async {
            let fleet = fleet().await;

            for header in ["idempotency-key", "if-match"] {
                let mut request = title_request("missing", "1", &body);
                request.headers_mut().remove(header);

                let response = crate::api::handle(fleet.clone(), request).await.unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            }

            for value in ["1", "*", "W/\"1\"", "\"bad\"", "\"1\", \"2\""] {
                let mut request = title_request("invalid", "1", &body);
                request
                    .headers_mut()
                    .insert("if-match", value.parse().unwrap());

                let response = crate::api::handle(fleet.clone(), request).await.unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            }

            let mut request = title_request("duplicate", "1", &body);
            request
                .headers_mut()
                .append("if-match", "\"1\"".parse().unwrap());

            let response = crate::api::handle(fleet, request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        });
    }

    #[test]
    fn set_title_rejects_missing_agents_and_wrong_methods() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let body = json!({
            "title": "Title",
        });
        let mut missing = title_request("missing", "1", &body);
        *missing.uri_mut() = "/agents/99/title".parse().unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;

            let response = crate::api::handle(fleet.clone(), missing).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);

            let response = crate::api::handle(fleet, request("/agents/1/title"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        });
    }

    #[test]
    fn set_pinned_updates_reads_and_settling_unpins() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pin = |key: &str, expected: &str, body: &'static [u8]| {
            Request::builder()
                .method("PUT")
                .uri("/agents/1/pinned")
                .header("content-type", "application/json")
                .header("idempotency-key", key)
                .header("if-match", format!("\"{expected}\""))
                .body(Full::new(Bytes::from_static(body)))
                .unwrap()
        };

        runtime.block_on(async {
            let fleet = idle_fleet().await;
            let response =
                crate::api::handle(fleet.clone(), pin("pin", "1", br#"{"pinned":true}"#))
                    .await
                    .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);

            let response = crate::api::handle(fleet.clone(), request("/agents/1"))
                .await
                .unwrap();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let agent: Value = serde_json::from_slice(&bytes).unwrap();
            let pinned = agent;
            assert!(pinned["pinned"].is_u64());
            let response = crate::api::handle(fleet.clone(), request("/agents"))
                .await
                .unwrap();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let page: Value = serde_json::from_slice(&bytes).unwrap();
            let summary = &page["agents"][0];
            assert_eq!(summary["pinned"], pinned["pinned"]);

            let settle = Request::builder()
                .method("PUT")
                .uri("/agents/1/inbox")
                .header("content-type", "application/json")
                .header("idempotency-key", "settle")
                .header(
                    "if-match",
                    format!("\"{}\"", summary["version"].as_str().unwrap()),
                )
                .body(Full::new(Bytes::from_static(br#"{"state":"settled"}"#)))
                .unwrap();
            let response = crate::api::handle(fleet.clone(), settle).await.unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);

            let response = crate::api::handle(fleet.clone(), request("/agents/1"))
                .await
                .unwrap();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let agent: Value = serde_json::from_slice(&bytes).unwrap();
            let version = agent["metadata_version"].as_str().unwrap();
            assert_eq!(agent["pinned"], Value::Null);
            assert_ne!(agent["metadata_version"], pinned["metadata_version"]);

            let cases = [
                (
                    "pin-settled",
                    br#"{"pinned":true}"#.as_slice(),
                    StatusCode::CONFLICT,
                ),
                (
                    "invalid",
                    br#"{"pinned":null}"#.as_slice(),
                    StatusCode::BAD_REQUEST,
                ),
                (
                    "unpin",
                    br#"{"pinned":false}"#.as_slice(),
                    StatusCode::NO_CONTENT,
                ),
            ];
            for (key, body, status) in cases {
                let request = pin(key, version, body);
                let response = crate::api::handle(fleet.clone(), request).await.unwrap();
                assert_eq!(response.status(), status, "{key}");
            }
        });
    }

    #[test]
    fn create_agent_accepts_title() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let body = json!({
            "id": "2",
            "role": "assistant",
            "title": "Initial title",
        });
        let create = Request::builder()
            .method("POST")
            .uri("/agents")
            .header("content-type", "application/json")
            .header("idempotency-key", "create-titled")
            .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;

            let response = crate::api::handle(fleet.clone(), create).await.unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);

            let response = crate::api::handle(fleet, request("/agents/2"))
                .await
                .unwrap();
            assert_eq!(response.headers()["read-revision"], "1");
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let agent: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                agent,
                json!({
                    "id": "2",
                    "role": "assistant",
                    "parameters": {},
                    "title": "Initial title",
                    "pinned": null,
                    "metadata_version": "1",
                })
            );
        });
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
    fn scopes_settle_without_clients_and_release_idle_shutdown() {
        use fleet_kit::Agents as _;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let fleet = fleet().await;
                    let store = &fleet.state.store;
                    store.consume_messages(1.into(), None, true).await.unwrap();
                    let mut response = model_response("Working");
                    response.output.push(ModelOutput::ToolCall(ToolCall {
                        id: "drive".into(),
                        tool: "delegate".into(),
                        is_async: true,
                        input: JsonObject::new(),
                    }));
                    store
                        .append_model_response(1.into(), 0.into(), true, &response)
                        .await
                        .unwrap();

                    future::join(fleet.run_agent_scopes(), async {
                        let access = store.agent_access(1.into()).for_call("drive".into());
                        let role = Configured {
                            name: "child".into(),
                            config: JsonObject::new(),
                        };
                        let child = access
                            .create("child", &role, &JsonObject::new())
                            .await
                            .unwrap();
                        let message = access.send("message", child, &[]).await.unwrap();
                        store
                            .append_tool_result(
                                1.into(),
                                &ToolResult {
                                    call_id: "drive".into(),
                                    output: ToolOutput {
                                        content: vec![],
                                        structured_content: None,
                                        is_error: false,
                                    },
                                },
                            )
                            .await
                            .unwrap();
                        let error = access.wait_for_turn_end(child, message).await.unwrap_err();
                        assert!(matches!(error, StoreError::MessageNotFound));
                        fleet.idle_handler().wait_for_mutations().await;
                        fleet.shutdown();
                    })
                    .await;
                })
                .await
                .unwrap();
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
    fn pending_snoozes_allow_idle_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = idle_fleet().await.state.store.clone();
            let fleet = Fleet::new(store, Some(Duration::from_secs(120)));
            let version = fleet.collection_revision().await.unwrap();
            fleet
                .set_inbox(
                    "later".into(),
                    1.into(),
                    version,
                    InboxAction::Snooze(crate::fleet::now() + 86_400_000),
                )
                .await
                .unwrap();
            let worker = pin!(fleet.run_snoozes());
            let idle = pin!(fleet_core::IdleHandler::on_idle(fleet.idle_handler()));
            let output =
                tokio::time::timeout(Duration::from_secs(3600), future::select(worker, idle))
                    .await
                    .unwrap();
            assert!(matches!(output, future::Either::Right(_)));
        });
    }

    /// Configures a release delay and gives `role-0` its own workspace.
    async fn releasing_fleet(settled_for: u64) -> Fleet {
        let fleet = idle_fleet().await.with_configuration(
            serde_json::from_value(json!({
                "release_settled_workspaces_after_ms": settled_for,
            }))
            .unwrap(),
        );
        let Value::Object(extensions) = json!({ "fleet.workspace": { "name": "fake:default" } })
        else {
            unreachable!()
        };
        let role = Role {
            name: "role-0".into(),
            parameters: BTreeMap::new(),
            model: Configured {
                name: "model".into(),
                config: JsonObject::new(),
            },
            tools: BTreeMap::new(),
            system_prompt: String::new(),
            compaction: None,
            extensions,
        };
        fleet.put_role("role".into(), role, None).await.unwrap();
        fleet
    }

    async fn set_inbox(fleet: &Fleet, key: &str, agent: u128, action: InboxAction) {
        let page = fleet.agents(AgentsQuery::default()).await.unwrap();
        let summary = page
            .agents
            .iter()
            .find(|summary| summary.id == agent.into())
            .unwrap();
        fleet
            .set_inbox(key.into(), agent.into(), summary.version, action)
            .await
            .unwrap();
    }

    async fn wait_for_releases(fleet: &Fleet) {
        while fleet
            .state
            .store
            .workspace_releases(u64::MAX, Some(0))
            .await
            .unwrap()
            != (vec![], None)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn settled_workspaces_are_released_once_their_deadline_passes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fleet = releasing_fleet(200).await;
            let releases = Releases::default();
            future::join(fleet.run_workspace_releases(&releases), async {
                set_inbox(&fleet, "settle", 1, InboxAction::Settle).await;
                set_inbox(&fleet, "restore", 1, InboxAction::Pending).await;
                tokio::time::sleep(Duration::from_millis(400)).await;
                assert_eq!(*releases.released.lock().unwrap(), []);

                set_inbox(&fleet, "settle-again", 1, InboxAction::Settle).await;
                tokio::time::timeout(Duration::from_secs(5), wait_for_releases(&fleet))
                    .await
                    .unwrap();
                assert_eq!(*releases.released.lock().unwrap(), [AgentId::from(1)]);
                let page = fleet.agents(AgentsQuery::default()).await.unwrap();
                assert_eq!(page.agents[0].inbox, fleet_kit::InboxState::Settled);

                set_inbox(&fleet, "restore-again", 1, InboxAction::Pending).await;
                fleet.shutdown();
            })
            .await;
            assert_eq!(*releases.released.lock().unwrap(), [AgentId::from(1)]);
        });
    }

    #[test]
    fn failing_releases_retry_without_blocking_others() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fleet = releasing_fleet(0).await;
            for (id, role) in [(2, "role-0"), (3, "deleted")] {
                let agent = Agent {
                    id: id.into(),
                    role: Configured {
                        name: role.into(),
                        config: JsonObject::new(),
                    },
                    extensions: JsonObject::new(),
                };
                fleet
                    .create_agent(format!("create-{id}"), agent, None)
                    .await
                    .unwrap();
            }
            for id in 1..=3 {
                set_inbox(&fleet, &format!("settle-{id}"), id, InboxAction::Settle).await;
            }
            let releases = Releases::default();
            releases.failures.lock().unwrap().insert(1.into(), 3);

            future::join(fleet.run_workspace_releases(&releases), async {
                tokio::time::timeout(Duration::from_secs(5), wait_for_releases(&fleet))
                    .await
                    .unwrap();
                fleet.shutdown();
            })
            .await;
            assert_eq!(
                *releases.released.lock().unwrap(),
                [AgentId::from(2), AgentId::from(1)]
            );
        });
    }

    fn environment(agent: u128) -> fleet_kit::WorkspaceContext {
        fleet_kit::WorkspaceContext {
            workspace: Configured {
                name: "fake:default".into(),
                config: JsonObject::new(),
            },
            agent_id: agent.into(),
            id: agent.into(),
        }
    }

    #[test]
    fn releases_in_progress_hold_back_use_and_finish_before_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fleet = releasing_fleet(0).await;
            set_inbox(&fleet, "settle", 1, InboxAction::Settle).await;
            let releases = Releases::default();
            let paused = releases.paused.clone().write_owned().await;

            future::join(fleet.run_workspace_releases(&releases), async {
                releases.entered.notified().await;
                let child = fleet.workspace_locks().wrap(releases.clone());
                let context = environment(1);
                let mut open = pin!(Workspaces::open(&child, &context));
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(futures_util::FutureExt::now_or_never(open.as_mut()).is_none());
                fleet.shutdown();
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(paused);
                open.await.unwrap_err();
            })
            .await;
            assert_eq!(*releases.released.lock().unwrap(), [AgentId::from(1)]);
            assert_eq!(
                fleet
                    .state
                    .store
                    .workspace_releases(u64::MAX, Some(0))
                    .await
                    .unwrap(),
                (vec![], None)
            );
        });
    }

    #[test]
    fn environments_in_use_are_released_after_their_use() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fleet = releasing_fleet(0).await;
            set_inbox(&fleet, "settle", 1, InboxAction::Settle).await;
            let (due, _) = fleet
                .state
                .store
                .workspace_releases(u64::MAX, Some(0))
                .await
                .unwrap();
            let releases = Releases::default();
            let borrower = fleet.workspace_locks().wrap(releases.clone());
            Workspaces::read(&borrower, &environment(1), "file", 1)
                .await
                .unwrap_err();

            let busy = fleet.try_release_workspace(&releases, due[0]).await;
            drop(borrower);
            let finished = fleet.try_release_workspace(&releases, due[0]).await;

            assert!(matches!(busy, Ok(ReleaseAttempt::Busy)));
            assert!(matches!(finished, Ok(ReleaseAttempt::Finished)));
            assert_eq!(*releases.released.lock().unwrap(), [AgentId::from(1)]);
        });
    }

    #[test]
    fn pending_releases_allow_idle_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = releasing_fleet(86_400_000).await.state.store.clone();
            let fleet = Fleet::new(store, Some(Duration::from_secs(120))).with_configuration(
                serde_json::from_value(json!({
                    "release_settled_workspaces_after_ms": 86_400_000,
                }))
                .unwrap(),
            );
            set_inbox(&fleet, "settle", 1, InboxAction::Settle).await;
            let releases = Releases::default();
            let worker = pin!(fleet.run_workspace_releases(&releases));
            let idle = pin!(fleet_core::IdleHandler::on_idle(fleet.idle_handler()));
            let output =
                tokio::time::timeout(Duration::from_secs(3600), future::select(worker, idle))
                    .await
                    .unwrap();
            assert!(matches!(output, future::Either::Right(_)));
        });
    }

    #[test]
    fn notification_worker_completes_turn_notifications() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let key = crate::web_push::WebPush::generate_key().unwrap();
            let fleet = idle_fleet()
                .await
                .with_notifications(Notifications::new(&key).unwrap());
            assert!(
                fleet
                    .state
                    .store
                    .next_notification()
                    .await
                    .unwrap()
                    .is_some()
            );
            let worker_fleet = fleet.clone();
            let worker = tokio::spawn(async move { worker_fleet.run_notifications().await });
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let changed = fleet.state.store.notification_changed();
                    if fleet
                        .state
                        .store
                        .next_notification()
                        .await
                        .unwrap()
                        .is_none()
                    {
                        break;
                    }
                    changed.await;
                }
            })
            .await
            .unwrap();
            fleet.shutdown();
            tokio::time::timeout(Duration::from_secs(1), worker)
                .await
                .unwrap()
                .unwrap();
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
                "interrupt_pending": false,
            });
            let pending_b = json!({
                "status": "pending",
                "id": "b",
                "tool": "shell",
                "is_async": true,
                "interrupt_pending": false,
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
                .interrupt_agent("interrupt".into(), 1.into(), true, None)
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
                    "interrupt_includes_async_tool_calls": false,
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

            store
                .publish_compaction_started(1.into(), 0.into())
                .await
                .unwrap();
            let text = next_frame(&mut body).await;
            assert_eq!(
                text,
                "event: preview\ndata: {\"status\":\"compacting\",\"input_cursor\":\"0\"}\n\n",
            );
            let response =
                crate::api::handle(fleet.clone(), request("/agents/1/preview/subscription"))
                    .await
                    .unwrap();
            let snapshot = next_frame(&mut response.into_body()).await;
            assert_eq!(snapshot, text);
            let agent = fleet.agent(1.into(), None).await.unwrap();
            assert_eq!(agent.revision, 2.into());

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
                .interrupt_agent("interrupt".into(), 1.into(), true, None)
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

    #[test]
    fn roles_are_edited_with_versions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fleet = Fleet::new(Store::memory().unwrap(), None);
        let send = async |method, key: &str, precondition: Option<(&str, &str)>, body: Value| {
            let mut request = Request::builder()
                .method(method)
                .uri("/roles/assistant")
                .header("content-type", "application/json")
                .header("idempotency-key", key);
            if let Some((name, value)) = precondition {
                request = request.header(name, value);
            }
            let request = request
                .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
                .unwrap();
            crate::api::handle(fleet.clone(), request)
                .await
                .unwrap()
                .status()
        };
        let body = async |path| {
            let response = crate::api::handle(fleet.clone(), request(path))
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        };
        let definition = json!({
            "title": "Assistant",
            "model": {
                "name": "work:model",
            },
            "system_prompt": "Be helpful.\n",
        });

        runtime.block_on(async {
            let status = send("PUT", "missing", None, definition.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let create = Some(("if-none-match", "*"));
            let status = send("PUT", "create", create, definition.clone()).await;
            assert_eq!(status, StatusCode::CREATED);
            let status = send("PUT", "create", create, definition.clone()).await;
            assert_eq!(status, StatusCode::CREATED);
            let status = send("PUT", "again", create, definition.clone()).await;
            assert_eq!(status, StatusCode::PRECONDITION_FAILED);

            let (status, roles) = body("/roles").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                roles,
                json!([
                    {
                        "name": "assistant",
                        "title": "Assistant",
                        "hidden": false,
                        "parameters": {},
                    },
                ]),
            );

            let (status, role) = body("/roles/assistant").await;
            assert_eq!(status, StatusCode::OK);
            let version = role["version"].as_str().unwrap().to_owned();
            assert_eq!(
                role,
                json!({
                    "name": "assistant",
                    "version": version,
                    "title": "Assistant",
                    "hidden": false,
                    "model": {
                        "name": "work:model",
                        "config": {},
                    },
                    "tools": {},
                    "parameters": {},
                    "system_prompt": "Be helpful.\n",
                }),
            );

            let current = format!("\"{version}\"");
            let mut edited = definition.clone();
            edited["system_prompt"] = "Be brief.\n".into();
            let status = send("PUT", "edit", Some(("if-match", &current)), edited).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            let (_, role) = body("/roles/assistant").await;
            assert_eq!(role["system_prompt"], "Be brief.\n");

            let status = send("DELETE", "delete", Some(("if-match", &current)), json!({})).await;
            assert_eq!(status, StatusCode::PRECONDITION_FAILED);
            let current = format!("\"{}\"", role["version"].as_str().unwrap());
            let status = send("DELETE", "delete", Some(("if-match", &current)), json!({})).await;
            assert_eq!(status, StatusCode::NO_CONTENT);

            let (status, error) = body("/roles/assistant").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(error["error"]["code"], "role_not_found");
        });
    }
}
