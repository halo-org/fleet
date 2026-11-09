use crate::{Probability, RandomError, Scheduler, TrySuspendConfig, WrapConfig};
use core::{
    fmt, mem,
    task::{Poll, Waker},
};
use fastrand::Rng;
use fleet_core::{
    Agent, AgentId, AgentState, ClientStore, ClientStoreErrorKind, Compaction, Event, EventCursor,
    EventPayload, Message, MessageId, MessageMode, ModelOutput, ModelResponse, ModelResponseChunk,
    RuntimeStore, RuntimeStoreErrorKind, ToolCallId, ToolResult, TurnEnd, Wake,
};
use futures_util::{Stream, StreamExt as _, future, stream};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Mutex, PoisonError},
};

/// An in-memory simulator for [`RuntimeStore`] and [`ClientStore`].
///
/// Agents are assigned to explicitly added wake partitions. Use [`Self::partition`] to get a
/// [`RuntimeStore`] for one.
///
/// Errors may be injected before or after a mutation. A failed operation may have changed state.
pub struct StoreSimulator<'sim> {
    scheduler: &'sim Scheduler,
    config: StoreSimulatorConfig,
    state: Mutex<State>,
}

/// The ID of a wake partition in a [`StoreSimulator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionId(u64);

/// A [`RuntimeStore`] view of a [`StoreSimulator`] partition.
///
/// Each wake stream independently receives wakes for the agents assigned to this partition.
/// New streams replay prior releases before yielding current outstanding work.
#[derive(Clone, Copy)]
pub struct StorePartition<'store, 'sim> {
    store: &'store StoreSimulator<'sim>,
    id: PartitionId,
}

/// An owned copy of an agent's state in the store simulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSnapshot {
    /// The agent's metadata.
    pub agent: Agent,
    /// The event log in chronological order.
    pub events: Vec<Event>,
    /// The pending messages in queue order.
    pub pending_messages: Vec<Message>,
    /// The unresolved tool calls and their preparation state.
    pub unresolved_tool_calls: BTreeMap<ToolCallId, ToolCallSnapshot>,
    /// The event cursor the agent is parked at.
    pub parked_at: Option<EventCursor>,
    /// Whether the agent has an interrupt intent.
    pub interrupt_intent: bool,
}

/// The state of an unresolved tool call in the store simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCallSnapshot {
    /// Whether the call has been prepared at least once.
    pub has_been_prepared: bool,
    /// Whether the model may continue without waiting for the call's result.
    pub is_async: bool,
}

struct State {
    idempotency_keys: BTreeMap<String, ClientRequest>,
    agents: BTreeMap<AgentId, AgentRecord>,
    partitions: BTreeMap<PartitionId, PartitionState>,
    next_partition_id: u64,
    next_subscription_id: u64,
    ready_wakers: Vec<Waker>,
}

struct AgentRecord {
    agent: Agent,
    wake_owner: Option<PartitionId>,
    events: Vec<Event>,
    pending_messages: Vec<Message>,
    parked_at: Option<EventCursor>,
    interrupt_intent: bool,
    message_ids: BTreeSet<MessageId>,
    tool_call_ids: BTreeSet<ToolCallId>,
    unresolved_tool_calls: BTreeMap<ToolCallId, ToolCallSnapshot>,
    response_stream: Option<ResponseStreamState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClientRequest {
    CreateAgent {
        agent: Agent,
        message: Option<Message>,
    },
    InterruptAgent {
        agent_id: AgentId,
        message: Option<Message>,
    },
    SendMessage {
        agent_id: AgentId,
        message: Message,
    },
}

struct ResponseStreamState {
    cursor: EventCursor,
    status: ResponseStreamStatus,
}

enum ResponseStreamStatus {
    Active(Vec<ModelResponseChunk>),
    Invalidated,
    Superseded,
}

struct PartitionState {
    is_member: bool,
    released_agents: BTreeSet<AgentId>,
    subscriptions: BTreeMap<u64, WakeSubscriptionState>,
}

struct WakeSubscriptionState {
    waker: Option<Waker>,
    pending_wakes: VecDeque<Wake>,
}

struct WakeSubscription<'store, 'sim> {
    simulator: &'store StoreSimulator<'sim>,
    partition: PartitionId,
    id: u64,
    is_idle: bool,
}

/// An error returned by [`StoreSimulator`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StoreSimulatorError<K> {
    inner: StoreSimulatorErrorInner<K>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StoreSimulatorErrorInner<K> {
    Random(RandomError),
    Kind(K, &'static str),
}

type RuntimeError = StoreSimulatorError<RuntimeStoreErrorKind>;

type ClientError = StoreSimulatorError<ClientStoreErrorKind>;

struct StoreSimulatorConfig {
    agent: WrapConfig,
    agent_state: WrapConfig,
    settle_interrupt: WrapConfig,
    park: WrapConfig,
    consume_messages: WrapConfig,
    publish_model_response_chunk: WrapConfig,
    append_model_response: WrapConfig,
    append_compaction: WrapConfig,
    prepare_tool_call: WrapConfig,
    append_tool_result: WrapConfig,
    create_agent: WrapConfig,
    interrupt_agent: WrapConfig,
    send_message: WrapConfig,
    wakes: TrySuspendConfig,
    wakes_repartition: Probability,
    events_suspend: Probability,
    events_error: Probability,
}

impl<'sim> StoreSimulator<'sim> {
    /// Creates an empty store simulator from the given seed.
    pub fn new(scheduler: &'sim Scheduler, seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);
        let config = StoreSimulatorConfig::generate(&mut rng);

        Self {
            scheduler,
            config,
            state: Mutex::new(State {
                idempotency_keys: BTreeMap::new(),
                agents: BTreeMap::new(),
                partitions: BTreeMap::new(),
                next_partition_id: 0,
                next_subscription_id: 0,
                ready_wakers: Vec::new(),
            }),
        }
    }

    /// Adds a wake partition and rebalances agents across all partitions.
    #[must_use]
    pub fn add_partition(&self) -> PartitionId {
        self.mutate(|state| {
            let id = PartitionId(state.next_partition_id);
            state.next_partition_id = id.0.checked_add(1).expect("IDs never overflow");

            state.partitions.insert(
                id,
                PartitionState {
                    is_member: true,
                    released_agents: BTreeSet::new(),
                    subscriptions: BTreeMap::new(),
                },
            );

            state.rebalance_wakes();

            id
        })
    }

    /// Removes a wake partition and rebalances its agents across the remaining partitions.
    ///
    /// The partition's wake streams still yield releases for its agents, but it receives no new
    /// agents.
    ///
    /// # Panics
    ///
    /// Panics if the partition was never added or has already been removed.
    pub fn remove_partition(&self, partition: PartitionId) {
        let is_removed = self.mutate(|state| {
            let Some(partition) = state
                .partitions
                .get_mut(&partition)
                .filter(|partition| partition.is_member)
            else {
                return false;
            };

            partition.is_member = false;
            state.rebalance_wakes();

            true
        });

        assert!(is_removed, "partition exists and has not been removed");
    }

    /// Returns a [`RuntimeStore`] view of the store for the given partition.
    ///
    /// # Panics
    ///
    /// Panics if the partition was never added.
    #[must_use]
    pub fn partition(&self, partition: PartitionId) -> StorePartition<'_, 'sim> {
        assert!(
            self.state
                .lock()
                .expect("never poisoned")
                .partitions
                .contains_key(&partition),
            "partition exists"
        );

        StorePartition {
            store: self,
            id: partition,
        }
    }

    /// Returns a consistent, owned copy of every agent's state.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<AgentId, AgentSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .agents
            .iter()
            .map(|(id, record)| {
                let snapshot = AgentSnapshot {
                    agent: record.agent.clone(),
                    events: record.events.clone(),
                    pending_messages: record.pending_messages.clone(),
                    unresolved_tool_calls: record.unresolved_tool_calls.clone(),
                    parked_at: record.parked_at,
                    interrupt_intent: record.interrupt_intent,
                };

                (*id, snapshot)
            })
            .collect()
    }

    /// Moves one randomly selected agent to another wake partition.
    ///
    /// Does nothing when faults are disabled, without agents, or with fewer than two partitions.
    pub fn repartition(&self) {
        self.mutate(|state| {
            if state.agents.is_empty() || state.member_partitions().count() < 2 {
                return;
            }

            if let Some(index) = self
                .scheduler
                .sample_fault_position(Probability::ALWAYS, 0..state.agents.len())
            {
                let agent_id = *state
                    .agents
                    .keys()
                    .nth(index)
                    .expect("selected agent exists");

                state.repartition_agent(self.scheduler, agent_id, Probability::ALWAYS);
            }
        });
    }

    fn mutate<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let (result, wakers) = {
            let mut state = self.state.lock().expect("never poisoned");

            let result = f(&mut state);

            (result, mem::take(&mut state.ready_wakers))
        };

        for waker in wakers {
            waker.wake();
        }

        result
    }

    fn partition_wakes(
        &self,
        partition: PartitionId,
    ) -> impl Stream<Item = Result<Wake, RuntimeError>> + Send {
        let subscription_future = async move { WakeSubscription::new(self, partition) };

        stream::once(subscription_future).flat_map(|subscription| {
            stream::unfold(Some(subscription), async |subscription| {
                let mut subscription = subscription?;
                let result = self
                    .scheduler
                    .try_suspend(&self.config.wakes, RuntimeError::random)
                    .await;

                match result {
                    Ok(()) => Some((Ok(subscription.next().await), Some(subscription))),
                    Err(error) => Some((Err(error), None)),
                }
            })
        })
    }
}

impl RuntimeStore for StorePartition<'_, '_> {
    type Error = RuntimeError;

