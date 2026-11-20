//! Exercises the shipped agent and workspace tools through their component interfaces.

#![cfg(test)]
#![expect(
    clippy::significant_drop_tightening,
    reason = "providers remain available through execution and cleanup"
)]

use core::{convert::Infallible, future::ready, time::Duration};
use fleet_core::{
    Agent, AgentId, ClientStore as _, Configured, ContentBlock, Error as _, JsonObject, Message,
    MessageMode, ModelOutput, ModelProvenance, ModelResponse, ModelUsage, Role, RuntimeStore as _,
    ToolCall, ToolContext, ToolResult, Tools as _, ToolsErrorKind,
};
use fleet_kit::{
    AgentCollection as _, AgentsQuery, Blobs as _, Queries as _, RoleStore as _, Workspace,
    WorkspaceCommand, WorkspaceContext, WorkspaceName, WorkspaceOutput, WorkspaceResolution,
    WorkspaceResolvers, WorkspaceWrite, Workspaces,
};
use fleet_store::Store;
use fleet_wasm::{
    Wasm, WasmAgents, WasmBlobs, WasmCapabilities, WasmComponent, WasmError, WasmRegistry,
    WasmRoles, WasmWorkspaceAccess,
};
use fleet_workspaces::AgentWorkspaces;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};

const IMAGE: &[u8] = include_bytes!("../../fleet-store/tests/fixtures/image.png");

/// Loads the plugins from `FLEET_TEST_WASM`, or builds them if that is unset.
fn components(wasm: &Wasm) -> (WasmComponent, WasmComponent) {
    static MODULES: OnceLock<[Vec<u8>; 2]> = OnceLock::new();
    let modules = MODULES.get_or_init(|| {
        let directory = std::env::var_os("FLEET_TEST_WASM").map_or_else(build, PathBuf::from);
        ["agents", "tools"].map(|plugin| {
            std::fs::read(directory.join(format!("fleet_plugin_{plugin}.wasm"))).unwrap()
        })
    });
    (
        wasm.compile(&modules[0]).unwrap(),
        wasm.compile(&modules[1]).unwrap(),
    )
}

fn build() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let target = root.join("target/subagent-fixture");
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "build",
            "--offline",
            "--locked",
            "-p",
            "fleet-plugin-agents",
            "-p",
            "fleet-plugin-tools",
            "--target",
            "wasm32-unknown-unknown",
            "--config",
            "profile.dev.opt-level=1",
        ])
        .arg("--target-dir")
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    target.join("wasm32-unknown-unknown/debug")
}

#[derive(Clone)]
struct Providers {
    registry: WasmRegistry,
    reads: Arc<Mutex<Vec<(WorkspaceContext, String)>>>,
    writes: Arc<Mutex<Vec<(WorkspaceContext, WorkspaceWrite)>>>,
}

impl WorkspaceResolvers for Providers {
    type Error = WasmError;
    fn get(
        &self,
        name: &WorkspaceName,
    ) -> impl Future<Output = Result<Option<Workspace>, Self::Error>> + Send {
        WorkspaceResolvers::get(&self.registry, name)
    }
    fn resolve(
        &self,
        subject: AgentId,
        workspace: &Configured<WorkspaceName>,
    ) -> impl Future<Output = Result<WorkspaceResolution, Self::Error>> + Send {
        self.registry.resolve(subject, workspace)
    }
}

impl Workspaces for Providers {
    type Error = Infallible;
    fn get(
        &self,
        name: &WorkspaceName,
    ) -> impl Future<Output = Result<Option<Workspace>, Self::Error>> + Send {
        ready(Ok(Some(Workspace {
            name: name.clone(),
            extensions: JsonObject::new(),
        })))
    }
    fn open(&self, _: &WorkspaceContext) -> impl Future<Output = Result<(), Self::Error>> + Send {
        ready(Ok(()))
    }
    fn read(
        &self,
        context: &WorkspaceContext,
        path: &str,
        _: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        self.reads
            .lock()
            .unwrap()
            .push((context.clone(), path.into()));
        ready(Ok(b"hello".to_vec()))
    }
    fn write(
        &self,
        context: &WorkspaceContext,
        write: &WorkspaceWrite,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.writes
            .lock()
            .unwrap()
            .push((context.clone(), write.clone()));
        ready(Ok(()))
    }
    fn exec(
        &self,
        _: &WorkspaceContext,
        _: bool,
        _: &WorkspaceCommand,
    ) -> impl Future<Output = Result<WorkspaceOutput, Self::Error>> + Send {
        core::future::pending()
    }
}

