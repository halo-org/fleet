//! Tests for the runtime with simulated providers.

use core::{
    assert_matches, mem,
    task::{Poll, Waker},
};
use fleet_core::{
    Agent, AgentId, ClientStore as _, ClientStoreErrorKind, Configured, ContentBlock, Error as _,
    EventPayload, JsonObject, Message, MessageMode, ModelOutput, ModelProvenance, ModelResponse,
    ModelUsage, RoleName, Roles as _, RuntimeStore as _, Tool, ToolAlias, ToolCall, ToolCallId,
    ToolContext, ToolName, ToolOutput, Tools, TurnEnd,
};
use fleet_simulator::{Scheduler, Simulator, ToolsSimulator, ToolsSimulatorError};
use futures_util::future;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

/// Tools that never finish calls with IDs starting with `stuck`.
///
/// Interrupted outputs for those calls wait while interrupts are held.
struct StuckTools<'sim> {
    tools: &'sim ToolsSimulator<'sim>,
    state: Mutex<StuckState>,
}

#[derive(Default)]
struct StuckState {
    running: BTreeSet<ToolCallId>,
    interrupted: Vec<ToolCallId>,
    holds_interrupts: bool,
    wakers: Vec<Waker>,
}

struct Running<'a, 'sim> {
    tools: &'a StuckTools<'sim>,
    call_id: ToolCallId,
}

impl<'sim> StuckTools<'sim> {
    fn new(tools: &'sim ToolsSimulator<'sim>) -> Self {
        Self {
            tools,
            state: Mutex::default(),
        }
    }

    fn update(&self, update: impl FnOnce(&mut StuckState)) {
        let mut state = self.state.lock().expect("never poisoned");
        update(&mut state);
        state.wakers.drain(..).for_each(Waker::wake);
    }

