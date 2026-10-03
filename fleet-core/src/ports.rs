use crate::{
    Agent, AgentId, AgentState, Event, EventCursor, JsonObject, Message, Model, ModelContext,
    ModelInput, ModelName, ModelResponse, ModelResponseChunk, Role, RoleName, Tool, ToolCall,
    ToolCallId, ToolContext, ToolName, ToolOutput, ToolResult, Wake,
};
use alloc::{boxed::Box, rc::Rc, sync::Arc};
use core::fmt;
use futures_util::Stream;

// === Roles ===

/// Provides access to roles.
pub trait Roles {
    /// The error type returned by this provider.
    type Error: core::error::Error + 'static;

    /// Returns the role with the given name.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to perform the lookup.
    fn get(
        &self,
        name: &RoleName,
    ) -> impl Future<Output = Result<Option<Role>, Self::Error>> + Send;
}

// === Models ===

/// Provides access to models.
pub trait Models {
    /// The error type returned by this provider.
    type Error: crate::Error<ModelsErrorKind> + 'static;

    /// Returns the model with the given name.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to perform the lookup.
    fn get(
        &self,
        name: &ModelName,
    ) -> impl Future<Output = Result<Option<Model>, Self::Error>> + Send;

    /// Streams a response to the provided input.
    ///
    /// A successful stream emits exactly one [`ResponseEnd`](ModelResponseChunk::ResponseEnd) as
    /// its final item. Ending without it means an incomplete response.
    ///
    /// # Errors
    ///
    /// Yields errors with the following error kinds:
    ///
    /// - [`InvalidContinuation`](ModelsErrorKind::InvalidContinuation) when the conversation can't
    ///   be continued using the provided continuation token.
    /// - [`ContextTooLong`](ModelsErrorKind::ContextTooLong) when the context of the conversation
    ///   exceeds the model's maximum context window.
    /// - [`Other`](ModelsErrorKind::Other) when there are any other provider specific errors.
    ///
    /// An error terminates the stream.
    fn generate(
        &self,
        context: &ModelContext,
        input: &[ModelInput],
    ) -> impl Stream<Item = Result<ModelResponseChunk, Self::Error>> + Send;
}

/// The kind of error returned by a model provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModelsErrorKind {
    /// The continuation token isn't valid for this model.
    ///
    /// This may be because it was issued by another model, or because it has expired.
    InvalidContinuation,
    /// The context of the conversation exceeds the model's maximum context window.
    ContextTooLong,
    /// An error not covered by any other kind.
    Other,
}

// === Tools ===

/// Provides access to tools.
pub trait Tools {
    /// The error type returned by this provider.
    type Error: crate::Error<ToolsErrorKind> + 'static;

    /// Returns the tool with the given name.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to perform the lookup.
    fn get(
        &self,
        name: &ToolName,
    ) -> impl Future<Output = Result<Option<Tool>, Self::Error>> + Send;

    /// Executes a tool.
    ///
    /// `is_retry` indicates whether this tool call may have been attempted before. An earlier
    /// attempt may still be running or may already have completed.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`UnsafeToRetry`](ToolsErrorKind::UnsafeToRetry) when a retry cannot be executed safely.
    /// - [`Other`](ToolsErrorKind::Other) when there are any other provider specific errors.
    ///
    /// Note that tool outputs can signal their own errors via [`ToolOutput::is_error`]. These are
    /// still considered successful so shouldn't be retried.
    fn execute(
        &self,
        context: &ToolContext,
        is_retry: bool,
        input: &JsonObject,
    ) -> impl Future<Output = Result<ToolOutput, Self::Error>> + Send;

    /// Returns the output for an interrupted tool call.
    ///
    /// This does not interrupt any ongoing tool calls, but simply returns what the output of an
    /// interrupted one should be.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to build the tool output.
    fn interrupted_output(
        &self,
        context: &ToolContext,
        input: &JsonObject,
    ) -> impl Future<Output = Result<ToolOutput, Self::Error>> + Send;

    /// Returns the output for an unknown tool.
    fn unknown_tool_output(&self, agent_id: AgentId, call: &ToolCall) -> ToolOutput;
}

