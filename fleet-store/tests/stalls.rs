//! Tests for stalled agents and their ancestors' counts.

#![cfg(test)]
#![cfg(feature = "subagents")]

use core::{assert_matches, pin::Pin, pin::pin, time::Duration};
use fleet_core::{
    Agent, AgentId, AgentState, ClientStore as _, Configured, JsonObject, Message, MessageMode,
    ModelOutput, ModelProvenance, ModelResponse, ModelUsage, RuntimeStore as _, StallReason,
    ToolCall, ToolOutput, ToolResult, Wake,
};
use fleet_kit::{AgentCollection as _, Agents as _, AgentsQuery, Queries as _, QueryUpdate};
use fleet_store::{Store, StoreError};
use futures_util::{FutureExt as _, StreamExt as _};
use serde_json::json;

fn role() -> Configured<fleet_core::RoleName> {
    Configured {
        name: "assistant".into(),
        config: JsonObject::new(),
    }
}

fn reason() -> StallReason {
    StallReason::RoleNotFound {
        role: "assistant".into(),
    }
}

fn message(id: u128) -> Message {
    Message {
        id: id.into(),
        mode: MessageMode::Queue,
        content: vec![],
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
            provider_id: "test".into(),
            model_id: "test".into(),
        },
        continuation_token: None,
    }
}

/// Consumes the agent's messages and answers with an async `drive` call.
async fn drive(store: &Store, id: AgentId) {
    let call = ModelOutput::ToolCall(ToolCall {
        id: "drive".into(),
        tool: "drive".into(),
        is_async: true,
        input: JsonObject::new(),
    });
    let state = store.agent_state(id).await.unwrap().unwrap();
    store
        .consume_messages(id, state.latest_event, true)
        .await
        .unwrap();
    let state = store.agent_state(id).await.unwrap().unwrap();
    store
        .append_model_response(id, state.latest_event.unwrap(), true, &response(vec![call]))
        .await
        .unwrap();
}

/// Creates a root agent, a child driven by its `drive` call, and a grandchild driven by the
/// child's, and stalls the grandchild mid-turn.
async fn chain(store: &Store) -> [AgentId; 3] {
    let root = AgentId::from(1);
    let agent = Agent {
        id: root,
        role: role(),
        extensions: JsonObject::new(),
    };
    store
        .create_agent("root", &agent, Some(&message(1)))
        .await
        .unwrap();
    drive(store, root).await;

    let child = create(store, root).await;
    drive(store, child).await;
    let grandchild = create(store, child).await;
    store
        .consume_messages(grandchild, None, true)
        .await
        .unwrap();
    stall(store, grandchild, &reason()).await.unwrap();
    [root, child, grandchild]
}

/// Stalls an agent at its current state.
async fn stall(store: &Store, id: AgentId, reason: &StallReason) -> Result<(), StoreError> {
    let state = store.agent_state(id).await.unwrap().unwrap();
    store
        .stall(id, state.latest_event, state.stall_token, reason)
        .await
}

/// Creates an agent driven by the parent's `drive` call and sends it a message.
async fn create(store: &Store, parent: AgentId) -> AgentId {
    let access = store.agent_access(parent).for_call("drive".into());
    let id = access
        .create("agent", &role(), &JsonObject::new())
        .await
        .unwrap();
    access.send("message", id, &[]).await.unwrap();
    id
}

/// Returns each agent's stall reason and stalled descendant count from its activity and summary.
async fn stalls(store: &Store, ids: [AgentId; 3]) -> Vec<(Option<StallReason>, u64)> {
    let mut stalls = Vec::new();
    for (index, id) in ids.into_iter().enumerate() {
        let QueryUpdate::Changed(activity) = store.activity(id, None).await.unwrap().value else {
            panic!("activity is returned without a revision");
        };
        let query = AgentsQuery {
            parent: index.checked_sub(1).map(|parent| ids[parent]),
            ..AgentsQuery::default()
        };
        let page = store.agents(query).await.unwrap();
        let summary = page
            .agents
            .into_iter()
            .find(|agent| agent.id == id)
            .unwrap();
        assert_eq!(
            (&summary.stalled, summary.stalled_descendants),
            (&activity.stalled, activity.stalled_descendants)
        );
        stalls.push((activity.stalled, activity.stalled_descendants));
    }
    stalls
}

fn no_stalls() -> Vec<(Option<StallReason>, u64)> {
    vec![(None, 0), (None, 0), (None, 0)]
}