    async fn wait_until(&self, condition: impl Fn(&StuckState) -> bool + Send) {
        future::poll_fn(move |cx| {
            let mut state = self.state.lock().expect("never poisoned");

            if condition(&state) {
                Poll::Ready(())
            } else {
                state.wakers.push(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }
}

impl Tools for StuckTools<'_> {
    type Error = ToolsSimulatorError;

    async fn get(&self, name: &ToolName) -> Result<Option<Tool>, Self::Error> {
        self.tools.get(name).await
    }

    async fn execute(
        &self,
        context: &ToolContext,
        is_retry: bool,
        input: &JsonObject,
    ) -> Result<ToolOutput, Self::Error> {
        if !context.call_id.as_ref().starts_with("stuck") {
            return self.tools.execute(context, is_retry, input).await;
        }

        self.update(|state| {
            state.running.insert(context.call_id.clone());
        });
        let _running = Running {
            tools: self,
            call_id: context.call_id.clone(),
        };
        future::pending().await
    }

    async fn interrupted_output(
        &self,
        context: &ToolContext,
        input: &JsonObject,
    ) -> Result<ToolOutput, Self::Error> {
        if context.call_id.as_ref().starts_with("stuck") {
            self.update(|state| state.interrupted.push(context.call_id.clone()));
            self.wait_until(|state| !state.holds_interrupts).await;
        }

        self.tools.interrupted_output(context, input).await
    }

    async fn unknown_tool_output(
        &self,
        agent_id: AgentId,
        call: &ToolCall,
    ) -> Result<ToolOutput, Self::Error> {
        self.tools.unknown_tool_output(agent_id, call).await
    }
}

impl Drop for Running<'_, '_> {
    fn drop(&mut self) {
        self.tools.update(|state| {
            state.running.remove(&self.call_id);
        });
    }
}

fn agent(role: RoleName) -> Agent {
    Agent {
        id: 0.into(),
        role: Configured {
            name: role,
            config: JsonObject::new(),
        },
        extensions: JsonObject::new(),
    }
}

fn message(id: u128, mode: MessageMode) -> Message {
    Message {
        id: id.into(),
        mode,
        content: vec![ContentBlock::Text("Hello".into())],
    }
}

fn response(output: Vec<ModelOutput>) -> ModelResponse {
    ModelResponse {
        output,
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

fn tool_call(id: &str, tool: &ToolAlias, is_async: bool) -> ModelOutput {
    ModelOutput::ToolCall(ToolCall {
        id: id.into(),
        tool: tool.clone(),
        is_async,
        input: JsonObject::from_iter([("input".into(), "Hello".into())]),
    })
}

async fn role_with_tool(simulator: &Simulator<'_>) -> Option<(RoleName, ToolAlias)> {
    for name in simulator.roles.role_names() {
        let role = simulator
            .roles
            .get(&name)
            .await
            .expect("faults are disabled")
            .expect("role exists");

        if let Some(alias) = role.tools.into_keys().next() {
            return Some((name, alias));
        }
    }

    None
}

#[test]
fn run_returns_when_store_is_empty() {
    for seed in 0..10 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();

        scheduler.run(simulator.run(partition));
        assert_eq!(simulator.store.snapshot(), BTreeMap::new());
    }
}

#[test]
fn run_processes_a_message() {
    for seed in 0..10 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let agent = agent(simulator.roles.role_names()[0].clone());
        let message = message(0, MessageMode::Queue);

        scheduler.run(async {
            while let Err(error) = simulator
                .store
                .create_agent("create", &agent, Some(&message))
                .await
            {
                assert_eq!(error.kind(), ClientStoreErrorKind::Other);
            }

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        let mut inputs = Vec::new();
        let mut last_response = None;
        let mut tool_calls = BTreeSet::new();
        let mut tool_results = BTreeSet::new();

        for event in &snapshot.events {
            match &event.payload {
                EventPayload::Input(id, content) => inputs.push((id, content)),
                EventPayload::ModelResponse(response) => {
                    last_response = Some(response);

                    for output in &response.output {
                        if let ModelOutput::ToolCall(tool_call) = output {
                            assert!(tool_calls.insert(&tool_call.id));
                        }
                    }
                }
                EventPayload::ToolResult(tool_result) => {
                    assert!(tool_calls.contains(&tool_result.call_id));
                    assert!(tool_results.insert(&tool_result.call_id));
                }
                EventPayload::Compaction(_) => {}
                EventPayload::TurnEnd(turn_end) => assert_eq!(*turn_end, TurnEnd::Completed),
            }
        }

        let last_response = last_response.unwrap();
        assert_eq!(inputs, [(&message.id, &message.content)]);
        assert_matches!(&snapshot.events[0].payload, EventPayload::Input(..));
        assert!(
            last_response
                .output
                .iter()
                .all(|output| !matches!(output, ModelOutput::ToolCall(_)))
        );
        assert_eq!(tool_calls, tool_results);
        assert_eq!(snapshot.unresolved_tool_calls, BTreeMap::new());
        assert_eq!(snapshot.pending_messages, []);
        assert_eq!(
            snapshot.parked_at,
            snapshot.events.last().map(|event| event.cursor)
        );
    }
}

#[test]
fn run_finishes_steering_turn_before_queued_message() {
    for seed in 0..10 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let agent = agent(simulator.roles.role_names()[0].clone());
        let steering_a = message(0, MessageMode::Steer);
        let steering_b = message(1, MessageMode::Steer);
        let queued = message(2, MessageMode::Queue);

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&steering_a))
                .await
                .unwrap();
            simulator
                .store
                .send_message("steer", agent.id, &steering_b)
                .await
                .unwrap();
            simulator
                .store
                .send_message("queue", agent.id, &queued)
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        let mut first_turn_finished = false;
        let inputs = snapshot
            .events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::Input(id, _) => Some((*id, first_turn_finished)),
                EventPayload::TurnEnd(_) => {
                    first_turn_finished = true;
                    None
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            inputs,
            [
                (steering_a.id, false),
                (steering_b.id, false),
                (queued.id, true)
            ]
        );
        assert_eq!(snapshot.pending_messages, []);
    }
}

#[test]
fn run_defers_compaction_after_an_interrupt_until_a_message_arrives() {
    for seed in 0..10 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let store = simulator.store.partition(partition);
        let agent = agent(simulator.roles.role_names()[0].clone());
        let first_message = message(0, MessageMode::Queue);
        let next_message = message(1, MessageMode::Queue);
        let response = ModelResponse {
            output: vec![ModelOutput::Text("Done".into())],
            usage: ModelUsage {
                input: u32::MAX,
                output: 1,
                cache_read: 0,
                cache_write: 0,
            },
            provenance: ModelProvenance {
                provider_id: "provider".into(),
                model_id: "model".into(),
            },
            continuation_token: None,
        };

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&first_message))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .append_model_response(agent.id, 0.into(), true, &response)
                .await
                .unwrap();
            simulator
                .store
                .interrupt_agent("interrupt", agent.id, true, None)
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_eq!(snapshot.events.len(), 4);
        assert_eq!(
            snapshot.events[3].payload,
            EventPayload::TurnEnd(TurnEnd::Interrupted)
        );
        assert_eq!(snapshot.parked_at, Some(3.into()));
        assert!(!snapshot.interrupt_intent);

