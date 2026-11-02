//! Tests for the runtime with simulated providers.

use core::assert_matches;
use fleet_core::{
    Agent, ClientStore as _, ClientStoreErrorKind, Configured, ContentBlock, Error as _,
    EventPayload, JsonObject, Message, MessageMode, ModelOutput, RoleName, TurnEnd,
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

fn message() -> Message {
    Message {
        id: 0.into(),
        mode: MessageMode::Queue,
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
        let message = message();

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