/// The kind of error returned by a tools provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolsErrorKind {
    /// The tool call cannot be retried safely because it risks repeating effects of an earlier attempt.
    UnsafeToRetry,
    /// An error not covered by any other kind.
    Other,
}

// === Error Handler ===

/// Handles errors and controls retry backoff.
pub trait ErrorHandler {
    /// Handles an error.
    ///
    /// This is typically used for error reporting.
    fn handle_error<E>(&self, error: E) -> impl Future<Output = ()> + Send
    where
        E: core::error::Error;

    /// Handles an error with a backoff.
    ///
    /// This is typically used for error reporting.
    ///
    /// The future returned by this method will become ready once its backoff has completed.
    fn handle_error_with_backoff<E>(
        &self,
        error: E,
        retries: u32,
    ) -> impl Future<Output = ()> + Send
    where
        E: core::error::Error;
}

// === Idle Handler ===

/// Controls when an idle runtime may exit.
pub trait IdleHandler {
    /// Waits until an idle runtime may exit.
    fn on_idle(&self) -> impl Future<Output = ()> + Send;
}

// === Stores ===

/// Provides storage operations used by the runtime.
pub trait RuntimeStore {
    /// The error type returned by this provider.
    type Error: crate::Error<RuntimeStoreErrorKind> + 'static;

