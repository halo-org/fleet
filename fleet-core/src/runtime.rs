#![expect(
    clippy::future_not_send,
    reason = "returned Futures are Send according to _assert_run_is_send"
)]

use crate::{
    Agent, AgentId, Backoff, Compaction, Error as _, ErrorHandler, Event, EventCursor, EventLog,
    EventPayload, IdleHandler, Model, ModelCompactionResponse, ModelContext, ModelContinuation,
    ModelInput, ModelOutput, ModelRequest, ModelResponse, ModelResponseChunk, ModelTool, Models,
    OperationError, ParametersError, PrioritySignal, Role, Roles, RuntimeError, RuntimeStore,
    RuntimeStoreErrorKind, Signal, SignalReceiver, SignalSender, ToolCall, ToolCallId, ToolContext,
    ToolResult, Tools, ToolsErrorKind, Wake,
};
use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet, btree_map},
    vec::Vec,
};
use core::{
    mem,
    pin::{Pin, pin},
};
use futures_util::{
    FutureExt as _, Stream, StreamExt as _, TryStreamExt as _,
    future::{self, Either},
    stream::{self, FuturesUnordered},
};

/// Runs agents with pending work.
///
/// Returns when the store's wake stream ends or `idle_handler` finishes while the runtime is idle.
pub async fn run<I, E, S, R, M, T>(
    idle_handler: I,
    error_handler: E,
    store: S,
    roles: R,
    models: M,
    tools: T,
) where
    I: IdleHandler,
    E: ErrorHandler,
    S: RuntimeStore,
    R: Roles,
    M: Models,
    T: Tools,
{
    Runtime {
        idle_handler,
        error_handler,
        store,
        roles,
        models,
        tools,
    }
    .run()
    .await;
}

struct Runtime<I, E, S, R, M, T> {
    idle_handler: I,
    error_handler: E,
    store: S,
    roles: R,
    models: M,
    tools: T,
}

struct ReconcileState {
    cursor: Option<EventCursor>,
    tool_calls: BTreeMap<ToolCallId, ToolCall>,
    tool_calls_to_start: BTreeSet<ToolCallId>,
    tool_results: BTreeMap<ToolCallId, ToolResult>,
}

enum ReconcileAction {
    UpdateToolCalls {
        cursor: EventCursor,
        pending_tool_calls: Vec<ToolCall>,
        completed_tool_calls: Vec<ToolCallId>,
    },
    StartToolCalls,
    Continue,
    Wait,
    Stop,
}

enum ModelResult {
    Response(ModelResponse),
    Compaction(ModelCompactionResponse),
}

struct AgentRole {
    agent: Agent,
    role: Role,
}

enum Outcome<T> {
    Completed(T),
    Obsolete,
}

#[derive(Debug)]
struct AgentNotFound;

// This is needed to work around a compiler bug that prevents the retry method from using
// `AsyncFnMut`. Once fixed, it can be removed and the method updated.
//
// https://github.com/rust-lang/rust/issues/145127
macro_rules! retry {
    ($runtime:expr, $agent_id:expr, $attempt:expr $(,)?) => {
        async {
            let mut backoff = Backoff::new(&($runtime).error_handler);

            loop {
                let result = ($attempt)
                    .await
                    .map_err(|source| Self::error(Some($agent_id), source));

                backoff.handle_result(&result).await;

                if let Ok(value) = result {
                    break value;
                }
            }
        }
    };
}