#[test]
fn stalling_conflicts_with_newer_state() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        let [root, child, grandchild] = chain(&store).await;
        let access = store.agent_access(root).for_call("drive".into());
        let state = async |id| store.agent_state(id).await.unwrap().unwrap();
        let conflicts = async |id, before: AgentState| {
            let error = store
                .stall(id, before.latest_event, before.stall_token, &reason())
                .await
                .unwrap_err();
            assert_matches!(error, StoreError::Conflict(_));
        };

        let before = state(grandchild).await;
        store.resume_agent("resume", grandchild).await.unwrap();
        conflicts(grandchild, before).await;

        let before = state(grandchild).await;
        store
            .resume_agent("resume-again", grandchild)
            .await
            .unwrap();
        store
            .stall(
                grandchild,
                before.latest_event,
                before.stall_token,
                &reason(),
            )
            .await
            .unwrap();

        let before = state(child).await;
        access.send("send", child, &[]).await.unwrap();
        conflicts(child, before).await;

        let before = state(child).await;
        store
            .interrupt_tool_calls("interrupt", child, &["drive".into()])
            .await
            .unwrap();
        conflicts(child, before).await;

        let before = state(root).await;
        store
            .interrupt_agent("interrupt-root", root, false, None)
            .await
            .unwrap();
        conflicts(root, before).await;

        let before = state(grandchild).await;
        store
            .append_model_response(grandchild, 0.into(), true, &response(vec![]))
            .await
            .unwrap();
        conflicts(grandchild, before).await;
    });
}

#[test]
fn ending_a_scope_conflicts_with_stalling_its_agent() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        let [root, child, _] = chain(&store).await;
        let access = store.agent_access(root).for_call("drive".into());
        let state = store.agent_state(child).await.unwrap().unwrap();
        store.park(child, state.latest_event).await.unwrap();
        access.send("queue", child, &[]).await.unwrap();

        let before = store.agent_state(child).await.unwrap().unwrap();
        store
            .interrupt_tool_calls("interrupt", root, &["drive".into()])
            .await
            .unwrap();
        store.reconcile_agent_scopes().await.unwrap();

        let error = store
            .stall(child, before.latest_event, before.stall_token, &reason())
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::Conflict(_));
    });
}

#[test]
fn parents_created_after_their_stalled_children_count_them() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        let agent = |id: u128, parent: Option<u128>| Agent {
            id: id.into(),
            role: role(),
            extensions: parent.map_or_else(JsonObject::new, |parent| {
                JsonObject::from_iter([(
                    "fleet.parent".into(),
                    json!({ "agent": parent.to_string(), "call": "drive" }),
                )])
            }),
        };
        store
            .create_agent("child", &agent(2, Some(1)), None)
            .await
            .unwrap();
        stall(&store, 2.into(), &reason()).await.unwrap();
        store
            .create_agent("root", &agent(1, Some(0)), None)
            .await
            .unwrap();
        store
            .create_agent("ancestor", &agent(0, None), None)
            .await
            .unwrap();

        let counts = async || {
            let mut counts = Vec::new();
            for id in [0, 1] {
                let QueryUpdate::Changed(activity) =
                    store.activity(id.into(), None).await.unwrap().value
                else {
                    panic!("activity is returned without a revision");
                };
                counts.push(activity.stalled_descendants);
            }
            counts
        };
        assert_eq!(counts().await, [1, 1]);

        store.resume_subtree("resume", 0.into()).await.unwrap();
        assert_eq!(counts().await, [0, 0]);
        assert!(
            !store
                .agent_state(2.into())
                .await
                .unwrap()
                .unwrap()
                .is_stalled
        );
    });
}

#[test]
fn stalled_agents_are_counted_by_their_ancestors() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        let ids = chain(&store).await;
        let state = store.agent_state(ids[1]).await.unwrap().unwrap();

        assert!(store.agent_state(ids[2]).await.unwrap().unwrap().is_stalled);
        assert_eq!(
            stalls(&store, ids).await,
            [(None, 1), (None, 1), (Some(reason()), 0)]
        );

        let reason = StallReason::ModelError {
            model: "model".into(),
        };
        store
            .stall(ids[1], state.latest_event, state.stall_token, &reason)
            .await
            .unwrap();
        assert_eq!(
            stalls(&store, ids).await,
            [(None, 2), (Some(reason), 1), (Some(self::reason()), 0)]
        );

        store.resume_subtree("resume", ids[0]).await.unwrap();
        assert_eq!(stalls(&store, ids).await, no_stalls());
    });
}