    /// Returns the agent with the given ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to perform the lookup.
    fn agent(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<Agent>, Self::Error>> + Send;

    /// Returns the state of the agent with the given ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to perform the lookup.
    fn agent_state(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<Option<AgentState>, Self::Error>> + Send;

    /// Streams wakes to drive the runtime.
    ///
    /// The stream first yields wakes for all outstanding work. After that, it yields wakes when
    /// there is new activity. Whenever the stream has caught up, it will yield [`Idle`](Wake::Idle)
    /// once. There is outstanding work when there are unprocessed events, pending messages,
    /// unresolved tool calls, or an interrupt intent for the agent.
    ///
    /// New activity is caused by mutations made by the corresponding [`ClientStore`]. Mutations
    /// made by the runtime do not trigger new wakes.
    ///
    /// # Partitioning
    ///
    /// Stores may partition wakes by agent between different instances of the store. In such
    /// stores, the stream internally manages claims on its agents and yields
    /// [`Release`](Wake::Release) once the claim is lost. Consumers should abort in-flight work and
    /// drop any resources for a released agent as soon as possible.
    ///
    /// Partitioning only affects wake delivery. Other methods on the store can be used for agents
    /// across partitions. Temporary overlapping consumers during repartition are allowed and
    /// handled by optimistic concurrency control on all write methods.
    ///
    /// # Errors
    ///
    /// Yields an error when the provider has an internal failure. An error terminates the stream.
    fn wakes(&self) -> impl Stream<Item = Result<Wake, Self::Error>> + Send;

    /// Streams events from the event log in descending cursor order.
    ///
    /// Includes events at or before `from` and strictly after `until`. When `until` is `None`,
    /// there is no lower bound.
    ///
    /// # Errors
    ///
    /// Yields errors with the following error kinds:
    ///
    /// - [`Conflict`](RuntimeStoreErrorKind::Conflict) when there is no agent with the provided ID.
    /// - [`Other`](RuntimeStoreErrorKind::Other) when there are any other provider specific errors.
    ///
    /// An error terminates the stream.
    fn events(
        &self,
        agent_id: AgentId,
        from: EventCursor,
        until: Option<EventCursor>,
    ) -> impl Stream<Item = Result<Event, Self::Error>> + Send;

    /// Settles an agent's interrupt intent.
    ///
    /// This will atomically clear the agent's interrupt intent, append the provided tool results to
    /// its event log, and park the agent at the resulting latest event.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](RuntimeStoreErrorKind::Conflict) when:
    ///   - there is no agent with the provided ID.
    ///   - the event log's latest event doesn't match `expected_latest_event`.
    ///   - the agent has no interrupt intent.
    ///   - the provided results do not correspond exactly to the unresolved tool calls.
    /// - [`Other`](RuntimeStoreErrorKind::Other) when there are any other provider specific errors.
    fn settle_interrupt(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
        tool_results: &[ToolResult],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Parks the agent.
    ///
    /// Parking marks the event log as processed through to the latest event. This means events up
    /// to that point no longer count as outstanding work. Pending messages and an interrupt intent
    /// still do.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](RuntimeStoreErrorKind::Conflict) when:
    ///   - there is no agent with the provided ID.
    ///   - the event log's latest event doesn't match `expected_latest_event`.
    /// - [`Other`](RuntimeStoreErrorKind::Other) when there are any other provider specific errors.
    fn park(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Moves pending messages into the agent's event log.
    ///
    /// Consumes all pending steering messages and, if `allow_queued` is true, the oldest queued
    /// message. Appending the messages and removing them from pending storage happen atomically.
    ///
    /// Returns whether any messages were consumed.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](RuntimeStoreErrorKind::Conflict) when:
    ///   - there is no agent with the provided ID.
    ///   - the event log's latest event doesn't match `expected_latest_event`.
    ///   - the agent has an interrupt intent.
    /// - [`Other`](RuntimeStoreErrorKind::Other) when there are any other provider specific errors.
    fn consume_messages(
        &self,
        agent_id: AgentId,
        expected_latest_event: Option<EventCursor>,
        allow_queued: bool,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Publishes a model response chunk as a live preview.
    ///
    /// `input_cursor` identifies the event log position used to generate the response. `sequence`
    /// should start at zero for each generation attempt and increase by one per chunk.
    ///
    /// If sequence continuity is lost for an input cursor, the store invalidates its preview and
    /// suppresses further chunks for that cursor. Clients must discard the invalidated preview and
    /// wait for the committed response.
    ///
    /// Chunks do not become part of the event log or trigger runtime wakes. The committed response
    /// supersedes its preview.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to publish the chunk.
    fn publish_model_response_chunk(
        &self,
        agent_id: AgentId,
        input_cursor: EventCursor,
        sequence: u32,
        chunk: &ModelResponseChunk,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Appends a model response to the agent's event log.
    ///
    /// The response supersedes any live preview for `expected_latest_event`.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](RuntimeStoreErrorKind::Conflict) when:
    ///   - there is no agent with the provided ID.
    ///   - the event log's latest event doesn't match `expected_latest_event`.
    ///   - the agent is parked at `expected_latest_event`.
    ///   - there are unresolved synchronous tool calls.
    ///   - tool call IDs in the response are duplicated or already exist in the agent's history.
    /// - [`Other`](RuntimeStoreErrorKind::Other) when there are any other provider specific errors.
    fn append_model_response(
        &self,
        agent_id: AgentId,
        expected_latest_event: EventCursor,
        response: &ModelResponse,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Prepares a tool call.
    ///
    /// Returns whether the same tool call has previously been prepared. The first preparation for a
    /// given agent ID and call ID pair returns `false`, and any subsequent preparations return
    /// `true`.
    ///
    /// A previous preparation does not guarantee that execution actually began.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](RuntimeStoreErrorKind::Conflict) when:
    ///   - there is no agent with the provided ID.
    ///   - there is no unresolved tool call with the provided ID.
    /// - [`Other`](RuntimeStoreErrorKind::Other) when there are any other provider specific errors.
    fn prepare_tool_call(
        &self,
        agent_id: AgentId,
        call_id: &ToolCallId,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;

    /// Appends a tool result to the agent's event log.
    ///
    /// The first committed result for a tool call wins. Existing results are never replaced.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](RuntimeStoreErrorKind::Conflict) when:
    ///   - there is no agent with the provided ID.
    ///   - there is no unresolved tool call matching the result's call ID.
    /// - [`Other`](RuntimeStoreErrorKind::Other) when there are any other provider specific errors.
    fn append_tool_result(
        &self,
        agent_id: AgentId,
        result: &ToolResult,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// The kind of error returned by a runtime store provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuntimeStoreErrorKind {
    /// There was a conflict between the caller's state and the store's state.
    ///
    /// This usually means the caller's state is out of date and should be reloaded.
    Conflict,
    /// An error not covered by any other kind.
    Other,
}

/// Provides storage operations used by clients.
///
/// # Idempotency
///
/// All mutations require an idempotency key. This allows all operations of a client store to be
/// idempotent. Mutation methods called repeatedly with the same idempotency key and arguments will
/// only perform their side effects once. After a mutation succeeds, matching retries return success
/// even if the store's state has since changed. Reusing a key with a different method or arguments
/// returns an [`IdempotencyConflict`](ClientStoreErrorKind::IdempotencyConflict) error.
pub trait ClientStore {
    /// The error type returned by this provider.
    type Error: crate::Error<ClientStoreErrorKind> + 'static;

    /// Creates an agent with an optional initial message.
    ///
    /// The agent starts parked with an empty event log. If provided, the message is added to its
    /// pending messages atomically on creation.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](ClientStoreErrorKind::Conflict) when:
    ///   - the agent ID is already in use.
    ///   - the parent has no unresolved tool call matching the provided call ID.
    /// - [`IdempotencyConflict`](ClientStoreErrorKind::IdempotencyConflict) when the key was used
    ///   with a different method or arguments.
    /// - [`AgentNotFound`](ClientStoreErrorKind::AgentNotFound) when the parent agent does not
    ///   exist.
    /// - [`Other`](ClientStoreErrorKind::Other) when there are any other provider specific errors.
    fn create_agent(
        &self,
        idempotency_key: &str,
        agent: &Agent,
        message: Option<&Message>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Interrupts an agent with an optional message.
    ///
    /// Sets the agent's interrupt intent and, if provided, adds the message to its pending messages
    /// atomically. Existing pending messages are preserved. Pending messages are consumed after the
    /// runtime settles the interrupt.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](ClientStoreErrorKind::Conflict) when the provided message ID is already in
    ///   use for the agent.
    /// - [`IdempotencyConflict`](ClientStoreErrorKind::IdempotencyConflict) when the key was used
    ///   with a different method or arguments.
    /// - [`AgentNotFound`](ClientStoreErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`Other`](ClientStoreErrorKind::Other) when there are any other provider specific errors.
    fn interrupt_agent(
        &self,
        idempotency_key: &str,
        agent_id: AgentId,
        message: Option<&Message>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Sends a message to an agent.
    ///
    /// The message remains pending until the runtime consumes it according to its mode.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`Conflict`](ClientStoreErrorKind::Conflict) when the message ID is already in use for the
    ///   agent.
    /// - [`IdempotencyConflict`](ClientStoreErrorKind::IdempotencyConflict) when the key was used
    ///   with a different method or arguments.
    /// - [`AgentNotFound`](ClientStoreErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`Other`](ClientStoreErrorKind::Other) when there are any other provider specific errors.
    fn send_message(
        &self,
        idempotency_key: &str,
        agent_id: AgentId,
        message: &Message,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Waits until the agent is idle.
    ///
    /// An agent is idle when it is parked and has no pending messages, interrupt intent, or
    /// unresolved tool calls.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`AgentNotFound`](ClientStoreErrorKind::AgentNotFound) when the agent does not exist.
    /// - [`Other`](ClientStoreErrorKind::Other) when there are any other provider specific errors.
    fn wait_until_idle(
        &self,
        agent_id: AgentId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// The kind of error returned by a client store provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClientStoreErrorKind {
    /// The operation conflicts with the store's current state.
    Conflict,
    /// The idempotency key was used with a different method or arguments.
    IdempotencyConflict,
    /// The agent does not exist.
    AgentNotFound,
    /// An error not covered by any other kind.
    Other,
}

// === Macros ===

macro_rules! impl_roles_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T> Roles for $t
        where
            T: Roles + ?Sized,
        {
            type Error = T::Error;

            fn get(
                &self,
                name: &RoleName,
            ) -> impl Future<Output = Result<Option<Role>, Self::Error>> + Send {
                (**self).get(name)
            }
        }
    };
}

macro_rules! impl_models_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T> Models for $t
        where
            T: Models + ?Sized,
        {
            type Error = T::Error;

            fn get(
                &self,
                name: &ModelName,
            ) -> impl Future<Output = Result<Option<Model>, Self::Error>> + Send {
                (**self).get(name)
            }

            fn generate(
                &self,
                context: &ModelContext,
                input: &[ModelInput],
            ) -> impl Stream<Item = Result<ModelResponseChunk, Self::Error>> + Send {
                (**self).generate(context, input)
            }
        }
    };
}

macro_rules! impl_tools_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T> Tools for $t
        where
            T: Tools + ?Sized,
        {
            type Error = T::Error;

            fn get(
                &self,
                name: &ToolName,
            ) -> impl Future<Output = Result<Option<Tool>, Self::Error>> + Send {
                (**self).get(name)
            }

            fn execute(
                &self,
                context: &ToolContext,
                is_retry: bool,
                input: &JsonObject,
            ) -> impl Future<Output = Result<ToolOutput, Self::Error>> + Send {
                (**self).execute(context, is_retry, input)
            }

            fn interrupted_output(
                &self,
                context: &ToolContext,
                input: &JsonObject,
            ) -> impl Future<Output = Result<ToolOutput, Self::Error>> + Send {
                (**self).interrupted_output(context, input)
            }

            fn unknown_tool_output(&self, agent_id: AgentId, call: &ToolCall) -> ToolOutput {
                (**self).unknown_tool_output(agent_id, call)
            }
        }
    };
}

macro_rules! impl_error_handler_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T> ErrorHandler for $t
        where
            T: ErrorHandler + ?Sized,
        {
            fn handle_error<E>(&self, error: E) -> impl Future<Output = ()> + Send
            where
                E: core::error::Error,
            {
                (**self).handle_error(error)
            }

            fn handle_error_with_backoff<E>(
                &self,
                error: E,
                retries: u32,
            ) -> impl Future<Output = ()> + Send
            where
                E: core::error::Error,
            {
                (**self).handle_error_with_backoff(error, retries)
            }
        }
    };
}

macro_rules! impl_idle_handler_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T> IdleHandler for $t
        where
            T: IdleHandler + ?Sized,
        {
            fn on_idle(&self) -> impl Future<Output = ()> + Send {
                (**self).on_idle()
            }
        }
    };
}

macro_rules! impl_runtime_store_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T> RuntimeStore for $t
        where
            T: RuntimeStore + ?Sized,
        {
            type Error = T::Error;

            fn agent(
                &self,
                agent_id: AgentId,
            ) -> impl Future<Output = Result<Option<Agent>, Self::Error>> + Send {
                (**self).agent(agent_id)
            }

            fn agent_state(
                &self,
                agent_id: AgentId,
            ) -> impl Future<Output = Result<Option<AgentState>, Self::Error>> + Send {
                (**self).agent_state(agent_id)
            }

            fn wakes(&self) -> impl Stream<Item = Result<Wake, Self::Error>> + Send {
                (**self).wakes()
            }

            fn events(
                &self,
                agent_id: AgentId,
                from: EventCursor,
                until: Option<EventCursor>,
            ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
                (**self).events(agent_id, from, until)
            }

            fn settle_interrupt(
                &self,
                agent_id: AgentId,
                expected_latest_event: Option<EventCursor>,
                tool_results: &[ToolResult],
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).settle_interrupt(agent_id, expected_latest_event, tool_results)
            }

            fn park(
                &self,
                agent_id: AgentId,
                expected_latest_event: Option<EventCursor>,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).park(agent_id, expected_latest_event)
            }

            fn consume_messages(
                &self,
                agent_id: AgentId,
                expected_latest_event: Option<EventCursor>,
                allow_queued: bool,
            ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
                (**self).consume_messages(agent_id, expected_latest_event, allow_queued)
            }

            fn publish_model_response_chunk(
                &self,
                agent_id: AgentId,
                input_cursor: EventCursor,
                sequence: u32,
                chunk: &ModelResponseChunk,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).publish_model_response_chunk(agent_id, input_cursor, sequence, chunk)
            }