impl<I, E, S, R, M, T> Runtime<I, E, S, R, M, T>
where
    I: IdleHandler,
    E: ErrorHandler,
    S: RuntimeStore,
    R: Roles,
    M: Models,
    T: Tools,
{
    // === Scheduling ===

    async fn run(&self) {
        let mut tasks = FuturesUnordered::new();
        let mut signal_senders = BTreeMap::<AgentId, SignalSender>::new();
        let mut idle_cursors = BTreeMap::<AgentId, EventCursor>::new();
        let mut wakes = pin!(self.wakes());
        let mut store_is_idle = false;

        loop {
            let completion_or_wake = match future::select(tasks.next(), wakes.next()).await {
                Either::Left((Some(completion), _)) => Either::Left(completion),
                Either::Left((None, wake_future)) => {
                    match future::select(wake_future, pin!(self.handle_idle(store_is_idle))).await {
                        Either::Left((Some(wake), _)) => Either::Right(wake),
                        _ => return,
                    }
                }
                Either::Right((Some(wake), _)) => Either::Right(wake),
                Either::Right((None, _)) => return,
            };

            match completion_or_wake {
                Either::Left((agent_id, cursor)) => {
                    signal_senders
                        .remove(&agent_id)
                        .expect("entry outlives its task");

                    if let Some(cursor) = cursor {
                        idle_cursors.insert(agent_id, cursor);
                    }
                }
                Either::Right(wake @ (Wake::Reconcile(agent_id) | Wake::Interrupt(agent_id))) => {
                    store_is_idle = false;

                    let signal = if matches!(wake, Wake::Interrupt(_)) {
                        Signal::Recheck
                    } else {
                        Signal::Check
                    };

                    match signal_senders.entry(agent_id) {
                        btree_map::Entry::Occupied(entry) => entry.get().send(signal),
                        btree_map::Entry::Vacant(entry) => {
                            let cursor = idle_cursors.remove(&agent_id);
                            let (task, sender) = self.build_reconcile_task(agent_id, cursor);
                            entry.insert(sender);
                            tasks.push(task);
                        }
                    }
                }
                Either::Right(Wake::Release(agent_id)) => {
                    store_is_idle = false;
                    idle_cursors.remove(&agent_id);

                    if let Some(sender) = signal_senders.get(&agent_id) {
                        sender.send(Signal::Stop);
                    }
                }
                Either::Right(Wake::Idle) => store_is_idle = true,
            }
        }
    }

    async fn handle_idle(&self, store_is_idle: bool) {
        if store_is_idle {
            self.idle_handler.on_idle().await;
        } else {
            future::pending::<()>().await;
        }
    }

    fn build_reconcile_task(
        &self,
        agent_id: AgentId,
        idle_cursor: Option<EventCursor>,
    ) -> (
        impl Future<Output = (AgentId, Option<EventCursor>)>,
        SignalSender,
    ) {
        let (sender, receiver) = crate::signal_channel();
        let task = self
            .reconcile(receiver, agent_id, idle_cursor)
            .map(move |cursor| (agent_id, cursor));
        (task, sender)
    }

    // === Reconciliation ===

    async fn reconcile(
        &self,
        mut receiver: SignalReceiver,
        agent_id: AgentId,
        idle_cursor: Option<EventCursor>,
    ) -> Option<EventCursor> {
        let mut state = ReconcileState::new(idle_cursor);
        let mut tool_call_futures = FuturesUnordered::new();

        loop {
            let mut tool_results = Vec::<ToolResult>::new();

            let action = {
                let mut step = pin!(self.reconcile_step(agent_id, &state));

                loop {
                    let next_event = async {
                        let tool_result = tool_call_futures.next();
                        let signal = receiver.priority_signal();

                        match future::select(tool_result, signal).await {
                            Either::Left((Some(tool_result), _)) => Either::Left(tool_result),
                            Either::Left((None, signal)) => Either::Right(signal.await),
                            Either::Right((signal, _)) => Either::Right(signal),
                        }
                    };

                    match future::select(step.as_mut(), pin!(next_event)).await {
                        Either::Left((action, _)) => break action,
                        Either::Right((Either::Left(Outcome::Completed(tool_result)), _)) => {
                            tool_results.push(tool_result);
                        }
                        Either::Right((
                            Either::Left(Outcome::Obsolete)
                            | Either::Right(PrioritySignal::Recheck),
                            _,
                        )) => {
                            break ReconcileAction::Continue;
                        }
                        Either::Right((Either::Right(PrioritySignal::Stop), _)) => {
                            break ReconcileAction::Stop;
                        }
                    }
                }
            };

            let step_is_stale = !tool_results.is_empty();

            for tool_result in tool_results {
                state.record_tool_result(tool_result);
            }

            match action {
                ReconcileAction::UpdateToolCalls {
                    cursor,
                    pending_tool_calls,
                    completed_tool_calls,
                } => {
                    for tool_call_id in completed_tool_calls {
                        state.tool_calls.remove(&tool_call_id);
                        state.tool_calls_to_start.remove(&tool_call_id);
                        state.tool_results.remove(&tool_call_id);
                    }

                    if state.tool_calls.is_empty() {
                        tool_call_futures.clear();
                    }

                    for tool_call in pending_tool_calls {
                        if let btree_map::Entry::Vacant(entry) =
                            state.tool_calls.entry(tool_call.id.clone())
                        {
                            state.tool_calls_to_start.insert(tool_call.id.clone());
                            entry.insert(tool_call);
                        }
                    }

                    state.cursor = Some(cursor);
                }
                ReconcileAction::StartToolCalls => {
                    for tool_call_id in mem::take(&mut state.tool_calls_to_start) {
                        let tool_call = state
                            .tool_calls
                            .get(&tool_call_id)
                            .expect("call is awaiting execution")
                            .clone();
                        tool_call_futures.push(self.execute_tool_call(agent_id, tool_call));
                    }
                }
                ReconcileAction::Continue => {}
                ReconcileAction::Wait if step_is_stale => {}
                ReconcileAction::Wait if tool_call_futures.is_empty() => {
                    match receiver.take_signal() {
                        Some(Signal::Check | Signal::Recheck) => {}
                        Some(Signal::Stop) => return None,
                        None => return state.cursor,
                    }
                }
                ReconcileAction::Wait => {
                    match future::select(tool_call_futures.next(), receiver.signal()).await {
                        Either::Left((Some(Outcome::Completed(tool_result)), _)) => {
                            state.record_tool_result(tool_result);
                        }
                        Either::Left((None, _)) => unreachable!("never empty in this branch"),
                        Either::Left((Some(Outcome::Obsolete), _))
                        | Either::Right((Signal::Check | Signal::Recheck, _)) => {}
                        Either::Right((Signal::Stop, _)) => return None,
                    }
                }
                ReconcileAction::Stop => return None,
            }
        }
    }

    async fn reconcile_step(&self, agent_id: AgentId, state: &ReconcileState) -> ReconcileAction {
        let Some(agent) = self
            .retry(agent_id, async || {
                self.store
                    .agent_state(agent_id)
                    .await
                    .map_err(OperationError::LoadAgentState)
            })
            .await
        else {
            return ReconcileAction::Stop;
        };

        if let Some(latest_event) = agent.latest_event
            && state.has_unread_events(latest_event)
        {
            return self
                .reconcile_tool_calls(agent_id, state, latest_event)
                .await;
        }

        if agent.interrupt_intent {
            return self
                .reconcile_interrupt_intent(agent_id, state, agent.latest_event)
                .await;
        }

        if !state.tool_results.is_empty() {
            return self.reconcile_tool_results(agent_id, state).await;
        }

        if !state.tool_calls_to_start.is_empty() {
            return ReconcileAction::StartToolCalls;
        }

        if state.tool_calls.values().any(|call| !call.is_async) {
            return ReconcileAction::Wait;
        }

        let Some(latest_event) = agent.latest_event else {
            return self.reconcile_parked(agent_id, None).await;
        };

        self.reconcile_conversation(
            agent_id,
            state,
            latest_event,
            agent.parked_at == Some(latest_event),
        )
        .await
    }

    async fn reconcile_tool_calls(
        &self,
        agent_id: AgentId,
        state: &ReconcileState,
        latest_event: EventCursor,
    ) -> ReconcileAction {
        let mut events = pin!(self.events(agent_id, latest_event, state.cursor));
        let mut completed_tool_calls = BTreeSet::new();
        let mut pending_tool_calls = Vec::new();
        let mut latest_compaction = None::<Compaction>;

        loop {
            let event = match events.try_next().await {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(AgentNotFound) => return ReconcileAction::Stop,
            };

            if latest_compaction
                .as_ref()
                .is_some_and(|compaction| compaction.last_event >= event.cursor)
            {
                break;
            }

            match event.payload {
                EventPayload::Input(..) => {}
                EventPayload::ModelResponse(response) => {
                    for output in response.output {
                        if let ModelOutput::ToolCall(tool_call) = output
                            && !completed_tool_calls.contains(&tool_call.id)
                        {
                            pending_tool_calls.push(tool_call);
                        }
                    }
                }
                EventPayload::ToolResult(tool_result) => {
                    completed_tool_calls.insert(tool_result.call_id);
                }
                EventPayload::Compaction(compaction) => {
                    if state.cursor.is_none() && latest_compaction.is_none() {
                        latest_compaction = Some(compaction);
                    }
                }
            }
        }

        if let Some(compaction) = latest_compaction {
            pending_tool_calls.extend(
                compaction
                    .pending_tool_calls
                    .into_iter()
                    .filter(|call| !completed_tool_calls.contains(&call.id)),
            );
        }

        let completed_tool_calls = completed_tool_calls
            .into_iter()
            .filter(|id| state.tool_calls.contains_key(id))
            .collect();

        ReconcileAction::UpdateToolCalls {
            cursor: latest_event,
            pending_tool_calls,
            completed_tool_calls,
        }
    }

    async fn reconcile_interrupt_intent(
        &self,
        agent_id: AgentId,
        state: &ReconcileState,
        latest_event: Option<EventCursor>,
    ) -> ReconcileAction {
        self.retry(agent_id, async || {
            let Outcome::Completed(tool_results) =
                self.interrupted_tool_results(agent_id, state).await?
            else {
                return Ok(ReconcileAction::Stop);
            };

            let result = self
                .store
                .settle_interrupt(agent_id, latest_event, &tool_results)
                .await;

            match result {
                Ok(()) => Ok(ReconcileAction::Continue),
                Err(error) if error.kind() == RuntimeStoreErrorKind::Conflict => {
                    Ok(ReconcileAction::Continue)
                }
                Err(error) => Err(OperationError::SettleInterrupt(error)),
            }
        })
        .await
    }

    async fn reconcile_tool_results(
        &self,
        agent_id: AgentId,
        state: &ReconcileState,
    ) -> ReconcileAction {
        future::join_all(state.tool_results.values().map(|tool_result| {
            self.retry(agent_id, async || {
                match self.store.append_tool_result(agent_id, tool_result).await {
                    Err(error) if error.kind() == RuntimeStoreErrorKind::Conflict => Ok(()),
                    result => result.map_err(|source| OperationError::AppendToolResult {
                        call_id: tool_result.call_id.clone(),
                        source,
                    }),
                }
            })
        }))
        .await;

        ReconcileAction::Continue
    }

    async fn reconcile_parked(
        &self,
        agent_id: AgentId,
        latest_event: Option<EventCursor>,
    ) -> ReconcileAction {
        self.retry(agent_id, async || {
            let result = self
                .store
                .consume_messages(agent_id, latest_event, true)
                .await;

            match result {
                Ok(true) => Ok(ReconcileAction::Continue),
                Ok(false) => Ok(ReconcileAction::Wait),
                Err(error) if error.kind() == RuntimeStoreErrorKind::Conflict => {
                    Ok(ReconcileAction::Continue)
                }
                Err(error) => Err(OperationError::ConsumeMessages(error)),
            }
        })
        .await
    }

    async fn reconcile_conversation(
        &self,
        agent_id: AgentId,
        state: &ReconcileState,
        latest_event: EventCursor,
        is_parked: bool,
    ) -> ReconcileAction {
        let mut event_log = EventLog::new(self.events(agent_id, latest_event, None));

        if !is_parked {
            let turn_is_finished = match event_log.is_turn_finished().await {
                Ok(finished) => finished,
                Err(AgentNotFound) => return ReconcileAction::Stop,
            };

            if turn_is_finished {
                self.retry(agent_id, async || {
                    match self.store.park(agent_id, Some(latest_event)).await {
                        Err(error) if error.kind() == RuntimeStoreErrorKind::Conflict => Ok(()),
                        result => result.map_err(OperationError::ParkAgent),
                    }
                })
                .await;

                return ReconcileAction::Continue;
            }
        }

        retry!(self, agent_id, async {
            let Outcome::Completed(mut agent_role) = self.agent_role(agent_id).await? else {
                return Ok(ReconcileAction::Stop);
            };

            agent_role.role.model.config = crate::apply_model_parameters(
                agent_role.role.model.config,
                &agent_role.role.parameters,
                &agent_role.agent.role.config,
            )
            .map_err(|source| OperationError::ApplyModelParameters {
                role: agent_role.agent.role.name.clone(),
                source,
            })?;

            let model = self.model(&agent_role).await?;

            let compaction_threshold = agent_role
                .role
                .compaction
                .as_ref()
                .and_then(|compaction| compaction.threshold)
                .unwrap_or(model.compaction_threshold);

            let should_compact = match event_log.last_token_count().await {
                Ok(tokens) => tokens.is_some_and(|tokens| tokens >= compaction_threshold),
                Err(AgentNotFound) => return Ok(ReconcileAction::Stop),
            };

            if !should_compact {
                let result = self
                    .store
                    .consume_messages(agent_id, Some(latest_event), is_parked)
                    .await;

                match result {
                    Ok(true) => return Ok(ReconcileAction::Continue),
                    Ok(false) if is_parked => return Ok(ReconcileAction::Wait),
                    Ok(false) => {}
                    Err(error) if error.kind() == RuntimeStoreErrorKind::Conflict => {
                        return Ok(ReconcileAction::Continue);
                    }
                    Err(error) => return Err(OperationError::ConsumeMessages(error)),
                }
            }

            let Outcome::Completed(model_result) = self
                .run_model(
                    &agent_role,
                    latest_event,
                    model.context_window,
                    should_compact,
                    &mut event_log,
                )
                .await?
            else {
                return Ok(ReconcileAction::Stop);
            };

            self.append_model_result(&agent_role, latest_event, state, model_result)
                .await
        })
        .await
    }

    // === Tools ===

    async fn execute_tool_call(
        &self,
        agent_id: AgentId,
        tool_call: ToolCall,
    ) -> Outcome<ToolResult> {
        self.retry(agent_id, async || {
            let Outcome::Completed(agent_role) = self.agent_role(agent_id).await? else {
                return Ok(Outcome::Obsolete);
            };

            let context = agent_role.tool_context(&tool_call).map_err(|source| {
                OperationError::ApplyToolParameters {
                    role: agent_role.agent.role.name.clone(),
                    source,
                }
            })?;

            let output = if let Some(context) = context {
                let is_retry = self
                    .retry(agent_id, async || {
                        match self.store.prepare_tool_call(agent_id, &tool_call.id).await {
                            Ok(is_retry) => Ok(Outcome::Completed(is_retry)),
                            Err(error) if error.kind() == RuntimeStoreErrorKind::Conflict => {
                                Ok(Outcome::Obsolete)
                            }
                            Err(source) => Err(OperationError::PrepareToolCall {
                                call_id: tool_call.id.clone(),
                                source,
                            }),
                        }
                    })
                    .await;

                let Outcome::Completed(is_retry) = is_retry else {
                    return Ok(Outcome::Obsolete);
                };

                let result = self
                    .tools
                    .execute(&context, is_retry, &tool_call.input)
                    .await;

                match result {
                    Ok(output) => output,
                    Err(error) if error.kind() == ToolsErrorKind::UnsafeToRetry => self
                        .tools
                        .interrupted_output(&context, &tool_call.input)
                        .await
                        .map_err(|source| OperationError::ProduceInterruptedToolOutput {
                            tool: context.tool.name.clone(),
                            call_id: tool_call.id.clone(),
                            source,
                        })?,
                    Err(source) => {
                        return Err(OperationError::ExecuteTool {
                            tool: context.tool.name.clone(),
                            call_id: tool_call.id.clone(),
                            source,
                        });
                    }
                }
            } else {
                self.tools.unknown_tool_output(agent_id, &tool_call)
            };

            Ok(Outcome::Completed(ToolResult {
                call_id: tool_call.id.clone(),
                output,
            }))
        })
        .await
    }

    async fn interrupted_tool_results(
        &self,
        agent_id: AgentId,
        state: &ReconcileState,
    ) -> Result<Outcome<Vec<ToolResult>>, OperationError<S::Error, R::Error, M::Error, T::Error>>
    {
        if state.tool_calls.is_empty() {
            return Ok(Outcome::Completed(Vec::new()));
        }

        let Outcome::Completed(agent_role) = self.agent_role(agent_id).await? else {
            return Ok(Outcome::Obsolete);
        };

        let mut results = Vec::new();

        for call in state.tool_calls.values() {
            if let Some(result) = state.tool_results.get(&call.id) {
                results.push(result.clone());
                continue;
            }

            let context = agent_role.tool_context(call).map_err(|source| {
                OperationError::ApplyToolParameters {
                    role: agent_role.agent.role.name.clone(),
                    source,
                }
            })?;

            let output = if let Some(context) = context {
                self.tools
                    .interrupted_output(&context, &call.input)
                    .await
                    .map_err(|source| OperationError::ProduceInterruptedToolOutput {
                        tool: context.tool.name.clone(),
                        call_id: call.id.clone(),
                        source,
                    })?
            } else {
                self.tools.unknown_tool_output(agent_id, call)
            };

            results.push(ToolResult {
                call_id: call.id.clone(),
                output,
            });
        }

        Ok(Outcome::Completed(results))
    }

    // === Model ===

    async fn run_model<L>(
        &self,
        agent_role: &AgentRole,
        latest_event: EventCursor,
        context_window: u32,
        should_compact: bool,
        event_log: &mut EventLog<L>,
    ) -> Result<Outcome<ModelResult>, OperationError<S::Error, R::Error, M::Error, T::Error>>
    where
        L: Stream<Item = Result<Event, AgentNotFound>>,
    {
        let compaction_instructions = agent_role
            .role
            .compaction
            .as_ref()
            .and_then(|compaction| compaction.instructions.as_deref());

        let model_context = self.model_context(agent_role).await?;

        let mut model_request =
            match ModelRequest::prepare(event_log, model_context, context_window, should_compact)
                .await
            {
                Ok(request) => request,
                Err(AgentNotFound) => return Ok(Outcome::Obsolete),
            };

        loop {
            let result = if model_request.should_compact() {
                self.models
                    .compact(
                        model_request.context(),
                        model_request.continuation(),
                        model_request.input(),
                        compaction_instructions,
                    )
                    .await
                    .map(ModelResult::Compaction)
                    .map_err(|source| OperationError::CompactModelInput {
                        model: agent_role.role.model.name.clone(),
                        source,
                    })
            } else {
                self.generate_model_response(
                    agent_role.agent.id,
                    latest_event,
                    model_request.context(),
                    model_request.continuation(),
                    model_request.input(),
                )
                .await
                .map(ModelResult::Response)
            };

            let error = match result {
                Ok(model_result) => return Ok(Outcome::Completed(model_result)),
                Err(error) => error,
            };

            let error_kind = match &error {
                OperationError::GenerateModelResponse { source, .. }
                | OperationError::CompactModelInput { source, .. } => source.kind(),
                _ => return Err(error),
            };

            match model_request.recover(error_kind).await {
                Ok(true) => {}
                Ok(false) => return Err(error),
                Err(AgentNotFound) => return Ok(Outcome::Obsolete),
            }
        }
    }

    async fn generate_model_response(
        &self,
        agent_id: AgentId,
        latest_event: EventCursor,
        context: &ModelContext,
        continuation: Option<&ModelContinuation>,
        input: &[ModelInput],
    ) -> Result<ModelResponse, OperationError<S::Error, R::Error, M::Error, T::Error>> {
        let mut stream = pin!(self.models.generate(context, continuation, input));
        let mut chunk_sequence = Some(0);

        while let Some(chunk) =
            stream
                .try_next()
                .await
                .map_err(|source| OperationError::GenerateModelResponse {
                    model: context.model.name.clone(),
                    source,
                })?
        {
            if let Some(sequence) = chunk_sequence {
                let result = self
                    .store
                    .publish_model_response_chunk(agent_id, latest_event, sequence, &chunk)
                    .await;

                match result {
                    Ok(()) => chunk_sequence = sequence.checked_add(1),
                    Err(error) => {
                        chunk_sequence = None;
                        self.error_handler
                            .handle_error(Self::error(
                                Some(agent_id),
                                OperationError::PublishModelResponseChunk {
                                    model: context.model.name.clone(),
                                    source: error,
                                },
                            ))
                            .await;
                    }
                }
            }

            if let ModelResponseChunk::ResponseEnd(response) = chunk {
                return Ok(response);
            }
        }

        Err(OperationError::IncompleteModelResponse {
            model: context.model.name.clone(),
            latest_event,
        })
    }

    async fn append_model_result(
        &self,
        agent_role: &AgentRole,
        latest_event: EventCursor,
        state: &ReconcileState,
        model_result: ModelResult,
    ) -> Result<ReconcileAction, OperationError<S::Error, R::Error, M::Error, T::Error>> {
        let agent_id = agent_role.agent.id;

        let result = match model_result {
            ModelResult::Response(response) => self
                .store
                .append_model_response(agent_id, latest_event, &response)
                .await
                .map_err(|source| OperationError::AppendModelResponse {
                    model: agent_role.role.model.name.clone(),
                    source,
                }),
            ModelResult::Compaction(response) => {
                let compaction = Compaction {
                    last_event: latest_event,
                    input: response.input,
                    pending_tool_calls: state.tool_calls.values().cloned().collect(),
                    usage: response.usage,
                };

                self.store
                    .append_compaction(agent_id, latest_event, &compaction)
                    .await
                    .map_err(|source| OperationError::AppendCompaction {
                        model: agent_role.role.model.name.clone(),
                        source,
                    })
            }
        };

        match result {
            Ok(()) => Ok(ReconcileAction::Continue),
            Err(
                OperationError::AppendModelResponse { source, .. }
                | OperationError::AppendCompaction { source, .. },
            ) if source.kind() == RuntimeStoreErrorKind::Conflict => Ok(ReconcileAction::Continue),
            Err(error) => Err(error),
        }
    }

    // === Configuration ===

    async fn agent_role(
        &self,
        agent_id: AgentId,
    ) -> Result<Outcome<AgentRole>, OperationError<S::Error, R::Error, M::Error, T::Error>> {
        let Some(agent) = self
            .store
            .agent(agent_id)
            .await
            .map_err(OperationError::LoadAgent)?
        else {
            return Ok(Outcome::Obsolete);
        };

        let role = self
            .roles
            .get(&agent.role.name)
            .await
            .map_err(|source| OperationError::LoadRole {
                role: agent.role.name.clone(),
                source,
            })?
            .ok_or_else(|| OperationError::RoleNotFound {
                role: agent.role.name.clone(),
            })?;

        Ok(Outcome::Completed(AgentRole { agent, role }))
    }

    async fn model(
        &self,
        agent_role: &AgentRole,
    ) -> Result<Model, OperationError<S::Error, R::Error, M::Error, T::Error>> {
        self.models
            .get(&agent_role.role.model.name)
            .await
            .map_err(|source| OperationError::LoadModel {
                model: agent_role.role.model.name.clone(),
                source,
            })?
            .ok_or_else(|| OperationError::ModelNotFound {
                role: agent_role.agent.role.name.clone(),
                model: agent_role.role.model.name.clone(),
            })
    }

    async fn model_context(
        &self,
        agent_role: &AgentRole,
    ) -> Result<ModelContext, OperationError<S::Error, R::Error, M::Error, T::Error>> {
        let mut tools = Vec::new();

        for (alias, tool) in &agent_role.role.tools {
            let tool = self
                .tools
                .get(&tool.name)
                .await
                .map_err(|source| OperationError::LoadTool {
                    tool: tool.name.clone(),
                    source,
                })?
                .ok_or_else(|| OperationError::ToolNotFound {
                    role: agent_role.agent.role.name.clone(),
                    alias: alias.clone(),
                    tool: tool.name.clone(),
                })?;

            tools.push(ModelTool {
                name: alias.clone(),
                description: tool.description,
                is_async: tool.is_async,
                input_schema: tool.input_schema,
            });
        }

        Ok(ModelContext {
            model: agent_role.role.model.clone(),
            system_prompt: agent_role.role.system_prompt.clone(),
            tools,
        })
    }

    // === Utilities ===

    #[expect(
        clippy::type_complexity,
        reason = "a type alias would be less clear and still need to preserve the generics"
    )]
    const fn error(
        agent_id: Option<AgentId>,
        source: OperationError<S::Error, R::Error, M::Error, T::Error>,
    ) -> RuntimeError<OperationError<S::Error, R::Error, M::Error, T::Error>> {
        RuntimeError::new(agent_id, source)
    }

    async fn retry<U, A>(&self, agent_id: AgentId, mut attempt: impl FnMut() -> A) -> U
    where
        A: Future<Output = Result<U, OperationError<S::Error, R::Error, M::Error, T::Error>>>,
    {
        retry!(self, agent_id, attempt()).await
    }

    fn wakes(&self) -> impl Stream<Item = Wake> {
        struct State<S, H> {
            stream: Pin<Box<S>>,
            backoff: Backoff<H>,
            store_is_idle: bool,
        }

        let state = State {
            stream: Box::pin(self.store.wakes()),
            backoff: Backoff::new(&self.error_handler),
            store_is_idle: false,
        };

        stream::unfold(state, async |mut state| {
            loop {
                let result = state
                    .stream
                    .try_next()
                    .await
                    .map_err(|source| Self::error(None, OperationError::ReadWakes(source)));

                state.backoff.handle_result(&result).await;

                match result {
                    Ok(Some(Wake::Idle)) => {
                        if !state.store_is_idle {
                            state.store_is_idle = true;
                            return Some((Wake::Idle, state));
                        }
                    }
                    Ok(Some(wake)) => {
                        state.store_is_idle = false;
                        return Some((wake, state));
                    }
                    Ok(None) => return None,
                    Err(_) => state.stream = Box::pin(self.store.wakes()),
                }
            }
        })
    }

    fn events(
        &self,
        agent_id: AgentId,
        from: EventCursor,
        until: Option<EventCursor>,
    ) -> impl Stream<Item = Result<Event, AgentNotFound>> {
        struct State<S, H> {
            stream: Pin<Box<S>>,
            backoff: Backoff<H>,
            last_event: Option<EventCursor>,
        }

        let state = State {
            stream: Box::pin(self.store.events(agent_id, from, until)),
            backoff: Backoff::new(&self.error_handler),
            last_event: None,
        };

        stream::try_unfold(state, move |mut state| async move {
            loop {
                let result = match state.stream.try_next().await {
                    Ok(Some(event)) if Some(event.cursor) == state.last_event => continue,
                    Err(error) if error.kind() == RuntimeStoreErrorKind::Conflict => {
                        return Err(AgentNotFound);
                    }
                    result => result,
                }
                .map_err(|source| {
                    Self::error(
                        Some(agent_id),
                        OperationError::ReadEvents {
                            cursor: state.last_event.unwrap_or(from),
                            source,
                        },
                    )
                });

                state.backoff.handle_result(&result).await;

                match result {
                    Ok(Some(event)) => {
                        state.last_event = Some(event.cursor);
                        return Ok(Some((event, state)));
                    }
                    Ok(None) => return Ok(None),
                    Err(_) => {
                        state.stream = Box::pin(self.store.events(
                            agent_id,
                            state.last_event.unwrap_or(from),
                            until,
                        ));
                    }
                }
            }
        })
    }
}