fn object(value: serde_json::Value) -> JsonObject {
    let serde_json::Value::Object(object) = value else {
        panic!("expected an object")
    };
    object
}
fn configured<N: From<&'static str>>(name: &'static str) -> Configured<N> {
    Configured {
        name: name.into(),
        config: JsonObject::new(),
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
fn call(id: &str) -> ModelOutput {
    ModelOutput::ToolCall(ToolCall {
        id: id.into(),
        tool: "delegate".into(),
        is_async: true,
        input: JsonObject::new(),
    })
}

fn providers(
    wasm: &Wasm,
    agents_component: &WasmComponent,
    tools_component: &WasmComponent,
    store: &Store,
    reads: &Arc<Mutex<Vec<(WorkspaceContext, String)>>>,
    writes: &Arc<Mutex<Vec<(WorkspaceContext, WorkspaceWrite)>>>,
) -> (WasmRegistry, WasmRegistry) {
    let agent_store = store.clone();
    let role_store = store.clone();
    let blob_store = store.clone();
    let provider = wasm
        .provider(
            agents_component,
            &JsonObject::new(),
            WasmCapabilities {
                agents: Some(WasmAgents::new(move |id, call| {
                    let access = agent_store.agent_access(id);
                    match call {
                        Some(call) => access.for_call(call),
                        None => access,
                    }
                })),
                roles: Some(WasmRoles::new(move |_| role_store.clone()).read_only()),
                blobs: Some(
                    WasmBlobs::new(store.clone())
                        .with_agents(move |id, call| blob_store.agent_blobs(id).for_call(call)),
                ),
                ..WasmCapabilities::default()
            },
        )
        .unwrap();
    let mut registry = WasmRegistry::new();
    registry.insert("renamed", provider.clone()).unwrap();
    registry.insert("alias", provider).unwrap();
    let providers = Providers {
        registry: registry.clone(),
        reads: reads.clone(),
        writes: writes.clone(),
    };
    let workspace_store = store.clone();
    let blob_store = store.clone();
    let tools = wasm
        .provider(
            tools_component,
            &JsonObject::new(),
            WasmCapabilities {
                workspace_access: Some(WasmWorkspaceAccess::new(move |agent| {
                    AgentWorkspaces::new(
                        agent,
                        workspace_store.agent_access(agent),
                        workspace_store.clone(),
                        providers.clone(),
                    )
                })),
                blobs: Some(
                    WasmBlobs::new(store.clone())
                        .with_agents(move |id, call| blob_store.agent_blobs(id).for_call(call)),
                ),
                ..WasmCapabilities::default()
            },
        )
        .unwrap();
    let mut tools_registry = WasmRegistry::new();
    tools_registry.insert("tools", tools).unwrap();

    (registry, tools_registry)
}

async fn start(store: &Store) {
    for (name, workspace) in [
        (
            "parent",
            json!({ "name": "native:default", "config": { "cwd": "/repo" } }),
        ),
        (
            "child",
            json!({ "name": "renamed:parent", "config": { "directory": "src" } }),
        ),
    ] {
        let role = Role {
            name: name.into(),
            parameters: BTreeMap::new(),
            model: configured("model"),
            tools: BTreeMap::new(),
            system_prompt: String::new(),
            compaction: None,
            extensions: object(json!({ "fleet.workspace": workspace })),
        };
        store.put_role(name, &role, None).await.unwrap();
    }
    store
        .create_agent(
            "create-parent",
            &Agent {
                id: 1.into(),
                role: configured("parent"),
                extensions: JsonObject::new(),
            },
            Some(&Message {
                id: 1.into(),
                mode: MessageMode::Queue,
                content: vec![],
            }),
        )
        .await
        .unwrap();
    store.consume_messages(1.into(), None, true).await.unwrap();
    store
        .append_model_response(1.into(), 0.into(), true, &response(vec![call("drive")]))
        .await
        .unwrap();
}

async fn queued_child(store: &Store) -> AgentId {
    loop {
        let changed = store.agent_scopes_changed();
        let page = store
            .agents(AgentsQuery {
                parent: Some(1.into()),
                ..AgentsQuery::default()
            })
            .await
            .unwrap();
        if let Some(child) = page.agents.first() {
            let state = store.agent_state(child.id).await.unwrap().unwrap();
            if state.latest_event.is_none()
                && !store
                    .messages(child.id, None)
                    .await
                    .unwrap()
                    .value
                    .is_empty()
            {
                break child.id;
            }
        }
        changed.await;
    }
}

#[test]
fn parent_workspace_on_root_returns_tool_errors() {
    let wasm = Wasm::new().unwrap();
    let (agents_component, tools_component) = components(&wasm);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::memory().unwrap();
            start(&store).await;
            store
                .create_agent(
                    "create-root",
                    &Agent {
                        id: 2.into(),
                        role: configured("child"),
                        extensions: JsonObject::new(),
                    },
                    None,
                )
                .await
                .unwrap();
            let reads = Arc::new(Mutex::new(Vec::new()));
            let writes = Arc::new(Mutex::new(Vec::new()));
            let (_, tools) = providers(&wasm, &agents_component, &tools_component, &store, &reads, &writes);

            for (name, input) in [
                ("tools:bash", json!({ "command": "pwd" })),
                ("tools:read", json!({ "path": "file" })),
                ("tools:write", json!({ "path": "file", "content": "hello" })),
                (
                    "tools:edit",
                    json!({
                        "path": "file",
                        "edits": [{ "old": "hello", "new": "world" }],
                    }),
                ),
            ] {
                let context = ToolContext {
                    tool: configured(name),
                    agent_id: 2.into(),
                    call_id: name.into(),
                    role_extensions: JsonObject::new(),
                };
                let output = tools
                    .execute(&context, false, &object(input))
                    .await
                    .unwrap();

                assert!(output.is_error);
                assert_eq!(
                    output.content,
                    [ContentBlock::Text(
                        concat!(
                            "No workspace is available for this agent, so tools that require a workspace cannot run.\n\n",
                            "Details: could not resolve workspace: agent has no parent"
                        )
                        .into()
                    )]
                );
            }
            assert!(reads.lock().unwrap().is_empty());
        });
}