    async fn agent(&self, agent_id: AgentId) -> Result<Option<Agent>, Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.agent,
                StoreSimulatorError::random,
                async {
                    let agent = self
                        .store
                        .state
                        .lock()
                        .expect("never poisoned")
                        .agents
                        .get(&agent_id)
                        .map(|record| record.agent.clone());

                    Ok(agent)
                },
            )
            .await
    }

    async fn agent_state(&self, agent_id: AgentId) -> Result<Option<AgentState>, Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.agent_state,
                StoreSimulatorError::random,
                async {
                    let agent_state = self
                        .store
                        .state
                        .lock()
                        .expect("never poisoned")
                        .agents
                        .get(&agent_id)
                        .map(AgentRecord::state);

                    Ok(agent_state)
                },
            )
            .await
    }

    fn wakes(&self) -> impl Stream<Item = Result<Wake, Self::Error>> + Send {
        self.store.partition_wakes(self.id)
    }

    fn events(
        &self,
        agent_id: AgentId,
        from: EventCursor,
        until: Option<EventCursor>,
    ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
        #[expect(clippy::significant_drop_tightening, reason = "false positive")]
        let items_future = async move {
            let state = self.store.state.lock().expect("never poisoned");

            let Some(agent) = state.agents.get(&agent_id) else {
                return Vec::from([Err(RuntimeError::conflict("agent does not exist"))]);
            };

            let start = until.map_or_default(|until| {
                agent.events.partition_point(|event| event.cursor <= until)
            });
            let end = agent.events.partition_point(|event| event.cursor <= from);

            let mut items = agent
                .events
                .get(start..end)
                .unwrap_or_default()
                .iter()
                .rev()
                .cloned()
                .map(Ok)
                .collect::<Vec<_>>();

            if let Some(position) = self
                .store
                .scheduler
                .sample_fault_position(self.store.config.events_error, 0..=items.len())
            {
                items.truncate(position);
                items.push(Err(RuntimeError::random()));
            }

            items
        };

        stream::once(items_future).flat_map(|items| {
            self.store
                .scheduler
                .suspend_stream(self.store.config.events_suspend, items)
        })
    }

    async fn settle_interrupt(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
        tool_results: &[ToolResult],
    ) -> Result<(), Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.settle_interrupt,
                StoreSimulatorError::random,
                async {
                    self.store.mutate(|state| {
                        state.settle_interrupt(agent_id, expected_latest_event, tool_results)
                    })
                },
            )
            .await
    }

    async fn park(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
    ) -> Result<(), Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.park,
                StoreSimulatorError::random,
                async {
                    self.store
                        .mutate(|state| state.park(agent_id, expected_latest_event))
                },
            )
            .await
    }

    async fn consume_messages(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
        allow_queued: bool,
    ) -> Result<bool, Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.consume_messages,
                StoreSimulatorError::random,
                async {
                    self.store.mutate(|state| {
                        state.consume_messages(agent_id, expected_latest_event, allow_queued)
                    })
                },
            )
            .await
    }

    async fn publish_model_response_chunk(
        &self,
        agent_id: AgentId,
        input_cursor: EventCursor,
        sequence: u32,
        chunk: &ModelResponseChunk,
    ) -> Result<(), Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.publish_model_response_chunk,
                StoreSimulatorError::random,
                async {
                    self.store.mutate(|state| {
                        state.publish_model_response_chunk(agent_id, input_cursor, sequence, chunk)
                    })
                },
            )
            .await
    }

    async fn append_model_response(
        &self,
        agent_id: AgentId,
        expected_latest_event: EventCursor,
        ends_turn: bool,
        response: &ModelResponse,
    ) -> Result<(), Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.append_model_response,
                StoreSimulatorError::random,
                async {
                    self.store.mutate(|state| {
                        state.append_model_response(
                            agent_id,
                            expected_latest_event,
                            ends_turn,
                            response,
                        )
                    })
                },
            )
            .await
    }

    async fn append_compaction(
        &self,
        agent_id: AgentId,
        expected_latest_event: EventCursor,
        compaction: &Compaction,
    ) -> Result<(), Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.append_compaction,
                StoreSimulatorError::random,
                async {
                    self.store.mutate(|state| {
                        state.append_compaction(agent_id, expected_latest_event, compaction)
                    })
                },
            )
            .await
    }

    async fn prepare_tool_call(
        &self,
        agent_id: AgentId,
        call_id: &ToolCallId,
    ) -> Result<bool, Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.prepare_tool_call,
                StoreSimulatorError::random,
                async {
                    self.store
                        .mutate(|state| state.prepare_tool_call(agent_id, call_id))
                },
            )
            .await
    }

    async fn append_tool_result(
        &self,
        agent_id: AgentId,
        result: &ToolResult,
    ) -> Result<(), Self::Error> {
        self.store
            .scheduler
            .wrap(
                &self.store.config.append_tool_result,
                StoreSimulatorError::random,
                async {
                    self.store
                        .mutate(|state| state.append_tool_result(agent_id, result))
                },
            )
            .await
    }
}

impl ClientStore for StoreSimulator<'_> {
    type Error = ClientError;

    async fn create_agent(
        &self,
        idempotency_key: &str,
        agent: &Agent,
        message: Option<&Message>,
    ) -> Result<(), Self::Error> {
        self.scheduler
            .wrap(
                &self.config.create_agent,
                StoreSimulatorError::random,
                async {
                    self.mutate(|state| {
                        state.handle_request(
                            self.scheduler,
                            self.config.wakes_repartition,
                            idempotency_key,
                            ClientRequest::CreateAgent {
                                agent: agent.clone(),
                                message: message.cloned(),
                            },
                        )
                    })
                },
            )
            .await
    }

    async fn interrupt_agent(
        &self,
        idempotency_key: &str,
        agent_id: AgentId,
        message: Option<&Message>,
    ) -> Result<(), Self::Error> {
        self.scheduler
            .wrap(
                &self.config.interrupt_agent,
                StoreSimulatorError::random,
                async {
                    self.mutate(|state| {
                        state.handle_request(
                            self.scheduler,
                            self.config.wakes_repartition,
                            idempotency_key,
                            ClientRequest::InterruptAgent {
                                agent_id,
                                message: message.cloned(),
                            },
                        )
                    })
                },
            )
            .await
    }

    async fn send_message(
        &self,
        idempotency_key: &str,
        agent_id: AgentId,
        message: &Message,
    ) -> Result<(), Self::Error> {
        self.scheduler
            .wrap(
                &self.config.send_message,
                StoreSimulatorError::random,
                async {
                    self.mutate(|state| {
                        state.handle_request(
                            self.scheduler,
                            self.config.wakes_repartition,
                            idempotency_key,
                            ClientRequest::SendMessage {
                                agent_id,
                                message: message.clone(),
                            },
                        )
                    })
                },
            )
            .await
    }
}

impl State {
    fn create_agent(
        &mut self,
        agent: &Agent,
        message: Option<&Message>,
    ) -> Result<(), ClientError> {
        if self.agents.contains_key(&agent.id) {
            return Err(ClientError::conflict("agent already exists"));
        }

        let mut agent_record = AgentRecord::new(agent.clone());

        let wake_owner = self
            .agents
            .len()
            .checked_rem(self.member_partitions().count())
            .and_then(|index| self.member_partitions().nth(index));

        agent_record.wake_owner = wake_owner;

        if let Some(message) = message {
            agent_record.push_pending_message(message.clone());
        }

        self.agents.insert(agent.id, agent_record);

        if message.is_some()
            && let Some(owner) = wake_owner
        {
            self.enqueue_wake(owner, Wake::Reconcile(agent.id));
        }

        Ok(())
    }