        scheduler.run(async {
            simulator
                .store
                .send_message("send", agent.id, &next_message)
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_matches!(
            &snapshot.events[4].payload,
            EventPayload::Compaction(compaction) if compaction.last_event == 3.into()
        );
        assert_eq!(
            snapshot.events[5].payload,
            EventPayload::Input(next_message.id, next_message.content)
        );
        assert_eq!(snapshot.pending_messages, []);
    }
}

#[test]
fn run_interrupts_a_turn_without_its_async_tool_calls() {
    for seed in 0..50 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let store = simulator.store.partition(partition);
        let Some((role, tool)) = scheduler.run(role_with_tool(&simulator)) else {
            continue;
        };
        let agent = agent(role);
        let response = response(vec![
            tool_call("sync", &tool, false),
            tool_call("async", &tool, true),
        ]);

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&message(0, MessageMode::Queue)))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .append_model_response(agent.id, 0.into(), false, &response)
                .await
                .unwrap();
            simulator
                .store
                .interrupt_agent("interrupt", agent.id, false, None)
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_matches!(
            &snapshot.events[2].payload,
            EventPayload::ToolResult(result)
                if result.call_id == "sync".into() && result.output.is_error
        );
        assert_eq!(
            snapshot.events[3].payload,
            EventPayload::TurnEnd(TurnEnd::Interrupted)
        );
        assert_matches!(
            &snapshot.events[4].payload,
            EventPayload::ToolResult(result)
                if result.call_id == "async".into() && !result.output.is_error
        );
    }
}

#[test]
fn run_interrupts_a_tool_call_without_interrupting_the_turn() {
    for seed in 0..50 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let store = simulator.store.partition(partition);
        let Some((role, tool)) = scheduler.run(role_with_tool(&simulator)) else {
            continue;
        };
        let agent = agent(role);
        let response = response(vec![tool_call("sync", &tool, false)]);

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&message(0, MessageMode::Queue)))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .append_model_response(agent.id, 0.into(), false, &response)
                .await
                .unwrap();
            simulator
                .store
                .interrupt_tool_calls("interrupt", agent.id, &["sync".into()])
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_matches!(
            &snapshot.events[2].payload,
            EventPayload::ToolResult(result)
                if result.call_id == "sync".into() && result.output.is_error
        );
        assert_matches!(&snapshot.events[3].payload, EventPayload::ModelResponse(_));
        assert!(
            snapshot
                .events
                .iter()
                .all(|event| event.payload != EventPayload::TurnEnd(TurnEnd::Interrupted))
        );
    }
}

#[test]
fn run_continues_a_parked_agent_after_interrupting_a_tool_call() {
    for seed in 0..50 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let store = simulator.store.partition(partition);
        let Some((role, tool)) = scheduler.run(role_with_tool(&simulator)) else {
            continue;
        };
        let agent = agent(role);
        let response = response(vec![tool_call("async", &tool, true)]);

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&message(0, MessageMode::Queue)))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .append_model_response(agent.id, 0.into(), true, &response)
                .await
                .unwrap();
            store.park(agent.id, Some(2.into())).await.unwrap();
            simulator
                .store
                .interrupt_tool_calls("interrupt", agent.id, &["async".into()])
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_matches!(
            &snapshot.events[3].payload,
            EventPayload::ToolResult(result)
                if result.call_id == "async".into() && result.output.is_error
        );
        assert_matches!(&snapshot.events[4].payload, EventPayload::ModelResponse(_));
    }
}