            fn append_model_response(
                &self,
                agent_id: AgentId,
                expected_latest_event: EventCursor,
                response: &ModelResponse,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).append_model_response(agent_id, expected_latest_event, response)
            }

            fn prepare_tool_call(
                &self,
                agent_id: AgentId,
                call_id: &ToolCallId,
            ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
                (**self).prepare_tool_call(agent_id, call_id)
            }

            fn append_tool_result(
                &self,
                agent_id: AgentId,
                result: &ToolResult,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).append_tool_result(agent_id, result)
            }
        }
    };
}

macro_rules! impl_client_store_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T> ClientStore for $t
        where
            T: ClientStore + ?Sized,
        {
            type Error = T::Error;

            fn create_agent(
                &self,
                idempotency_key: &str,
                agent: &Agent,
                message: Option<&Message>,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).create_agent(idempotency_key, agent, message)
            }

            fn interrupt_agent(
                &self,
                idempotency_key: &str,
                agent_id: AgentId,
                message: Option<&Message>,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).interrupt_agent(idempotency_key, agent_id, message)
            }

            fn send_message(
                &self,
                idempotency_key: &str,
                agent_id: AgentId,
                message: &Message,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).send_message(idempotency_key, agent_id, message)
            }

            fn wait_until_idle(
                &self,
                agent_id: AgentId,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).wait_until_idle(agent_id)
            }
        }
    };
}