#[test]
fn clearing_stalls_and_ending_scopes_uncount_stalled_agents() {
    #[derive(Clone, Copy)]
    enum Path {
        Message,
        Interrupt,
        InterruptToolCalls,
        Resume,
        ResumeSubtree,
        CompleteDrivingCall,
        InterruptDrivingCall,
    }

    let paths = [
        Path::Message,
        Path::Interrupt,
        Path::InterruptToolCalls,
        Path::Resume,
        Path::ResumeSubtree,
        Path::CompleteDrivingCall,
        Path::InterruptDrivingCall,
    ];

    for (index, path) in paths.into_iter().enumerate() {
        println!("path: {index}");

        futures_executor::block_on(async {
            let store = Store::memory().unwrap();
            let [root, child, grandchild] = chain(&store).await;

            match path {
                Path::Message => {
                    let access = store.agent_access(child).for_call("drive".into());
                    access.send("send", grandchild, &[]).await.unwrap();
                }
                Path::Interrupt => store
                    .interrupt_agent("interrupt", grandchild, false, None)
                    .await
                    .unwrap(),
                Path::InterruptToolCalls => store
                    .interrupt_tool_calls("interrupt", grandchild, &[])
                    .await
                    .unwrap(),
                Path::Resume => store.resume_agent("resume", grandchild).await.unwrap(),
                Path::ResumeSubtree => store.resume_subtree("resume", root).await.unwrap(),
                Path::CompleteDrivingCall => {
                    let result = ToolResult {
                        call_id: "drive".into(),
                        output: ToolOutput {
                            content: vec![],
                            structured_content: None,
                            is_error: false,
                        },
                    };
                    store.append_tool_result(child, &result).await.unwrap();
                    store.reconcile_agent_scopes().await.unwrap();
                }
                Path::InterruptDrivingCall => {
                    store
                        .interrupt_tool_calls("interrupt", child, &["drive".into()])
                        .await
                        .unwrap();
                    store.reconcile_agent_scopes().await.unwrap();
                }
            }

            let ids = [root, child, grandchild];
            assert_eq!(stalls(&store, ids).await, no_stalls());
            assert!(
                !store
                    .agent_state(grandchild)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_stalled
            );
        });
    }
}

#[test]
fn ancestors_receive_their_stalled_descendant_counts_live() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        let ids = chain(&store).await;
        store.resume_agent("resume", ids[2]).await.unwrap();
        let root = store.activity(ids[0], None).await.unwrap().revision;
        let collection = store.collection_revision().await.unwrap();
        let mut updates = pin!(store.updates(ids[0], root));
        let mut agents = pin!(store.agents_updates(collection));
        assert!(updates.as_mut().now_or_never().is_none());
        assert!(agents.as_mut().now_or_never().is_none());

        stall(&store, ids[2], &reason()).await.unwrap();

        let updates = ready(updates);
        assert_matches!(
            updates.activity,
            QueryUpdate::Changed(activity) if activity.stalled_descendants == 1
        );
        let mut agents = ready(agents);
        let summary = loop {
            if let Some(summary) = agents.agents.iter().find(|agent| agent.id == ids[0]) {
                break summary.clone();
            }
            agents = store.agents_updates(agents.revision).await.unwrap();
        };
        assert_eq!(summary.stalled_descendants, 1);
    });
}

/// Waits a bounded time for a subscription that is already registered.
fn ready<T, E: core::fmt::Debug>(mut future: Pin<&mut impl Future<Output = Result<T, E>>>) -> T {
    for _ in 0..500 {
        if let Some(result) = future.as_mut().now_or_never() {
            return result.unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("subscription was not notified");
}

#[test]
fn stalled_agents_stay_stalled_and_asleep_after_reopening() {
    let path = std::env::temp_dir().join(format!("fleet-stalls-{}.redb", std::process::id()));

    futures_executor::block_on(async {
        let ids = chain(&Store::open(&path).unwrap()).await;
        let store = Store::open(&path).unwrap();
        let wakes = store
            .wakes()
            .take_while(|wake| core::future::ready(!matches!(wake, Ok(Wake::Idle))))
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .await;

        assert!(!wakes.contains(&Wake::Reconcile(ids[2])));
        assert!(wakes.contains(&Wake::Reconcile(ids[1])));
        assert_eq!(
            stalls(&store, ids).await,
            [(None, 1), (None, 1), (Some(reason()), 0)]
        );
    });

    std::fs::remove_file(path).unwrap();
}