    fn interrupt_agent(
        &mut self,
        agent_id: AgentId,
        message: Option<&Message>,
    ) -> Result<(), ClientError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| ClientError::agent_not_found("agent does not exist"))?;

        if let Some(message) = message {
            if agent.message_ids.contains(&message.id) {
                return Err(ClientError::conflict("message ID already exists"));
            }

            agent.push_pending_message(message.clone());
        }

        agent.interrupt_intent = true;

        if let Some(owner) = agent.wake_owner {
            self.enqueue_wake(owner, Wake::Interrupt(agent_id));
        }

        Ok(())
    }

    fn send_message(&mut self, agent_id: AgentId, message: &Message) -> Result<(), ClientError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| ClientError::agent_not_found("agent does not exist"))?;

        if agent.message_ids.contains(&message.id) {
            return Err(ClientError::conflict("message ID already exists"));
        }

        agent.push_pending_message(message.clone());

        if let Some(owner) = agent.wake_owner {
            self.enqueue_wake(owner, Wake::Reconcile(agent_id));
        }

        Ok(())
    }

    fn settle_interrupt(
        &mut self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
        tool_results: &[ToolResult],
    ) -> Result<(), RuntimeError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| RuntimeError::conflict("agent does not exist"))?;

        agent.check_latest_event(expected_latest_event)?;

        if !agent.interrupt_intent {
            return Err(RuntimeError::conflict("agent has no interrupt intent"));
        }

        let tool_call_ids = tool_results
            .iter()
            .map(|result| &result.call_id)
            .collect::<BTreeSet<_>>();

        let resolves_all_tool_calls = tool_call_ids.len() == tool_results.len()
            && tool_call_ids.len() == agent.unresolved_tool_calls.len()
            && tool_call_ids
                .iter()
                .all(|id| agent.unresolved_tool_calls.contains_key(*id));

        if !resolves_all_tool_calls {
            return Err(RuntimeError::conflict(
                "results must match all unresolved tool calls exactly",
            ));
        }

        for result in tool_results {
            agent.append_tool_result(result);
        }

        agent.append_event(EventPayload::TurnEnd(TurnEnd::Interrupted));
        agent.interrupt_intent = false;
        agent.parked_at = agent.latest_event();

        Ok(())
    }

    fn park(
        &mut self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
    ) -> Result<(), RuntimeError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| RuntimeError::conflict("agent does not exist"))?;

        agent.check_latest_event(expected_latest_event)?;
        agent.parked_at = agent.latest_event();

        Ok(())
    }

    fn consume_messages(
        &mut self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
        allow_queued: bool,
    ) -> Result<bool, RuntimeError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| RuntimeError::conflict("agent does not exist"))?;

        agent.check_latest_event(expected_latest_event)?;

        if agent.interrupt_intent {
            return Err(RuntimeError::conflict("agent has an interrupt intent"));
        }

        let allow_queued = allow_queued
            && !agent
                .pending_messages
                .iter()
                .any(|message| message.mode == MessageMode::Steer);
        let mut queued_consumed = false;

        let messages = agent
            .pending_messages
            .extract_if(.., |message| match message.mode {
                MessageMode::Steer => true,
                MessageMode::Queue => allow_queued && !mem::replace(&mut queued_consumed, true),
            })
            .collect::<Vec<_>>();

        let consumed = !messages.is_empty();

        for message in messages {
            agent.append_event(EventPayload::Input(message.id, message.content));
        }

        Ok(consumed)
    }

    fn publish_model_response_chunk(
        &mut self,
        agent_id: AgentId,
        input_cursor: EventCursor,
        sequence: u32,
        chunk: &ModelResponseChunk,
    ) -> Result<(), RuntimeError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| RuntimeError::other("agent does not exist"))?;

        if agent
            .events
            .binary_search_by_key(&input_cursor, |event| event.cursor)
            .is_err()
        {
            return Err(RuntimeError::other("input cursor does not exist"));
        }

        let stream = agent
            .response_stream
            .get_or_insert_with(|| ResponseStreamState {
                cursor: input_cursor,
                status: ResponseStreamStatus::Active(Vec::new()),
            });

        if input_cursor < stream.cursor {
            return Ok(());
        }

        if input_cursor > stream.cursor {
            stream.cursor = input_cursor;
            stream.status = ResponseStreamStatus::Active(Vec::new());
        }

        if let ResponseStreamStatus::Active(chunks) = &mut stream.status {
            if u32::try_from(chunks.len()) == Ok(sequence) {
                chunks.push(chunk.clone());
            } else {
                stream.status = ResponseStreamStatus::Invalidated;
            }
        }

        Ok(())
    }

    fn append_model_response(
        &mut self,
        agent_id: AgentId,
        expected_latest_event: EventCursor,
        ends_turn: bool,
        response: &ModelResponse,
    ) -> Result<(), RuntimeError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| RuntimeError::conflict("agent does not exist"))?;

        agent.check_latest_event(Some(expected_latest_event))?;

        if agent.parked_at == Some(expected_latest_event) {
            return Err(RuntimeError::conflict(
                "agent is parked at the input cursor",
            ));
        }

        if agent
            .unresolved_tool_calls
            .values()
            .any(|call| !call.is_async)
        {
            return Err(RuntimeError::conflict(
                "agent has unresolved synchronous tool calls",
            ));
        }

        let mut call_ids = BTreeSet::new();

        for output in &response.output {
            let ModelOutput::ToolCall(call) = output else {
                continue;
            };

            if !call_ids.insert(&call.id) {
                return Err(RuntimeError::conflict("duplicate tool call ID in response"));
            }

            if agent.tool_call_ids.contains(&call.id) {
                return Err(RuntimeError::conflict("tool call ID already exists"));
            }
        }

        for output in &response.output {
            if let ModelOutput::ToolCall(call) = output {
                agent.tool_call_ids.insert(call.id.clone());
                agent.unresolved_tool_calls.insert(
                    call.id.clone(),
                    ToolCallSnapshot {
                        has_been_prepared: false,
                        is_async: call.is_async,
                    },
                );
            }
        }

        agent.append_event(EventPayload::ModelResponse(response.clone()));
        agent.response_stream = Some(ResponseStreamState {
            cursor: expected_latest_event,
            status: ResponseStreamStatus::Superseded,
        });

        if ends_turn {
            agent.append_event(EventPayload::TurnEnd(TurnEnd::Completed));
        }

        Ok(())
    }

    fn append_compaction(
        &mut self,
        agent_id: AgentId,
        expected_latest_event: EventCursor,
        compaction: &Compaction,
    ) -> Result<(), RuntimeError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| RuntimeError::conflict("agent does not exist"))?;

        agent.check_latest_event(Some(expected_latest_event))?;

        if agent
            .events
            .binary_search_by_key(&compaction.last_event, |event| event.cursor)
            .is_err()
        {
            return Err(RuntimeError::conflict("compaction cursor does not exist"));
        }

        let mut pending = BTreeMap::new();

        for event in agent
            .events
            .iter()
            .take_while(|event| event.cursor <= compaction.last_event)
        {
            match &event.payload {
                EventPayload::ModelResponse(response) => {
                    for output in &response.output {
                        if let ModelOutput::ToolCall(call) = output {
                            pending.insert(&call.id, call);
                        }
                    }
                }
                EventPayload::ToolResult(result) => {
                    pending.remove(&result.call_id);
                }
                EventPayload::Input(..)
                | EventPayload::Compaction(_)
                | EventPayload::TurnEnd(_) => {}
            }
        }

        if pending.values().any(|call| !call.is_async) {
            return Err(RuntimeError::conflict(
                "agent has unresolved synchronous tool calls",
            ));
        }

        let provided = compaction
            .pending_tool_calls
            .iter()
            .map(|call| (&call.id, call))
            .collect::<BTreeMap<_, _>>();

        if provided.len() != compaction.pending_tool_calls.len() || provided != pending {
            return Err(RuntimeError::conflict("pending tool calls do not match"));
        }

        let was_parked = agent.parked_at == Some(expected_latest_event);

        agent.append_event(EventPayload::Compaction(compaction.clone()));

        if was_parked {
            agent.parked_at = agent.latest_event();
        }

        Ok(())
    }

    fn prepare_tool_call(
        &mut self,
        agent_id: AgentId,
        call_id: &ToolCallId,
    ) -> Result<bool, RuntimeError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| RuntimeError::conflict("agent does not exist"))?;

        if !agent.tool_call_ids.contains(call_id) {
            return Err(RuntimeError::conflict("tool call does not exist"));
        }

        let call = agent
            .unresolved_tool_calls
            .get_mut(call_id)
            .ok_or_else(|| RuntimeError::conflict("tool call is already resolved"))?;

        let previously_prepared = call.has_been_prepared;
        call.has_been_prepared = true;

        Ok(previously_prepared)
    }

    fn append_tool_result(
        &mut self,
        agent_id: AgentId,
        result: &ToolResult,
    ) -> Result<(), RuntimeError> {
        let agent = self
            .agents
            .get_mut(&agent_id)
            .ok_or_else(|| RuntimeError::conflict("agent does not exist"))?;

        if !agent.tool_call_ids.contains(&result.call_id) {
            return Err(RuntimeError::conflict("tool call does not exist"));
        }

        if !agent.unresolved_tool_calls.contains_key(&result.call_id) {
            return Err(RuntimeError::conflict("tool call is already resolved"));
        }

        agent.append_tool_result(result);

        Ok(())
    }

    fn handle_request(
        &mut self,
        scheduler: &Scheduler,
        repartition_probability: Probability,
        idempotency_key: &str,
        request: ClientRequest,
    ) -> Result<(), ClientError> {
        if let Some(previous_request) = self.idempotency_keys.get(idempotency_key) {
            if *previous_request != request {
                return Err(ClientError::idempotency_conflict(
                    "reused idempotency key with different arguments",
                ));
            }

            return Ok(());
        }

        match &request {
            ClientRequest::CreateAgent { agent, message } => {
                self.create_agent(agent, message.as_ref())?;
            }
            ClientRequest::InterruptAgent { agent_id, message } => {
                self.interrupt_agent(*agent_id, message.as_ref())?;
                self.repartition_agent(scheduler, *agent_id, repartition_probability);
            }
            ClientRequest::SendMessage { agent_id, message } => {
                self.send_message(*agent_id, message)?;
                self.repartition_agent(scheduler, *agent_id, repartition_probability);
            }
        }

        self.idempotency_keys
            .insert(idempotency_key.to_owned(), request);

        Ok(())
    }

    fn member_partitions(&self) -> impl Iterator<Item = PartitionId> + Clone {
        self.partitions
            .iter()
            .filter(|(_, partition)| partition.is_member)
            .map(|(id, _)| *id)
    }

    fn enqueue_wake(&mut self, owner: PartitionId, wake: Wake) {
        let partition = self.partitions.get_mut(&owner).expect("partition exists");

        for subscription in partition.subscriptions.values_mut() {
            subscription.pending_wakes.push_back(wake);

            if let Some(waker) = subscription.waker.take() {
                self.ready_wakers.push(waker);
            }
        }
    }

    fn repartition_agent(
        &mut self,
        scheduler: &Scheduler,
        agent_id: AgentId,
        probability: Probability,
    ) {
        let owner = self.agents.get(&agent_id).expect("agent exists").wake_owner;

        let owner = {
            let mut candidates = self.member_partitions().filter(|id| Some(*id) != owner);
            let candidate_count = candidates.clone().count();

            if candidate_count == 0 {
                return;
            }

            scheduler
                .sample_fault_position(probability, 0..candidate_count)
                .map(|index| candidates.nth(index).expect("selected partition exists"))
        };

        if let Some(owner) = owner {
            self.move_agent(agent_id, Some(owner));
        }
    }

    fn move_agent(&mut self, agent_id: AgentId, owner: Option<PartitionId>) {
        let agent = self.agents.get_mut(&agent_id).expect("agent exists");
        let previous = agent.wake_owner;

        if previous == owner {
            return;
        }

        agent.wake_owner = owner;

        let initial_wake = agent.initial_wake();

        if let Some(previous) = previous {
            let partition = self
                .partitions
                .get_mut(&previous)
                .expect("partition exists");
            partition.released_agents.insert(agent_id);

            for subscription in partition.subscriptions.values_mut() {
                subscription.pending_wakes.retain(|wake| {
                    !matches!(wake, Wake::Reconcile(id) | Wake::Interrupt(id) if *id == agent_id)
                });
            }

            self.enqueue_wake(previous, Wake::Release(agent_id));
        }

        if let Some(owner) = owner
            && let Some(wake) = initial_wake
        {
            self.enqueue_wake(owner, wake);
        }
    }

    fn rebalance_wakes(&mut self) {
        let assignments = {
            let mut owners = self.member_partitions().cycle();

            self.agents
                .keys()
                .map(|id| (*id, owners.next()))
                .collect::<Vec<_>>()
        };

        for (agent_id, owner) in assignments {
            self.move_agent(agent_id, owner);
        }
    }

    const fn allocate_subscription_id(&mut self) -> u64 {
        let id = self.next_subscription_id;
        self.next_subscription_id = id.checked_add(1).expect("IDs never overflow");
        id
    }
}