#[test]
fn workspace_agent_lookup_failures_remain_provider_errors() {
    let wasm = Wasm::new().unwrap();
    let (agents_component, tools_component) = components(&wasm);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::memory().unwrap();
            let reads = Arc::new(Mutex::new(Vec::new()));
            let writes = Arc::new(Mutex::new(Vec::new()));
            let (_, tools) = providers(
                &wasm,
                &agents_component,
                &tools_component,
                &store,
                &reads,
                &writes,
            );
            let context = ToolContext {
                tool: configured("tools:read"),
                agent_id: 1.into(),
                call_id: "read".into(),
                role_extensions: JsonObject::new(),
            };

            let error = tools
                .execute(&context, false, &object(json!({ "path": "file" })))
                .await
                .unwrap_err();
            let kind: ToolsErrorKind = error.kind();

            assert_eq!(kind, ToolsErrorKind::Other);
            assert!(error.to_string().contains("could not read workspace agent"));
            assert!(reads.lock().unwrap().is_empty());
        });
}

#[test]
fn delegation_retries_and_parent_resolution_ignore_provider_names() {
    let wasm = Wasm::new().unwrap();
    let (agents_component, tools_component) = components(&wasm);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(15), async {
                let store = Store::memory().unwrap();
                start(&store).await;
                let reads = Arc::new(Mutex::new(Vec::new()));
                let writes = Arc::new(Mutex::new(Vec::new()));
                let (registry, tools_registry) = providers(
                    &wasm,
                    &agents_component,
                    &tools_component,
                    &store,
                    &reads,
                    &writes,
                );
                let context = ToolContext {
                    tool: configured("renamed:oneshot_agent"),
                    agent_id: 1.into(),
                    call_id: "drive".into(),
                    role_extensions: JsonObject::new(),
                };
                let input = object(json!({ "role": "child", "prompt": "Read the file" }));
                let (output, child) =
                    futures_util::future::join(registry.execute(&context, false, &input), async {
                        let child = queued_child(&store).await;
                        let context = ToolContext {
                            tool: configured("tools:read"),
                            agent_id: child,
                            call_id: "read".into(),
                            role_extensions: JsonObject::new(),
                        };
                        let output = tools_registry
                            .execute(&context, false, &object(json!({ "path": "file" })))
                            .await
                            .unwrap();
                        assert!(!output.is_error);
                        store.consume_messages(child, None, true).await.unwrap();
                        store
                            .append_model_response(
                                child,
                                0.into(),
                                true,
                                &response(vec![call("server"), ModelOutput::Text("Done".into())]),
                            )
                            .await
                            .unwrap();
                        child
                    })
                    .await;
                let output = output.unwrap();
                assert_eq!(output.content, [ContentBlock::Text("Done".into())]);
                assert_eq!(output.structured_content, None);
                let retry = registry
                    .execute(
                        &ToolContext {
                            tool: configured("alias:oneshot_agent"),
                            ..context
                        },
                        true,
                        &input,
                    )
                    .await
                    .unwrap();
                assert_eq!(retry, output);
                let (workspace, path) = reads.lock().unwrap().first().unwrap().clone();
                assert_eq!(workspace.agent_id, child);
                assert_eq!(workspace.id, 1.into());
                assert_eq!(
                    workspace.workspace.config,
                    object(json!({ "cwd": "/repo" }))
                );
                assert_eq!(path, "src/file");
                store
                    .append_tool_result(
                        1.into(),
                        &ToolResult {
                            call_id: "drive".into(),
                            output,
                        },
                    )
                    .await
                    .unwrap();
                store.reconcile_agent_scopes().await.unwrap();
                let state = store.agent_state(child).await.unwrap().unwrap();
                assert!(state.interrupt_intent);
            })
            .await
            .unwrap();
        });
}