// === Impls ===

impl_roles_for_pointer!(&T);
impl_roles_for_pointer!(&mut T);
impl_roles_for_pointer!(Box<T>);
impl_roles_for_pointer!(Rc<T>);
impl_roles_for_pointer!(Arc<T>);

impl_models_for_pointer!(&T);
impl_models_for_pointer!(&mut T);
impl_models_for_pointer!(Box<T>);
impl_models_for_pointer!(Rc<T>);
impl_models_for_pointer!(Arc<T>);

impl_tools_for_pointer!(&T);
impl_tools_for_pointer!(&mut T);
impl_tools_for_pointer!(Box<T>);
impl_tools_for_pointer!(Rc<T>);
impl_tools_for_pointer!(Arc<T>);

impl_error_handler_for_pointer!(&T);
impl_error_handler_for_pointer!(&mut T);
impl_error_handler_for_pointer!(Box<T>);
impl_error_handler_for_pointer!(Rc<T>);
impl_error_handler_for_pointer!(Arc<T>);

impl_idle_handler_for_pointer!(&T);
impl_idle_handler_for_pointer!(&mut T);
impl_idle_handler_for_pointer!(Box<T>);
impl_idle_handler_for_pointer!(Rc<T>);
impl_idle_handler_for_pointer!(Arc<T>);