impl AgentRecord {
    const fn new(agent: Agent) -> Self {
        Self {
            agent,
            wake_owner: None,
            events: Vec::new(),
            pending_messages: Vec::new(),
            parked_at: None,
            interrupt_intent: false,
            message_ids: BTreeSet::new(),
            tool_call_ids: BTreeSet::new(),
            unresolved_tool_calls: BTreeMap::new(),
            response_stream: None,
        }
    }

    fn state(&self) -> AgentState {
        AgentState {
            latest_event: self.latest_event(),
            parked_at: self.parked_at,
            interrupt_intent: self.interrupt_intent,
        }
    }

    fn latest_event(&self) -> Option<EventCursor> {
        self.events.last().map(|event| event.cursor)
    }

    fn check_latest_event(&self, expected: Option<EventCursor>) -> Result<(), RuntimeError> {
        if self.latest_event() != expected {
            return Err(RuntimeError::conflict("latest event does not match"));
        }

        Ok(())
    }

    fn is_idle(&self) -> bool {
        self.parked_at == self.latest_event()
            && self.pending_messages.is_empty()
            && !self.interrupt_intent
            && self.unresolved_tool_calls.is_empty()
    }

    fn initial_wake(&self) -> Option<Wake> {
        if self.is_idle() {
            None
        } else if self.interrupt_intent {
            Some(Wake::Interrupt(self.agent.id))
        } else {
            Some(Wake::Reconcile(self.agent.id))
        }
    }

    fn push_pending_message(&mut self, message: Message) {
        assert!(self.message_ids.insert(message.id), "message ID is new");
        self.pending_messages.push(message);
    }

    fn append_event(&mut self, payload: EventPayload) {
        let cursor =
            EventCursor::from(u128::try_from(self.events.len()).expect("event count fits u128"));

        self.events.push(Event { cursor, payload });
    }

    fn append_tool_result(&mut self, result: &ToolResult) {
        self.unresolved_tool_calls
            .remove(&result.call_id)
            .expect("tool call is unresolved");

        self.append_event(EventPayload::ToolResult(result.clone()));
    }
}

impl<'store, 'sim> WakeSubscription<'store, 'sim> {
    fn new(simulator: &'store StoreSimulator<'sim>, partition: PartitionId) -> Self {
        let id = simulator.mutate(|state| {
            let id = state.allocate_subscription_id();
            let initial_wakes = state
                .agents
                .values()
                .filter(|agent| agent.wake_owner == Some(partition))
                .filter_map(AgentRecord::initial_wake);
            let partition = state
                .partitions
                .get_mut(&partition)
                .expect("partition exists");
            let pending_wakes = partition
                .released_agents
                .iter()
                .copied()
                .map(Wake::Release)
                .chain(initial_wakes)
                .collect();

            partition.subscriptions.insert(
                id,
                WakeSubscriptionState {
                    waker: None,
                    pending_wakes,
                },
            );

            id
        });

        Self {
            simulator,
            partition,
            id,
            is_idle: false,
        }
    }

    async fn next(&mut self) -> Wake {
        #[expect(clippy::significant_drop_tightening, reason = "false positive")]
        future::poll_fn(|cx| {
            let mut state = self.simulator.state.lock().expect("never poisoned");

            let subscription = state
                .partitions
                .get_mut(&self.partition)
                .expect("partition exists")
                .subscriptions
                .get_mut(&self.id)
                .expect("subscription exists");

            if let Some(wake) = subscription.pending_wakes.pop_front() {
                self.is_idle = false;
                return Poll::Ready(wake);
            }

            if !self.is_idle {
                self.is_idle = true;

                return Poll::Ready(Wake::Idle);
            }

            subscription.waker = Some(cx.waker().clone());

            Poll::Pending
        })
        .await
    }
}

impl Drop for WakeSubscription<'_, '_> {
    fn drop(&mut self) {
        self.simulator.mutate(|state| {
            state
                .partitions
                .get_mut(&self.partition)
                .expect("partition exists")
                .subscriptions
                .remove(&self.id);
        });
    }
}

impl<K> StoreSimulatorError<K> {
    const fn random() -> Self {
        Self {
            inner: StoreSimulatorErrorInner::Random(RandomError),
        }
    }
}

impl StoreSimulatorError<RuntimeStoreErrorKind> {
    const fn conflict(message: &'static str) -> Self {
        Self {
            inner: StoreSimulatorErrorInner::Kind(RuntimeStoreErrorKind::Conflict, message),
        }
    }

    const fn other(message: &'static str) -> Self {
        Self {
            inner: StoreSimulatorErrorInner::Kind(RuntimeStoreErrorKind::Other, message),
        }
    }
}

impl StoreSimulatorError<ClientStoreErrorKind> {
    const fn conflict(message: &'static str) -> Self {
        Self {
            inner: StoreSimulatorErrorInner::Kind(ClientStoreErrorKind::Conflict, message),
        }
    }

    const fn idempotency_conflict(message: &'static str) -> Self {
        Self {
            inner: StoreSimulatorErrorInner::Kind(
                ClientStoreErrorKind::IdempotencyConflict,
                message,
            ),
        }
    }

    const fn agent_not_found(message: &'static str) -> Self {
        Self {
            inner: StoreSimulatorErrorInner::Kind(ClientStoreErrorKind::AgentNotFound, message),
        }
    }
}

impl<K> fmt::Display for StoreSimulatorError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner {
            StoreSimulatorErrorInner::Random(_) => f.write_str("store simulator error"),
            StoreSimulatorErrorInner::Kind(_, message) => {
                write!(f, "store simulator error: {message}")
            }
        }
    }
}

impl<K> core::error::Error for StoreSimulatorError<K>
where
    K: fmt::Debug,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.inner {
            StoreSimulatorErrorInner::Random(error) => Some(error),
            StoreSimulatorErrorInner::Kind(..) => None,
        }
    }
}

impl fleet_core::Error<RuntimeStoreErrorKind> for StoreSimulatorError<RuntimeStoreErrorKind> {
    fn kind(&self) -> RuntimeStoreErrorKind {
        match self.inner {
            StoreSimulatorErrorInner::Random(_) => RuntimeStoreErrorKind::Other,
            StoreSimulatorErrorInner::Kind(kind, _) => kind,
        }
    }
}

impl fleet_core::Error<ClientStoreErrorKind> for StoreSimulatorError<ClientStoreErrorKind> {
    fn kind(&self) -> ClientStoreErrorKind {
        match self.inner {
            StoreSimulatorErrorInner::Random(_) => ClientStoreErrorKind::Other,
            StoreSimulatorErrorInner::Kind(kind, _) => kind,
        }
    }
}

impl StoreSimulatorConfig {
    fn generate(rng: &mut Rng) -> Self {
        Self {
            agent: WrapConfig::generate(rng),
            agent_state: WrapConfig::generate(rng),
            settle_interrupt: WrapConfig::generate(rng),
            park: WrapConfig::generate(rng),
            consume_messages: WrapConfig::generate(rng),
            publish_model_response_chunk: WrapConfig::generate(rng),
            append_model_response: WrapConfig::generate(rng),
            append_compaction: WrapConfig::generate(rng),
            prepare_tool_call: WrapConfig::generate(rng),
            append_tool_result: WrapConfig::generate(rng),
            create_agent: WrapConfig::generate(rng),
            interrupt_agent: WrapConfig::generate(rng),
            send_message: WrapConfig::generate(rng),
            wakes: TrySuspendConfig::generate(rng),
            events_suspend: Probability::generate(rng),
            events_error: Probability::generate_for_faults(rng),
            wakes_repartition: Probability::generate_for_faults(rng),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        pin::{Pin, pin},
        sync::atomic::{AtomicBool, Ordering},
        task::Context,
    };
    use fleet_core::{
        Configured, ContentBlock, Error as _, JsonObject, ModelCompaction, ModelInput,
        ModelProvenance, ModelUsage, ToolCall, ToolOutput,
    };
    use futures_util::{
        FutureExt as _,
        task::{ArcWake, waker},
    };
    use std::sync::Arc;

    struct WakeFlag(AtomicBool);

    impl ArcWake for WakeFlag {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.store(true, Ordering::Relaxed);
        }
    }

    fn store(scheduler: &Scheduler) -> StoreSimulator<'_> {
        let mut store = StoreSimulator::new(scheduler, 0);
        store.config.wakes = TrySuspendConfig {
            suspend: Probability::NEVER,
            error: Probability::NEVER,
        };
        store.config.wakes_repartition = Probability::NEVER;