impl ReconcileState {
    const fn new(idle_cursor: Option<EventCursor>) -> Self {
        Self {
            cursor: idle_cursor,
            tool_calls: BTreeMap::new(),
            tool_calls_to_start: BTreeSet::new(),
            tool_results: BTreeMap::new(),
        }
    }

    fn has_unread_events(&self, cursor: EventCursor) -> bool {
        self.cursor.is_none_or(|c| c < cursor)
    }

    fn record_tool_result(&mut self, result: ToolResult) {
        if self.tool_calls.contains_key(&result.call_id) {
            self.tool_results
                .entry(result.call_id.clone())
                .or_insert(result);
        }
    }
}

impl AgentRole {
    fn tool_context(&self, call: &ToolCall) -> Result<Option<ToolContext>, ParametersError> {
        let Some(tool) = self.role.tools.get(&call.tool) else {
            return Ok(None);
        };

        let mut tool = tool.clone();

        tool.config = crate::apply_tool_parameters(
            &call.tool,
            tool.config,
            &self.role.parameters,
            &self.agent.role.config,
        )?;

        Ok(Some(ToolContext {
            tool,
            agent_id: self.agent.id,
            call_id: call.id.clone(),
        }))
    }
}

fn _assert_run_is_send<I, E, S, R, M, T>(
    idle_handler: I,
    error_handler: E,
    store: S,
    roles: R,
    models: M,
    tools: T,
) -> impl Future<Output = ()> + Send
where
    I: IdleHandler + Send + Sync,
    E: ErrorHandler + Send + Sync,
    S: RuntimeStore + Send + Sync,
    S::Error: Send,
    R: Roles + Send + Sync,
    R::Error: Send,
    M: Models + Send + Sync,
    M::Error: Send,
    T: Tools + Send + Sync,
    T::Error: Send,
{
    run(idle_handler, error_handler, store, roles, models, tools)
}