#[test]
fn run_stops_an_interrupted_tool_call_while_others_run() {
    for seed in 0..50 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let store = simulator.store.partition(partition);
        let Some((role, tool)) = scheduler.run(role_with_tool(&simulator)) else {
            continue;
        };
        let agent = agent(role);
        let response = response(vec![
            tool_call("stuck-async", &tool, true),
            tool_call("stuck-sync", &tool, false),
        ]);
        let tools = StuckTools::new(&simulator.tools);

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&message(0, MessageMode::Queue)))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .append_model_response(agent.id, 0.into(), false, &response)
                .await
                .unwrap();

            let run = fleet_core::run(
                &simulator.idle_handler,
                &simulator.error_handler,
                store,
                &simulator.roles,
                &simulator.models,
                &tools,
            );
            let interrupt = async {
                tools.wait_until(|state| state.running.len() == 2).await;
                simulator
                    .store
                    .interrupt_tool_calls("sync", agent.id, &["stuck-sync".into()])
                    .await
                    .unwrap();
                tools
                    .wait_until(|state| state.running == BTreeSet::from(["stuck-async".into()]))
                    .await;
                simulator
                    .store
                    .interrupt_tool_calls("async", agent.id, &["stuck-async".into()])
                    .await
                    .unwrap();
            };

            future::join(run, interrupt).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        let interrupted = snapshot
            .events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::ToolResult(result)
                    if result.call_id.as_ref().starts_with("stuck") =>
                {
                    Some((result.call_id.as_ref(), result.output.is_error))
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            interrupted,
            BTreeSet::from([("stuck-async", true), ("stuck-sync", true)])
        );
        assert!(
            snapshot
                .events
                .iter()
                .all(|event| event.payload != EventPayload::TurnEnd(TurnEnd::Interrupted))
        );
    }
}

#[test]
fn run_reuses_a_pending_interrupted_output_for_a_turn_interrupt() {
    for seed in 0..50 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        scheduler.disable_suspensions();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let store = simulator.store.partition(partition);
        let Some((role, tool)) = scheduler.run(role_with_tool(&simulator)) else {
            continue;
        };
        let agent = agent(role);
        let response = response(vec![tool_call("stuck", &tool, false)]);
        let tools = StuckTools::new(&simulator.tools);
        tools.update(|state| state.holds_interrupts = true);

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&message(0, MessageMode::Queue)))
                .await
                .unwrap();
            store.consume_messages(agent.id, None, true).await.unwrap();
            store
                .append_model_response(agent.id, 0.into(), false, &response)
                .await
                .unwrap();

            let run = fleet_core::run(
                &simulator.idle_handler,
                &simulator.error_handler,
                store,
                &simulator.roles,
                &simulator.models,
                &tools,
            );
            let interrupt = async {
                tools.wait_until(|state| !state.running.is_empty()).await;
                simulator
                    .store
                    .interrupt_tool_calls("call", agent.id, &["stuck".into()])
                    .await
                    .unwrap();
                tools
                    .wait_until(|state| !state.interrupted.is_empty())
                    .await;
                simulator
                    .store
                    .interrupt_agent("turn", agent.id, false, None)
                    .await
                    .unwrap();

                for _ in 0..100 {
                    let mut is_yielded = false;
                    future::poll_fn(|cx| {
                        if mem::replace(&mut is_yielded, true) {
                            Poll::Ready(())
                        } else {
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                    })
                    .await;
                }

                tools.update(|state| state.holds_interrupts = false);
            };

            future::join(run, interrupt).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        let interrupted = tools.state.lock().unwrap().interrupted.clone();
        assert_eq!(interrupted, [ToolCallId::from("stuck")]);
        assert_matches!(
            &snapshot.events[2].payload,
            EventPayload::ToolResult(result)
                if result.call_id == "stuck".into() && result.output.is_error
        );
        assert_eq!(
            snapshot.events[3].payload,
            EventPayload::TurnEnd(TurnEnd::Interrupted)
        );
    }
}