        store
    }

    fn agent(id: u128) -> Agent {
        Agent {
            id: id.into(),
            role: Configured {
                name: "test".into(),
                config: JsonObject::new(),
            },
            extensions: JsonObject::new(),
        }
    }

    fn message(id: u128) -> Message {
        Message {
            id: id.into(),
            mode: MessageMode::Steer,
            content: Vec::new(),
        }
    }

    fn response(calls: &[(&str, bool)]) -> ModelResponse {
        ModelResponse {
            output: calls
                .iter()
                .map(|(id, is_async)| {
                    ModelOutput::ToolCall(ToolCall {
                        id: (*id).into(),
                        tool: "test".into(),
                        is_async: *is_async,
                        input: JsonObject::new(),
                    })
                })
                .collect(),
            usage: ModelUsage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            },
            provenance: ModelProvenance {
                provider_id: "test".into(),
                model_id: "test".into(),
            },
            continuation_token: None,
        }
    }

    fn tool_result(id: &str) -> ToolResult {
        ToolResult {
            call_id: id.into(),
            output: ToolOutput {
                content: Vec::new(),
                structured_content: None,
                is_error: false,
            },
        }
    }

    fn compaction(last_event: u128, calls: &[(&str, bool)]) -> Compaction {
        let response = response(calls);
        let pending_tool_calls = response
            .output
            .iter()
            .filter_map(|output| match output {
                ModelOutput::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect();

        Compaction {
            last_event: last_event.into(),
            input: vec![
                ModelInput::User(vec![ContentBlock::Text("retained input".into())]),
                ModelInput::Compaction(
                    response.provenance.clone(),
                    ModelCompaction {
                        content: None,
                        redacted_content: Some("opaque state".into()),
                        attestation: Some("id".into()),
                    },
                ),
                ModelInput::Assistant(response.provenance, response.output),
            ],
            pending_tool_calls,
            usage: response.usage,
        }
    }

    fn populated_store(scheduler: &Scheduler) -> StoreSimulator<'_> {
        let store = store(scheduler);
        let agent = agent(0);

        store
            .mutate(|state| state.create_agent(&agent, Some(&message(0))))
            .unwrap();

        store
            .mutate(|state| state.consume_messages(agent.id, None, true))
            .unwrap();

        store
            .mutate(|state| {
                state.append_model_response(
                    agent.id,
                    0.into(),
                    false,
                    &response(&[("a", true), ("b", true)]),
                )
            })
            .unwrap();

        store
            .mutate(|state| state.prepare_tool_call(agent.id, &"a".into()))
            .unwrap();

        store
            .mutate(|state| state.park(agent.id, Some(1.into())))
            .unwrap();

        store
            .mutate(|state| state.send_message(agent.id, &message(1)))
            .unwrap();

        store
            .mutate(|state| state.send_message(agent.id, &message(2)))
            .unwrap();

        store
    }

    #[test]
    fn snapshot_preserves_agent_state_and_order() {
        let scheduler = Scheduler::new(0);
        let store = populated_store(&scheduler);

        let other_agent = agent(7);

        let queued = Message {
            id: 3.into(),
            mode: MessageMode::Queue,
            content: vec![ContentBlock::Text("queued".into())],
        };

        store
            .mutate(|state| state.create_agent(&other_agent, Some(&message(9))))
            .unwrap();

        store
            .mutate(|state| state.consume_messages(other_agent.id, None, true))
            .unwrap();

        store
            .mutate(|state| {
                state.append_model_response(
                    other_agent.id,
                    0.into(),
                    false,
                    &response(&[("a", false)]),
                )
            })
            .unwrap();

        store
            .mutate(|state| state.send_message(0.into(), &message(4)))
            .unwrap();

        store
            .mutate(|state| state.interrupt_agent(0.into(), Some(&queued)))
            .unwrap();

        let snapshot = store.snapshot();

        assert_eq!(
            snapshot.keys().copied().collect::<Vec<_>>(),
            [0.into(), 7.into()]
        );

        assert_eq!(
            snapshot[&0.into()],
            AgentSnapshot {
                agent: agent(0),
                events: vec![
                    Event {
                        cursor: 0.into(),
                        payload: EventPayload::Input(0.into(), Vec::new()),
                    },
                    Event {
                        cursor: 1.into(),
                        payload: EventPayload::ModelResponse(response(&[("a", true), ("b", true)])),
                    },
                ],
                pending_messages: vec![message(1), message(2), message(4), queued],
                unresolved_tool_calls: BTreeMap::from([
                    (
                        "a".into(),
                        ToolCallSnapshot {
                            has_been_prepared: true,
                            is_async: true,
                        },
                    ),
                    (
                        "b".into(),
                        ToolCallSnapshot {
                            has_been_prepared: false,
                            is_async: true,
                        },
                    ),
                ]),
                parked_at: Some(1.into()),
                interrupt_intent: true,
            }
        );

        assert_eq!(
            snapshot[&other_agent.id],
            AgentSnapshot {
                agent: other_agent.clone(),
                events: vec![
                    Event {
                        cursor: 0.into(),
                        payload: EventPayload::Input(9.into(), Vec::new()),
                    },
                    Event {
                        cursor: 1.into(),
                        payload: EventPayload::ModelResponse(response(&[("a", false)])),
                    },
                ],
                pending_messages: Vec::new(),
                unresolved_tool_calls: BTreeMap::from([(
                    "a".into(),
                    ToolCallSnapshot {
                        has_been_prepared: false,
                        is_async: false,
                    },
                )]),
                parked_at: None,
                interrupt_intent: false,
            }
        );
    }

    #[test]
    fn snapshots_are_independent_of_later_mutations() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);

        assert!(store.snapshot().is_empty());

        store
            .mutate(|state| state.create_agent(&agent(0), Some(&message(5))))
            .unwrap();

        let mut before = store.snapshot();

        store
            .mutate(|state| state.consume_messages(0.into(), None, true))
            .unwrap();

        let after = store.snapshot();

        assert_eq!(before[&0.into()].events, []);
        assert_eq!(before[&0.into()].pending_messages, [message(5)]);
        assert_eq!(after[&0.into()].pending_messages, []);
        assert_eq!(after[&0.into()].events.len(), 1);

        let copied = before.get_mut(&0.into()).unwrap();
        copied.agent.role.name = "changed".into();
        copied.pending_messages.clear();
        copied.interrupt_intent = true;

        assert_eq!(store.snapshot(), after);
    }

    #[test]
    fn compaction_preserves_history_tool_state_and_parking() {
        let scheduler = Scheduler::new(0);
        let store = populated_store(&scheduler);
        let before = store.snapshot();
        let compaction = compaction(1, &[("a", true), ("b", true)]);

        store
            .mutate(|state| state.append_compaction(0.into(), 1.into(), &compaction))
            .unwrap();

        let after = store.snapshot();
        let before = &before[&0.into()];
        let after = &after[&0.into()];
        assert_eq!(
            &after.events[..before.events.len()],
            before.events.as_slice()
        );
        assert_eq!(
            after.events.last().unwrap().payload,
            EventPayload::Compaction(compaction)
        );
        assert_eq!(after.events.len(), before.events.len() + 1);
        assert_eq!(after.pending_messages, before.pending_messages);
        assert_eq!(after.unresolved_tool_calls, before.unresolved_tool_calls);
        assert_eq!(after.parked_at, Some(2.into()));
        assert!(
            store
                .mutate(|state| state.prepare_tool_call(0.into(), &"a".into()))
                .unwrap()
        );
        assert!(
            !store
                .mutate(|state| state.prepare_tool_call(0.into(), &"b".into()))
                .unwrap()
        );
    }

    #[test]
    fn compaction_checkpoint_precedes_later_tool_activity() {
        for is_async in [false, true] {
            let scheduler = Scheduler::new(0);
            let store = populated_store(&scheduler);
            let compaction = compaction(1, &[("a", true), ("b", true)]);
            store
                .mutate(|state| state.append_tool_result(0.into(), &tool_result("a")))
                .unwrap();
            store
                .mutate(|state| {
                    state.append_model_response(
                        0.into(),
                        2.into(),
                        false,
                        &response(&[("c", is_async)]),
                    )
                })
                .unwrap();
            let before = store.snapshot();

            store
                .mutate(|state| state.append_compaction(0.into(), 3.into(), &compaction))
                .unwrap();

            let after = store.snapshot();
            let after = &after[&0.into()];
            assert_eq!(
                after.unresolved_tool_calls,
                before[&0.into()].unresolved_tool_calls
            );
            assert!(!after.unresolved_tool_calls.contains_key(&"a".into()));
            assert!(after.unresolved_tool_calls.contains_key(&"b".into()));
            assert!(after.unresolved_tool_calls.contains_key(&"c".into()));
            assert_eq!(
                after.events.last().unwrap().payload,
                EventPayload::Compaction(compaction)
            );
            assert_eq!(after.parked_at, Some(1.into()));

            store
                .mutate(|state| state.append_tool_result(0.into(), &tool_result("c")))
                .unwrap();
            assert!(
                !store.snapshot()[&0.into()]
                    .unresolved_tool_calls
                    .contains_key(&"c".into())
            );
        }
    }

    #[test]
    fn compaction_can_replace_an_existing_checkpoint() {
        for different_provenance in [false, true] {
            let scheduler = Scheduler::new(0);
            let store = populated_store(&scheduler);
            let mut compaction = compaction(1, &[("a", true), ("b", true)]);
            store
                .mutate(|state| state.append_compaction(0.into(), 1.into(), &compaction))
                .unwrap();
            let before = store.snapshot();

            let ModelInput::Compaction(provenance, content) = &mut compaction.input[1] else {
                unreachable!();
            };
            content.redacted_content = Some("replacement state".into());
            if different_provenance {
                provenance.provider_id = "other".into();
            }

            store
                .mutate(|state| state.append_compaction(0.into(), 2.into(), &compaction))
                .unwrap();

            let after = store.snapshot();
            let before = &before[&0.into()];
            let after = &after[&0.into()];
            assert_eq!(
                &after.events[..before.events.len()],
                before.events.as_slice()
            );
            assert_eq!(after.events.len(), before.events.len() + 1);
            assert_eq!(
                after.events.last().unwrap().payload,
                EventPayload::Compaction(compaction)
            );
            assert_eq!(after.unresolved_tool_calls, before.unresolved_tool_calls);
            assert_eq!(after.parked_at, Some(3.into()));
        }
    }

    #[test]
    fn invalid_compaction_checkpoints_leave_state_unchanged() {
        let scheduler = Scheduler::new(0);
        let store = populated_store(&scheduler);
        let valid = compaction(1, &[("a", true), ("b", true)]);
        let mut altered = valid.clone();
        altered.pending_tool_calls[0]
            .input
            .insert("changed".into(), true.into());
        let before = store.snapshot();

        let cases = [
            (1, 1, valid.clone()),
            (0, 0, valid.clone()),
            (0, 1, compaction(99, &[("a", true), ("b", true)])),
            (0, 1, compaction(1, &[("a", true)])),
            (
                0,
                1,
                compaction(1, &[("a", true), ("a", true), ("b", true)]),
            ),
            (0, 1, altered),
        ];

        for (agent, latest, compaction) in cases {
            assert!(
                store
                    .mutate(|state| state.append_compaction(
                        agent.into(),
                        latest.into(),
                        &compaction
                    ))
                    .is_err()
            );
            assert_eq!(store.snapshot(), before);
        }

        store
            .mutate(|state| state.append_compaction(0.into(), 1.into(), &valid))
            .unwrap();
        let before = store.snapshot();
        assert!(
            store
                .mutate(|state| state.append_compaction(0.into(), 1.into(), &valid))
                .is_err()
        );
        assert_eq!(store.snapshot(), before);

        store
            .mutate(|state| {
                state.append_compaction(
                    0.into(),
                    2.into(),
                    &compaction(2, &[("a", true), ("b", true)]),
                )
            })
            .unwrap();
    }

    #[test]
    fn compaction_rejects_unresolved_synchronous_calls_at_the_checkpoint() {
        let scheduler = Scheduler::new(0);
        let store = populated_store(&scheduler);
        store
            .mutate(|state| state.append_tool_result(0.into(), &tool_result("a")))
            .unwrap();
        store
            .mutate(|state| state.append_tool_result(0.into(), &tool_result("b")))
            .unwrap();
        store
            .mutate(|state| {
                state.append_model_response(
                    0.into(),
                    3.into(),
                    false,
                    &response(&[("sync", false)]),
                )
            })
            .unwrap();
        let before = store.snapshot();

        assert!(
            store
                .mutate(|state| state.append_compaction(
                    0.into(),
                    4.into(),
                    &compaction(4, &[("sync", false)])
                ))
                .is_err()
        );
        assert_eq!(store.snapshot(), before);

        store
            .mutate(|state| state.append_tool_result(0.into(), &tool_result("sync")))
            .unwrap();
        let before = store.snapshot();
        assert!(
            store
                .mutate(|state| state.append_compaction(
                    0.into(),
                    5.into(),
                    &compaction(4, &[("sync", false)])
                ))
                .is_err()
        );
        assert_eq!(store.snapshot(), before);
    }

    #[test]
    fn rejecting_a_missing_agent_preserves_state() {
        let scheduler = Scheduler::new(0);
        let store = populated_store(&scheduler);
        let previous = store.snapshot();

        assert!(store.mutate(|state| state.park(1.into(), None)).is_err());

        assert!(
            store
                .mutate(|state| state.handle_request(
                    &scheduler,
                    Probability::NEVER,
                    "missing",
                    ClientRequest::SendMessage {
                        agent_id: 1.into(),
                        message: message(0),
                    }
                ))
                .is_err()
        );

        assert_eq!(store.snapshot(), previous);
        assert!(store.state.lock().unwrap().idempotency_keys.is_empty());
    }

    #[test]
    fn tool_and_interrupt_lifecycle() {
        let scheduler = Scheduler::new(0);
        let store = populated_store(&scheduler);
        let id = AgentId::from(0);

        assert!(
            store
                .mutate(|state| state.prepare_tool_call(id, &"a".into()))
                .unwrap()
        );

        store
            .mutate(|state| state.append_tool_result(id, &tool_result("a")))
            .unwrap();

        store
            .mutate(|state| {
                state.append_model_response(id, 2.into(), false, &response(&[("sync", false)]))
            })
            .unwrap();

        assert!(
            store
                .mutate(|state| state.append_model_response(id, 3.into(), true, &response(&[])))
                .is_err()
        );

        store
            .mutate(|state| state.interrupt_agent(id, Some(&message(3))))
            .unwrap();

        assert!(
            store
                .mutate(|state| state.consume_messages(id, Some(3.into()), true))
                .is_err()
        );

        store
            .mutate(|state| state.park(id, Some(3.into())))
            .unwrap();

        store
            .mutate(|state| {
                state.settle_interrupt(id, Some(3.into()), &[tool_result("b"), tool_result("sync")])
            })
            .unwrap();

        assert!(!store.state.lock().unwrap().agents[&id].is_idle());

        store
            .mutate(|state| state.consume_messages(id, Some(6.into()), true))
            .unwrap();

        store
            .mutate(|state| state.park(id, Some(9.into())))
            .unwrap();

        assert!(store.state.lock().unwrap().agents[&id].is_idle());

        assert!(
            store
                .mutate(|state| state.append_tool_result(id, &tool_result("a")))
                .is_err()
        );
    }

    #[test]
    fn message_consumption_prioritizes_steering_and_preserves_queue_order() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);

        store
            .mutate(|state| state.create_agent(&agent(0), None))
            .unwrap();

        for (id, mode) in [
            (0, MessageMode::Queue),
            (1, MessageMode::Steer),
            (2, MessageMode::Queue),
            (3, MessageMode::Steer),
        ] {
            let message = Message {
                id: id.into(),
                mode,
                content: vec![ContentBlock::Text(id.to_string())],
            };

            store
                .mutate(|state| state.send_message(0.into(), &message))
                .unwrap();
        }

        for (cursor, allow_queued, expected_consumed, pending_ids) in [
            (None, true, true, vec![0, 2]),
            (Some(1), false, false, vec![0, 2]),
            (Some(1), true, true, vec![2]),
            (Some(2), true, true, vec![]),
        ] {
            assert_eq!(
                store
                    .mutate(|state| state.consume_messages(
                        0.into(),
                        cursor.map(EventCursor::from),
                        allow_queued
                    ))
                    .unwrap(),
                expected_consumed
            );

            assert_eq!(
                store.state.lock().unwrap().agents[&0.into()]
                    .pending_messages
                    .iter()
                    .map(|message| message.id)
                    .collect::<Vec<_>>(),
                pending_ids
                    .into_iter()
                    .map(MessageId::from)
                    .collect::<Vec<_>>()
            );
        }

        let state = store.state.lock().unwrap();
        let events = &state.agents[&0.into()].events;

        assert_eq!(
            events
                .iter()
                .map(|event| &event.payload)
                .collect::<Vec<_>>(),
            [1, 3, 0, 2]
                .map(|id: u128| EventPayload::Input(
                    id.into(),
                    vec![ContentBlock::Text(id.to_string())]
                ))
                .iter()
                .collect::<Vec<_>>()
        );

        drop(state);
    }

    #[test]
    fn create_agent_preserves_extensions_and_checks_retries() {
        let scheduler = Scheduler::new(0);
        scheduler.disable_faults();
        let store = store(&scheduler);
        let partition = store.add_partition();
        let mut agent = agent(0);
        agent.extensions = JsonObject::from_iter([(
            "app".into(),
            JsonObject::from_iter([("title".into(), "Example".into())]).into(),
        )]);

        scheduler.run(async {
            store.create_agent("create", &agent, None).await.unwrap();
            store.create_agent("create", &agent, None).await.unwrap();

            let saved = store
                .partition(partition)
                .agent(agent.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved, agent);

            agent.extensions.clear();

            let error = store
                .create_agent("create", &agent, None)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ClientStoreErrorKind::IdempotencyConflict);

            let loaded = store
                .partition(partition)
                .agent(agent.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(loaded, saved);
        });
    }

    #[test]
    fn idempotency_records_survive_retries_and_reject_conflicts() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let request = ClientRequest::CreateAgent {
            agent: agent(0),
            message: Some(message(0)),
        };

        store
            .mutate(|state| {
                state.handle_request(&scheduler, Probability::NEVER, "create", request.clone())
            })
            .unwrap();

        store
            .mutate(|state| state.consume_messages(0.into(), None, true))
            .unwrap();

        let previous = store.snapshot();

        store
            .mutate(|state| {
                state.handle_request(&scheduler, Probability::NEVER, "create", request.clone())
            })
            .unwrap();

        assert_eq!(store.snapshot(), previous);

        assert!(
            store
                .mutate(|state| state.handle_request(
                    &scheduler,
                    Probability::NEVER,
                    "create",
                    ClientRequest::SendMessage {
                        agent_id: 0.into(),
                        message: message(1),
                    }
                ))
                .is_err()
        );

        assert!(
            store
                .mutate(|state| {
                    state.handle_request(
                        &scheduler,
                        Probability::NEVER,
                        "duplicate",
                        request.clone(),
                    )
                })
                .is_err()
        );

        assert_eq!(store.snapshot(), previous);
        assert_eq!(
            store.state.lock().unwrap().idempotency_keys,
            BTreeMap::from([("create".into(), request)])
        );
    }

    #[test]
    fn injected_errors_after_commit_do_not_imply_rollback() {
        let scheduler = Scheduler::new(0);
        let mut store = store(&scheduler);
        store.config.create_agent = WrapConfig {
            suspend_before: Probability::NEVER,
            suspend_after: Probability::NEVER,
            error: Probability::ALWAYS,
            error_after: Probability::ALWAYS,
        };

        let agent = agent(0);
        let mut create = pin!(store.create_agent("create", &agent, None));

        assert!(matches!(
            create
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(_))
        ));

        assert!(store.state.lock().unwrap().agents.contains_key(&agent.id));
    }

    fn poll_wake(
        wakes: Pin<&mut impl Stream<Item = Result<Wake, RuntimeError>>>,
    ) -> Poll<Option<Result<Wake, RuntimeError>>> {
        wakes.poll_next(&mut Context::from_waker(Waker::noop()))
    }

    fn assert_wakes(
        mut wakes: Pin<&mut impl Stream<Item = Result<Wake, RuntimeError>>>,
        expected: &[Wake],
    ) {
        for wake in expected {
            assert_eq!(poll_wake(wakes.as_mut()), Poll::Ready(Some(Ok(*wake))));
        }

        assert_eq!(poll_wake(wakes.as_mut()), Poll::Pending);
    }

    fn connect<'store>(
        store: &'store StoreSimulator<'_>,
        partition: PartitionId,
    ) -> Pin<Box<impl Stream<Item = Result<Wake, RuntimeError>> + 'store>> {
        Box::pin(store.partition_wakes(partition))
    }

    fn next_wake(subscription: &mut WakeSubscription<'_, '_>) -> Poll<Wake> {
        pin!(subscription.next()).poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn partitions_split_existing_and_new_agents() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);

        for id in 0..3 {
            store
                .mutate(|state| state.create_agent(&agent(id), None))
                .unwrap();
        }

        store
            .mutate(|state| state.send_message(0.into(), &message(0)))
            .unwrap();
        store
            .mutate(|state| state.interrupt_agent(1.into(), None))
            .unwrap();

        let first = store.add_partition();
        let mut first_wakes = connect(&store, first);

        assert_wakes(
            first_wakes.as_mut(),
            &[
                Wake::Reconcile(0.into()),
                Wake::Interrupt(1.into()),
                Wake::Idle,
            ],
        );

        let second = store.add_partition();
        let mut second_wakes = connect(&store, second);

        assert_wakes(
            second_wakes.as_mut(),
            &[Wake::Interrupt(1.into()), Wake::Idle],
        );
        assert_wakes(first_wakes.as_mut(), &[Wake::Release(1.into()), Wake::Idle]);

        store
            .mutate(|state| state.send_message(2.into(), &message(0)))
            .unwrap();
        store
            .mutate(|state| state.create_agent(&agent(3), Some(&message(0))))
            .unwrap();
        store
            .mutate(|state| state.create_agent(&agent(4), Some(&message(0))))
            .unwrap();

        assert_wakes(
            first_wakes.as_mut(),
            &[
                Wake::Reconcile(2.into()),
                Wake::Reconcile(4.into()),
                Wake::Idle,
            ],
        );
        assert_wakes(
            second_wakes.as_mut(),
            &[Wake::Reconcile(3.into()), Wake::Idle],
        );
    }

    #[test]
    fn handoff_removes_queued_work() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let first = store.add_partition();
        let second = store.add_partition();
        let mut first_wakes = connect(&store, first);
        let mut duplicate_wakes = connect(&store, first);
        let mut second_wakes = connect(&store, second);

        assert_wakes(first_wakes.as_mut(), &[Wake::Idle]);
        assert_wakes(duplicate_wakes.as_mut(), &[Wake::Idle]);
        assert_wakes(second_wakes.as_mut(), &[Wake::Idle]);

        store
            .mutate(|state| state.create_agent(&agent(0), Some(&message(0))))
            .unwrap();

        assert_wakes(
            first_wakes.as_mut(),
            &[Wake::Reconcile(0.into()), Wake::Idle],
        );

        store
            .mutate(|state| state.send_message(0.into(), &message(1)))
            .unwrap();
        store.mutate(|state| state.move_agent(0.into(), Some(second)));

        assert_wakes(
            second_wakes.as_mut(),
            &[Wake::Reconcile(0.into()), Wake::Idle],
        );
        assert_wakes(first_wakes.as_mut(), &[Wake::Release(0.into()), Wake::Idle]);
        assert_wakes(
            duplicate_wakes.as_mut(),
            &[Wake::Release(0.into()), Wake::Idle],
        );

        store
            .mutate(|state| state.create_agent(&agent(1), None))
            .unwrap();
        store
            .mutate(|state| state.create_agent(&agent(2), Some(&message(0))))
            .unwrap();

        assert_wakes(
            first_wakes.as_mut(),
            &[Wake::Reconcile(2.into()), Wake::Idle],
        );
        assert_wakes(
            duplicate_wakes.as_mut(),
            &[Wake::Reconcile(2.into()), Wake::Idle],
        );
        assert_eq!(poll_wake(second_wakes.as_mut()), Poll::Pending);
    }

    #[test]
    fn rapid_handoffs_preserve_release_before_reacquisition() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let first = store.add_partition();
        let second = store.add_partition();
        let mut first_wakes = connect(&store, first);
        let mut second_wakes = connect(&store, second);

        assert_wakes(first_wakes.as_mut(), &[Wake::Idle]);
        assert_wakes(second_wakes.as_mut(), &[Wake::Idle]);

        store
            .mutate(|state| state.create_agent(&agent(0), Some(&message(0))))
            .unwrap();

        assert_wakes(
            first_wakes.as_mut(),
            &[Wake::Reconcile(0.into()), Wake::Idle],
        );

        store.mutate(|state| state.move_agent(0.into(), Some(second)));
        store.mutate(|state| state.move_agent(0.into(), Some(first)));

        assert_wakes(
            first_wakes.as_mut(),
            &[
                Wake::Release(0.into()),
                Wake::Reconcile(0.into()),
                Wake::Idle,
            ],
        );
        assert_wakes(
            second_wakes.as_mut(),
            &[Wake::Release(0.into()), Wake::Idle],
        );
    }

    #[test]
    fn removed_partition_hands_off_to_survivor_during_recovery() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let first = store.add_partition();
        let second = store.add_partition();
        let mut first_wakes = connect(&store, first);
        let mut second_wakes = connect(&store, second);

        assert_wakes(first_wakes.as_mut(), &[Wake::Idle]);
        assert_wakes(second_wakes.as_mut(), &[Wake::Idle]);

        store
            .mutate(|state| state.create_agent(&agent(0), Some(&message(0))))
            .unwrap();

        assert_wakes(
            first_wakes.as_mut(),
            &[Wake::Reconcile(0.into()), Wake::Idle],
        );

        let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let task_waker = waker(flag.clone());

        assert_eq!(
            second_wakes
                .as_mut()
                .poll_next(&mut Context::from_waker(&task_waker)),
            Poll::Pending
        );

        let before = store.snapshot();
        scheduler.begin_recovery();
        store.remove_partition(first);

        assert!(flag.0.load(Ordering::Relaxed));
        assert_wakes(
            second_wakes.as_mut(),
            &[Wake::Reconcile(0.into()), Wake::Idle],
        );
        assert_wakes(first_wakes.as_mut(), &[Wake::Release(0.into()), Wake::Idle]);

        store
            .mutate(|state| state.create_agent(&agent(1), Some(&message(0))))
            .unwrap();

        assert_wakes(
            second_wakes.as_mut(),
            &[Wake::Reconcile(1.into()), Wake::Idle],
        );
        assert_eq!(poll_wake(first_wakes.as_mut()), Poll::Pending);

        drop(second_wakes);

        let mut reconnected = connect(&store, second);

        assert_wakes(
            reconnected.as_mut(),
            &[
                Wake::Reconcile(0.into()),
                Wake::Reconcile(1.into()),
                Wake::Idle,
            ],
        );
        assert_eq!(store.snapshot()[&0.into()], before[&0.into()]);
    }

    #[test]
    fn failed_wake_stream_preserves_its_partition() {
        let scheduler = Scheduler::new(0);
        let mut store = store(&scheduler);
        store.config.wakes.error = Probability::ALWAYS;

        let first = store.add_partition();
        let second = store.add_partition();

        for id in 0..2 {
            store
                .mutate(|state| state.create_agent(&agent(id), Some(&message(0))))
                .unwrap();
        }

        let mut survivor = WakeSubscription::new(&store, first);
        let mut duplicate = WakeSubscription::new(&store, second);

        assert_eq!(
            next_wake(&mut survivor),
            Poll::Ready(Wake::Reconcile(0.into()))
        );

        let mut failing = connect(&store, second);

        let error = failing.next().now_or_never().unwrap().unwrap().unwrap_err();
        assert_eq!(error, RuntimeError::random());
        assert_eq!(poll_wake(failing.as_mut()), Poll::Ready(None));

        assert_eq!(next_wake(&mut survivor), Poll::Ready(Wake::Idle));
        assert_eq!(next_wake(&mut survivor), Poll::Pending);
        assert_eq!(
            next_wake(&mut duplicate),
            Poll::Ready(Wake::Reconcile(1.into()))
        );
        assert_eq!(next_wake(&mut duplicate), Poll::Ready(Wake::Idle));

        let mut reconnected = WakeSubscription::new(&store, second);

        assert_eq!(
            next_wake(&mut reconnected),
            Poll::Ready(Wake::Reconcile(1.into()))
        );
        assert_eq!(next_wake(&mut reconnected), Poll::Ready(Wake::Idle));

        store
            .mutate(|state| state.send_message(1.into(), &message(1)))
            .unwrap();

        assert_eq!(
            next_wake(&mut duplicate),
            Poll::Ready(Wake::Reconcile(1.into()))
        );
        assert_eq!(
            next_wake(&mut reconnected),
            Poll::Ready(Wake::Reconcile(1.into()))
        );
    }

    #[test]
    fn reconnecting_yields_releases_and_current_work() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let first = store.add_partition();
        let second = store.add_partition();

        store
            .mutate(|state| state.create_agent(&agent(0), Some(&message(0))))
            .unwrap();

        let mut first_wakes = connect(&store, first);

        assert_wakes(
            first_wakes.as_mut(),
            &[Wake::Reconcile(0.into()), Wake::Idle],
        );

        drop(first_wakes);
        store.mutate(|state| state.move_agent(0.into(), Some(second)));

        let mut first_wakes = connect(&store, first);
        let mut second_wakes = connect(&store, second);

        assert_wakes(first_wakes.as_mut(), &[Wake::Release(0.into()), Wake::Idle]);
        assert_wakes(
            second_wakes.as_mut(),
            &[Wake::Reconcile(0.into()), Wake::Idle],
        );

        drop(second_wakes);

        for id in 1..3 {
            store
                .mutate(|state| state.send_message(0.into(), &message(id)))
                .unwrap();
        }

        let mut second_wakes = connect(&store, second);

        assert_wakes(
            second_wakes.as_mut(),
            &[Wake::Reconcile(0.into()), Wake::Idle],
        );
    }

    #[test]
    fn subscribers_receive_initial_work_and_new_activity() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let partition = store.add_partition();
        let agent = agent(0);
        let first_flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let second_flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let first_waker = waker(first_flag.clone());
        let second_waker = waker(second_flag.clone());

        store
            .mutate(|state| state.create_agent(&agent, Some(&message(0))))
            .unwrap();

        let mut first = connect(&store, partition);
        let mut second = connect(&store, partition);

        assert_wakes(first.as_mut(), &[Wake::Reconcile(agent.id), Wake::Idle]);
        assert_wakes(second.as_mut(), &[Wake::Reconcile(agent.id), Wake::Idle]);
        assert_eq!(
            first
                .as_mut()
                .poll_next(&mut Context::from_waker(&first_waker)),
            Poll::Pending
        );
        assert_eq!(
            second
                .as_mut()
                .poll_next(&mut Context::from_waker(&second_waker)),
            Poll::Pending
        );

        store
            .mutate(|state| state.interrupt_agent(agent.id, None))
            .unwrap();

        assert!(first_flag.0.load(Ordering::Relaxed));
        assert!(second_flag.0.load(Ordering::Relaxed));
        assert_wakes(first.as_mut(), &[Wake::Interrupt(agent.id), Wake::Idle]);
        assert_wakes(second.as_mut(), &[Wake::Interrupt(agent.id), Wake::Idle]);

        drop(first);
        store
            .mutate(|state| state.send_message(agent.id, &message(1)))
            .unwrap();

        assert_wakes(second.as_mut(), &[Wake::Reconcile(agent.id), Wake::Idle]);
    }

    #[test]
    fn reconnecting_replays_releases_before_current_work() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let first = store.add_partition();
        let second = store.add_partition();
        let agent = agent(0);

        store
            .mutate(|state| state.create_agent(&agent, Some(&message(0))))
            .unwrap();

        let mut wakes = connect(&store, first);
        let mut duplicate = connect(&store, first);

        assert_wakes(wakes.as_mut(), &[Wake::Reconcile(agent.id), Wake::Idle]);
        assert_wakes(duplicate.as_mut(), &[Wake::Reconcile(agent.id), Wake::Idle]);

        drop(wakes);
        store.mutate(|state| state.move_agent(agent.id, Some(second)));
        store.mutate(|state| state.move_agent(agent.id, Some(first)));

        let expected = [
            Wake::Release(agent.id),
            Wake::Reconcile(agent.id),
            Wake::Idle,
        ];

        assert_wakes(duplicate.as_mut(), &expected);

        let mut reconnected = connect(&store, first);

        assert_wakes(reconnected.as_mut(), &expected);
        assert_eq!(poll_wake(duplicate.as_mut()), Poll::Pending);
    }

    #[test]
    #[should_panic(expected = "partition exists and has not been removed")]
    fn removing_a_partition_twice_panics() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let partition = store.add_partition();

        store.remove_partition(partition);
        store.remove_partition(partition);
    }

    #[test]
    fn client_repartitioning_respects_probability_retries_and_recovery() {
        for probability in [Probability::NEVER, Probability::ALWAYS] {
            for interrupt in [false, true] {
                let scheduler = Scheduler::new(0);
                let mut store = store(&scheduler);
                store.config.wakes_repartition = probability;
                let config = if interrupt {
                    &mut store.config.interrupt_agent
                } else {
                    &mut store.config.send_message
                };
                *config = WrapConfig {
                    suspend_before: Probability::NEVER,
                    suspend_after: Probability::NEVER,
                    error: Probability::NEVER,
                    error_after: Probability::NEVER,
                };

                let first = store.add_partition();
                let second = store.add_partition();
                let mut first_wakes = connect(&store, first);
                let mut second_wakes = connect(&store, second);

                assert_wakes(first_wakes.as_mut(), &[Wake::Idle]);
                assert_wakes(second_wakes.as_mut(), &[Wake::Idle]);

                store
                    .mutate(|state| state.create_agent(&agent(0), None))
                    .unwrap();

                let initial_message = message(0);
                let recovery_message = message(1);
                let apply = |key, message| {
                    if interrupt {
                        future::Either::Left(store.interrupt_agent(key, 0.into(), Some(message)))
                    } else {
                        future::Either::Right(store.send_message(key, 0.into(), message))
                    }
                };
                let wake = if interrupt {
                    Wake::Interrupt(0.into())
                } else {
                    Wake::Reconcile(0.into())
                };

                assert_eq!(
                    pin!(apply("first", &initial_message))
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Ok(())),
                );

                if probability == Probability::ALWAYS {
                    assert_wakes(second_wakes.as_mut(), &[wake, Wake::Idle]);
                    assert_wakes(first_wakes.as_mut(), &[Wake::Release(0.into()), Wake::Idle]);
                } else {
                    assert_wakes(first_wakes.as_mut(), &[wake, Wake::Idle]);
                    assert_eq!(poll_wake(second_wakes.as_mut()), Poll::Pending);
                }

                assert_eq!(
                    pin!(apply("first", &initial_message))
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Ok(())),
                );
                assert!(matches!(
                    pin!(apply("conflict", &initial_message))
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Err(_)),
                ));

                assert_eq!(poll_wake(first_wakes.as_mut()), Poll::Pending);
                assert_eq!(poll_wake(second_wakes.as_mut()), Poll::Pending);

                scheduler.begin_recovery();
                store.repartition();

                assert_eq!(
                    pin!(apply("recovery", &recovery_message))
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Ok(())),
                );

                if probability == Probability::ALWAYS {
                    assert_wakes(second_wakes.as_mut(), &[wake, Wake::Idle]);
                    assert_eq!(poll_wake(first_wakes.as_mut()), Poll::Pending);
                } else {
                    assert_wakes(first_wakes.as_mut(), &[wake, Wake::Idle]);
                    assert_eq!(poll_wake(second_wakes.as_mut()), Poll::Pending);
                }
            }
        }
    }

    #[test]
    fn explicit_repartition_moves_one_agent_and_preserves_store_contents() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);

        store.repartition();

        for id in 0..4 {
            store
                .mutate(|state| state.create_agent(&agent(id), Some(&message(0))))
                .unwrap();
        }

        store.repartition();

        let first = store.add_partition();
        let mut first_wakes = connect(&store, first);

        assert_wakes(
            first_wakes.as_mut(),
            &[
                Wake::Reconcile(0.into()),
                Wake::Reconcile(1.into()),
                Wake::Reconcile(2.into()),
                Wake::Reconcile(3.into()),
                Wake::Idle,
            ],
        );

        store.repartition();

        assert_eq!(poll_wake(first_wakes.as_mut()), Poll::Pending);

        let second = store.add_partition();
        let mut second_wakes = connect(&store, second);

        assert_wakes(
            second_wakes.as_mut(),
            &[
                Wake::Reconcile(1.into()),
                Wake::Reconcile(3.into()),
                Wake::Idle,
            ],
        );
        assert_wakes(
            first_wakes.as_mut(),
            &[Wake::Release(1.into()), Wake::Release(3.into()), Wake::Idle],
        );

        let owners = || {
            store
                .state
                .lock()
                .unwrap()
                .agents
                .values()
                .map(|agent| agent.wake_owner)
                .collect::<Vec<_>>()
        };
        let before_owners = owners();
        let before_contents = store.snapshot();

        store.repartition();

        assert_eq!(
            owners()
                .iter()
                .zip(&before_owners)
                .filter(|(a, b)| a != b)
                .count(),
            1
        );
        assert_eq!(store.snapshot(), before_contents);

        scheduler.begin_recovery();
        let recovered_owners = owners();

        store.repartition();

        assert_eq!(owners(), recovered_owners);
    }

    #[test]
    fn disabling_faults_preserves_queued_wakes() {
        for recover in [false, true] {
            let scheduler = Scheduler::new(0);
            let store = store(&scheduler);
            let partition = store.add_partition();
            let idle = agent(0);
            let first = agent(1);
            let second = agent(2);

            store
                .mutate(|state| state.create_agent(&idle, None))
                .unwrap();

            store
                .mutate(|state| state.create_agent(&first, Some(&message(0))))
                .unwrap();

            store
                .mutate(|state| state.create_agent(&second, Some(&message(0))))
                .unwrap();

            let mut wakes = connect(&store, partition);

            assert_eq!(
                poll_wake(wakes.as_mut()),
                Poll::Ready(Some(Ok(Wake::Reconcile(first.id))))
            );

            if recover {
                scheduler.begin_recovery();
            } else {
                scheduler.disable_faults();
            }

            assert_wakes(wakes.as_mut(), &[Wake::Reconcile(second.id), Wake::Idle]);

            store
                .mutate(|state| state.send_message(second.id, &message(1)))
                .unwrap();

            assert_wakes(wakes.as_mut(), &[Wake::Reconcile(second.id), Wake::Idle]);

            drop(wakes);

            let mut reconnected = connect(&store, partition);

            assert_wakes(
                reconnected.as_mut(),
                &[
                    Wake::Reconcile(first.id),
                    Wake::Reconcile(second.id),
                    Wake::Idle,
                ],
            );
        }
    }

    #[test]
    fn client_activity_wakes_waiting_consumer() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let partition = store.add_partition();
        let parked = agent(0);
        let active = agent(1);

        store
            .mutate(|state| state.create_agent(&parked, Some(&message(0))))
            .unwrap();

        store
            .mutate(|state| state.create_agent(&active, Some(&message(0))))
            .unwrap();

        let mut wakes = connect(&store, partition);

        assert_wakes(
            wakes.as_mut(),
            &[
                Wake::Reconcile(parked.id),
                Wake::Reconcile(active.id),
                Wake::Idle,
            ],
        );

        assert!(
            store
                .mutate(|state| state.consume_messages(parked.id, None, true))
                .unwrap()
        );

        store
            .mutate(|state| state.park(parked.id, Some(0.into())))
            .unwrap();

        let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let task_waker = waker(flag.clone());

        assert_eq!(
            wakes
                .as_mut()
                .poll_next(&mut Context::from_waker(&task_waker)),
            Poll::Pending
        );

        store
            .mutate(|state| state.send_message(active.id, &message(1)))
            .unwrap();

        assert!(flag.0.load(Ordering::Relaxed));
        assert_wakes(wakes.as_mut(), &[Wake::Reconcile(active.id), Wake::Idle]);
    }

    #[test]
    fn queued_wakes_remain_after_work_finishes() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let partition = store.add_partition();

        store
            .mutate(|state| state.create_agent(&agent(0), None))
            .unwrap();

        let mut wakes = connect(&store, partition);

        assert_wakes(wakes.as_mut(), &[Wake::Idle]);

        for id in 0..2 {
            store
                .mutate(|state| state.send_message(0.into(), &message(id)))
                .unwrap();
        }

        store
            .mutate(|state| state.consume_messages(0.into(), None, true))
            .unwrap();

        store
            .mutate(|state| state.park(0.into(), Some(1.into())))
            .unwrap();

        assert!(store.state.lock().unwrap().agents[&0.into()].is_idle());

        assert_wakes(
            wakes.as_mut(),
            &[
                Wake::Reconcile(0.into()),
                Wake::Reconcile(0.into()),
                Wake::Idle,
            ],
        );
    }

    #[test]
    fn empty_and_idle_stores_yield_only_idle() {
        let scheduler = Scheduler::new(0);
        let store = store(&scheduler);
        let partition = store.add_partition();
        let mut empty = connect(&store, partition);

        assert_wakes(empty.as_mut(), &[Wake::Idle]);

        drop(empty);

        let idle = agent(0);

        store
            .mutate(|state| state.create_agent(&idle, None))
            .unwrap();

        let mut wakes = connect(&store, partition);

        assert_wakes(wakes.as_mut(), &[Wake::Idle]);
    }
}
