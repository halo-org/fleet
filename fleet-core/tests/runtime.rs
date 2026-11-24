//! Tests for the runtime with simulated providers.

use core::assert_matches;
use fleet_core::{
    Agent, ClientStore as _, ClientStoreErrorKind, Configured, ContentBlock, Error as _,
    EventPayload, JsonObject, Message, MessageMode, ModelOutput, ModelProvenance, ModelResponse,
    ModelUsage, RoleName, RuntimeStore as _, StallReason, ToolCall, ToolsErrorKind, TurnEnd,
};
use fleet_simulator::{Scheduler, Simulator};
use std::collections::{BTreeMap, BTreeSet};

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
    for seed in 0..10 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let store = simulator.store.partition(partition);
        let agent = agent(simulator.roles.role_names()[0].clone());
        let tool_call = |id: &str, is_async| {
            ModelOutput::ToolCall(ToolCall {
                id: id.into(),
                tool: "missing".into(),
                is_async,
                input: JsonObject::new(),
            })
        };
        let response = ModelResponse {
            output: vec![tool_call("sync", false), tool_call("async", true)],
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
        };

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
            EventPayload::ToolResult(result) if result.call_id == "sync".into()
        );
        assert_eq!(
            snapshot.events[3].payload,
            EventPayload::TurnEnd(TurnEnd::Interrupted)
        );
        assert_matches!(
            &snapshot.events[4].payload,
            EventPayload::ToolResult(result) if result.call_id == "async".into()
        );
    }
}

#[test]
fn run_stalls_agents_on_errors_that_retrying_cannot_fix() {
    for seed in 0..10 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let agent = agent("missing".into());

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&message(0, MessageMode::Queue)))
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_eq!(
            snapshot.stalled,
            Some(StallReason::RoleNotFound {
                role: "missing".into()
            })
        );
    }
}

#[test]
fn run_retries_stalled_agents_only_after_they_are_resumed() {
    for seed in 0..10 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let mut simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let agent = agent("missing".into());

        scheduler.run(async {
            simulator
                .store
                .create_agent("create", &agent, Some(&message(0, MessageMode::Queue)))
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let stalled = simulator.store.snapshot().remove(&agent.id).unwrap();
        let name = simulator.roles.role_names()[0].clone();
        let mut role = simulator.roles.role(&name).unwrap().clone();
        role.name = "missing".into();
        simulator.roles.insert(role);

        scheduler.run(simulator.run(partition));
        assert_eq!(simulator.store.snapshot()[&agent.id], stalled);

        scheduler.run(async {
            simulator
                .store
                .resume_agent("resume", agent.id)
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_eq!(snapshot.stalled, None);
        assert_eq!(
            snapshot.events.last().unwrap().payload,
            EventPayload::TurnEnd(TurnEnd::Completed)
        );
    }
}

#[test]
fn run_stalls_agents_on_tool_errors_until_they_are_resumed() {
    for seed in 0..10 {
        println!("seed: {seed}");

        let scheduler = Scheduler::new(seed);
        scheduler.disable_faults();
        let mut simulator = Simulator::new(&scheduler, seed);
        let partition = simulator.store.add_partition();
        let store = simulator.store.partition(partition);
        let Some(tool) = simulator.tools.tool_names().first().cloned() else {
            continue;
        };
        let agent = agent(simulator.roles.role_names()[0].clone());
        let mut role = simulator.roles.role(&agent.role.name).unwrap().clone();
        let tool_config = Configured {
            name: tool.clone(),
            config: JsonObject::new(),
        };
        role.tools.insert("tool".into(), tool_config);
        simulator.roles.insert(role);
        let response = ModelResponse {
            output: vec![ModelOutput::ToolCall(ToolCall {
                id: "call".into(),
                tool: "tool".into(),
                is_async: false,
                input: JsonObject::from_iter([("input".into(), "input".into())]),
            })],
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
        };
        simulator
            .tools
            .set_error(&tool, Some(ToolsErrorKind::Rejected));

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

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_eq!(
            snapshot.stalled,
            Some(StallReason::ToolError { tool: tool.clone() })
        );
        assert_eq!(snapshot.events.len(), 2);

        simulator.tools.set_error(&tool, None);
        scheduler.run(async {
            simulator
                .store
                .resume_agent("resume", agent.id)
                .await
                .unwrap();

            simulator.run(partition).await;
        });

        let snapshot = simulator.store.snapshot().remove(&agent.id).unwrap();
        assert_matches!(
            &snapshot.events[2].payload,
            EventPayload::ToolResult(result) if result.call_id == "call".into()
        );
        assert_eq!(
            snapshot.events.last().unwrap().payload,
            EventPayload::TurnEnd(TurnEnd::Completed)
        );
    }
}