#[test]
fn delegation_forwards_and_returns_images() {
    let wasm = Wasm::new().unwrap();
    let (agents_component, tools_component) = components(&wasm);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(15), async {
                let store = Store::memory().unwrap();
                start(&store).await;
                store.put(1.into(), "image/png", IMAGE).await.unwrap();
                store
                    .send_message(
                        "attach",
                        1.into(),
                        &Message {
                            id: 2.into(),
                            mode: MessageMode::Queue,
                            content: vec![ContentBlock::Image(1.into())],
                        },
                    )
                    .await
                    .unwrap();
                let reads = Arc::new(Mutex::new(Vec::new()));
                let writes = Arc::new(Mutex::new(Vec::new()));
                let (registry, tools_registry) = providers(
                    &wasm,
                    &agents_component,
                    &tools_component,
                    &store,
                    &reads,
                    &writes,
                );
                let context = ToolContext {
                    tool: configured("renamed:oneshot_agent"),
                    agent_id: 1.into(),
                    call_id: "drive".into(),
                    role_extensions: JsonObject::new(),
                };
                let input = object(json!({
                    "role": "child",
                    "prompt": "Crop the image",
                    "images": ["1"],
                }));
                let text = "Cropped as image reference 2, not 3.";

                let (output, ()) =
                    futures_util::future::join(registry.execute(&context, false, &input), async {
                        let child = queued_child(&store).await;
                        let context = ToolContext {
                            tool: configured("tools:write"),
                            agent_id: child,
                            call_id: "write".into(),
                            role_extensions: JsonObject::new(),
                        };
                        let input = object(json!({ "path": "image.png", "blob": "1" }));
                        let output = tools_registry
                            .execute(&context, false, &input)
                            .await
                            .unwrap();
                        assert!(!output.is_error);
                        let (workspace, write) = writes.lock().unwrap().first().unwrap().clone();
                        assert_eq!(workspace.agent_id, child);
                        assert_eq!(
                            write,
                            WorkspaceWrite {
                                path: "src/image.png".into(),
                                content: IMAGE.to_vec(),
                                expected: None,
                                create_parents: true,
                            }
                        );
                        store
                            .agent_blobs(child)
                            .put(2.into(), "image/png", IMAGE)
                            .await
                            .unwrap();
                        store.consume_messages(child, None, true).await.unwrap();
                        store
                            .append_model_response(
                                child,
                                0.into(),
                                true,
                                &response(vec![ModelOutput::Text(text.into())]),
                            )
                            .await
                            .unwrap();
                    })
                    .await;

                assert_eq!(
                    output.unwrap().content,
                    [
                        ContentBlock::Text(text.into()),
                        ContentBlock::Image(2.into())
                    ]
                );
            })
            .await
            .unwrap();
        });
}

#[test]
fn delegation_rejects_unavailable_images() {
    let wasm = Wasm::new().unwrap();
    let (agents_component, tools_component) = components(&wasm);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::memory().unwrap();
            start(&store).await;
            store
                .agent_blobs(1.into())
                .put(1.into(), "text/plain", b"text")
                .await
                .unwrap();
            let reads = Arc::new(Mutex::new(Vec::new()));
            let writes = Arc::new(Mutex::new(Vec::new()));
            let (registry, _) = providers(
                &wasm,
                &agents_component,
                &tools_component,
                &store,
                &reads,
                &writes,
            );
            let context = ToolContext {
                tool: configured("renamed:oneshot_agent"),
                agent_id: 1.into(),
                call_id: "drive".into(),
                role_extensions: JsonObject::new(),
            };

            for reference in ["1", "2", "image"] {
                let input = object(json!({
                    "role": "child",
                    "prompt": "Crop the image",
                    "images": [reference],
                }));
                let output = registry.execute(&context, false, &input).await.unwrap();

                assert!(output.is_error);
                assert_eq!(
                    output.content,
                    [ContentBlock::Text(format!(
                        "Image reference {reference} is not an available image."
                    ))]
                );
            }
        });
}
