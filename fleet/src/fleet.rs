use crate::{ErrorHandler, IdleHandler};
use core::{fmt, pin::pin, time::Duration};
use fleet_core::{
    Agent, AgentId, ClientStore as _, ClientStoreErrorKind, Event, EventCursor, Message, MessageId,
};
use fleet_kit::{EventDirection, Queries as _, QueriesErrorKind, TurnEndEvent};
use fleet_simulator::{
    ModelsSimulator, RolesSimulator, Scheduler, Simulator, StoreSimulator, StoreSimulatorError,
    ToolsSimulator,
};
use futures_util::{Stream, TryStreamExt as _, future};
use std::sync::{Arc, LazyLock};

static SCHEDULER: LazyLock<Scheduler> = LazyLock::new(|| {
    let scheduler = Scheduler::new(0);
    scheduler.disable_suspensions();
    scheduler.disable_faults();
    scheduler
});

/// An AI agent harness.
#[derive(Clone)]
pub struct Fleet {
    state: Arc<State>,
}

/// An error returned by [`Fleet`].
#[derive(Debug)]
pub enum FleetError {
    /// Shutdown has begun.
    ShuttingDown,
    /// A store operation failed.
    Store(StoreSimulatorError<ClientStoreErrorKind>),
    /// A query failed.
    Query(StoreSimulatorError<QueriesErrorKind>),
}

struct State {
    idle_handler: IdleHandler,
    store: StoreSimulator<'static>,
    roles: RolesSimulator<'static>,
    models: ModelsSimulator<'static>,
    tools: ToolsSimulator<'static>,
}

impl Fleet {
    /// Creates a fleet with simulated providers.
    #[must_use]
    pub fn new(idle_timeout: Option<Duration>) -> Self {
        let simulator = Simulator::new(&SCHEDULER, 0);

        Self {
            state: Arc::new(State {
                idle_handler: IdleHandler::new(idle_timeout),
                store: simulator.store,
                roles: simulator.roles,
                models: simulator.models,
                tools: simulator.tools,
            }),
        }
    }

    /// Runs agents until shutdown and waits for pending mutations.
    pub async fn run(&self) {
        let state = &self.state;
        let partition = state.store.add_partition();

        {
            let agents = async {
                loop {
                    let last_activity = state.idle_handler.last_activity();

                    fleet_core::run(
                        &state.idle_handler,
                        ErrorHandler,
                        state.store.partition(partition),
                        &state.roles,
                        &state.models,
                        &state.tools,
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

            future::select(pin!(shutdown), pin!(agents)).await;
        }

        state.store.remove_partition(partition);
    }

    /// Begins shutdown.
    pub fn shutdown(&self) {
        self.state.idle_handler.shutdown();
    }

    /// Waits for shutdown to begin.
    pub async fn wait_for_shutdown(&self) {
        self.state.idle_handler.wait_for_shutdown().await;
    }

    /// Streams events in the given direction.
    ///
    /// # Errors
    ///
    /// Yields an error if the agent is missing or the store fails.
    pub fn events(
        &self,
        agent_id: AgentId,
        from: Option<EventCursor>,
        until: Option<EventCursor>,
        direction: EventDirection,
    ) -> impl Stream<Item = Result<Event, FleetError>> + Send {
        self.state
            .store
            .events(agent_id, from, until, direction)
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
        F: Future<Output = Result<(), StoreSimulatorError<ClientStoreErrorKind>>> + Send + 'static,
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
        }
    }
}

impl core::error::Error for FleetError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::ShuttingDown => None,
            Self::Store(source) => Some(source),
            Self::Query(source) => Some(source),
        }
    }
}