impl_runtime_store_for_pointer!(&T);
impl_runtime_store_for_pointer!(&mut T);
impl_runtime_store_for_pointer!(Box<T>);
impl_runtime_store_for_pointer!(Rc<T>);
impl_runtime_store_for_pointer!(Arc<T>);

impl_client_store_for_pointer!(&T);
impl_client_store_for_pointer!(&mut T);
impl_client_store_for_pointer!(Box<T>);
impl_client_store_for_pointer!(Rc<T>);
impl_client_store_for_pointer!(Arc<T>);

impl fmt::Display for ModelsErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidContinuation => f.write_str("invalid continuation"),
            Self::ContextTooLong => f.write_str("context too long"),
            Self::Other => f.write_str("other error"),
        }
    }
}

impl fmt::Display for ToolsErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeToRetry => f.write_str("unsafe to retry"),
            Self::Other => f.write_str("other error"),
        }
    }
}

impl fmt::Display for RuntimeStoreErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => f.write_str("conflict"),
            Self::Other => f.write_str("other error"),
        }
    }
}

impl fmt::Display for ClientStoreErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => f.write_str("conflict"),
            Self::IdempotencyConflict => f.write_str("idempotency conflict"),
            Self::AgentNotFound => f.write_str("agent not found"),
            Self::Other => f.write_str("other error"),
        }
    }
}
