use super::*;
use crate::AdminToken;
use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use fleet_core::{Configured, JsonObject, ToolContext, Tools as _, TurnEnd};
use fleet_kit::{TurnEndEvent, WorkspaceContext, Workspaces, WorkspacesErrorKind};
use futures_util::future;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode, body::Bytes, header};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command, sync::OnceLock};

const TOKEN: &str = "sandbox-test-token-0123456789abcdef";

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("sandbox")
}

/// Returns the plugin directory from `FLEET_TEST_WASM`, or builds the plugins if that is unset.
fn plugins() -> &'static Path {
    static PLUGINS: OnceLock<PathBuf> = OnceLock::new();
    PLUGINS.get_or_init(|| {
        std::env::var_os("FLEET_TEST_WASM")
            .map_or_else(build, |directory| std::path::absolute(directory).unwrap())
    })
}

fn build() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let target = root.join("target/sandbox-fixture");
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["build", "--offline", "--locked", "--target"])
        .args([
            "wasm32-unknown-unknown",
            "--config",
            "profile.dev.opt-level=1",
        ])
        .args(["-p", "fleet-plugin-agents", "-p", "fleet-plugin-host"])
        .args(["-p", "fleet-plugin-roles", "-p", "fleet-plugin-sandbox"])
        .args(["-p", "fleet-plugin-sprites", "-p", "fleet-plugin-tasks"])
        .args(["-p", "fleet-plugin-tools"])
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

/// Loads the example config with the fixture plugins and additional top-level settings.
fn config(settings: Value) -> Config {
    let mut config: Value =
        serde_json::from_slice(&fs::read(directory().join("config.json")).unwrap()).unwrap();
    for (name, plugin) in config["plugins"].as_object_mut().unwrap() {
        let path = plugins().join(format!("fleet_plugin_{name}.wasm"));
        plugin["path"] = path.to_str().unwrap().into();
    }
    let Value::Object(settings) = settings else {
        panic!("settings must be an object")
    };
    for (key, value) in settings {
        config[key.as_str()] = value;
    }
    serde_json::from_value(config).unwrap()
}

fn run(test: impl AsyncFnOnce(&Fleet)) {
    run_with(json!({}), async |fleet, _| test(fleet).await);
}

fn run_with(settings: Value, test: impl AsyncFnOnce(&Fleet, &WasmRegistry)) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (fleet, providers) = open(config(settings), &directory()).await.unwrap();
            tokio::time::timeout(
                Duration::from_secs(60),
                future::join(fleet.run(&*providers, &*providers, &*providers), async {
                    test(&fleet, &providers).await;
                    fleet.shutdown();
                }),
            )
            .await
            .unwrap();
            providers.shutdown().await;
        });
}

/// Reads a file from an agent's own sandbox workspace, or returns `None` if it does not exist.
async fn workspace_file(providers: &WasmRegistry, agent: &str, path: &str) -> Option<String> {
    let id = agent.parse::<u128>().unwrap();
    let context = WorkspaceContext {
        workspace: Configured {
            name: "sandbox:default".into(),
            config: JsonObject::new(),
        },
        agent_id: id.into(),
        id: id.into(),
    };
    match Workspaces::read(providers, &context, path, 1024).await {
        Ok(bytes) => Some(String::from_utf8(bytes).unwrap()),
        Err(error)
            if fleet_core::Error::<WorkspacesErrorKind>::kind(&error)
                == WorkspacesErrorKind::FileNotFound =>
        {
            None
        }
        Err(error) => panic!("{error}"),
    }
}

/// Waits until a file is released from an agent's own sandbox workspace.
async fn wait_for_release(providers: &WasmRegistry, agent: &str, path: &str) {
    while workspace_file(providers, agent, path).await.is_some() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn call(
    fleet: &Fleet,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    call_with(fleet, method, path, &[], body).await
}

async fn call_with(
    fleet: &Fleet,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> (StatusCode, Value) {
    static KEYS: AtomicU64 = AtomicU64::new(0);
    let key = KEYS.fetch_add(1, Ordering::Relaxed);
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", key.to_string());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(Full::new(Bytes::from(
            body.map_or_else(String::new, Value::to_string),
        )))
        .unwrap();
    let token = AdminToken::new(TOKEN.as_ref()).unwrap();
    let response = crate::admin_auth::handle(token, fleet.clone(), request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

fn message(id: u128, script: &Value) -> Value {
    json!({
        "id": id.to_string(),
        "mode": "queue",
        "content": [{ "type": "text", "text": script.to_string() }],
    })
}

/// Creates agent 1 with message 1 and waits for its turn to end.
async fn start(fleet: &Fleet, role: &str, script: &Value) -> TurnEndEvent {
    let agent = json!({ "id": "1", "role": role, "message": message(1, script) });
    let (status, _) = call(fleet, "POST", "/agents", Some(&agent)).await;
    assert_eq!(status, StatusCode::CREATED);
    fleet.wait_for_turn_end(1.into(), 1.into()).await.unwrap()
}

/// Sends a message to agent 1 and waits for its turn to end.
async fn send(fleet: &Fleet, id: u128, script: &Value) -> TurnEndEvent {
    let message = message(id, script);
    let (status, _) = call(fleet, "POST", "/agents/1/messages", Some(&message)).await;
    assert_eq!(status, StatusCode::CREATED);
    fleet.wait_for_turn_end(1.into(), id.into()).await.unwrap()
}

async fn events(fleet: &Fleet, agent: &str) -> Vec<Value> {
    let (status, events) = call(fleet, "GET", &format!("/agents/{agent}/events"), None).await;
    assert_eq!(status, StatusCode::OK);
    events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["payload"].clone())
        .collect()
}

/// Returns the text of agent 1's latest model response.
async fn reply(fleet: &Fleet) -> String {
    let events = events(fleet, "1").await;
    let response = events
        .iter()
        .rfind(|event| event["type"] == "model_response")
        .unwrap();
    response["output"][0]["text"].as_str().unwrap().into()
}

#[test]
fn scripted_agents_use_tools_in_an_in_memory_workspace() {
    run(async |fleet| {
        let script = json!({
            "steps": [
                { "tool_calls": [{ "tool": "write", "input": { "path": "notes.txt", "content": "hello" } }] },
                {
                    "tool_calls": [{
                        "tool": "edit",
                        "input": { "path": "notes.txt", "edits": [{ "old": "hello", "new": "hello world" }] },
                    }],
                },
                { "tool_calls": [{ "tool": "read", "input": { "path": "notes.txt" } }] },
                { "tool_calls": [{ "tool": "bash", "input": { "command": "ls" } }] },
                { "text": "Done." },
            ],
        });

        let end = start(fleet, "assistant", &script).await;

        assert_eq!(end.payload, TurnEnd::Completed);
        let results: Vec<_> = events(fleet, "1")
            .await
            .into_iter()
            .filter(|event| event["type"] == "tool_result")
            .map(|event| event["output"].clone())
            .collect();
        assert_eq!(
            results
                .iter()
                .map(|output| &output["content"][0]["text"])
                .collect::<Vec<_>>(),
            [
                "Wrote 5 bytes to notes.txt.",
                "Edited notes.txt.",
                "hello world",
                "Simulated: ls\n",
            ],
        );
        assert!(results.iter().all(|output| output["is_error"] == false));
        assert_eq!(reply(fleet).await, "Done.");
    });
}

#[test]
fn scripted_agents_delete_roles() {
    run(async |fleet| {
        for (name, tools) in [
            (
                "admin",
                json!({ "delete_role": { "name": "roles:delete_role" } }),
            ),
            ("temporary", json!({})),
        ] {
            let role = json!({ "model": { "name": "sandbox:scripted" }, "tools": tools });
            let path = format!("/roles/{name}");
            let headers = [("if-none-match", "*")];
            let (status, _) = call_with(fleet, "PUT", &path, &headers, Some(&role)).await;
            assert_eq!(status, StatusCode::CREATED);
        }
        let (_, temporary) = call(fleet, "GET", "/roles/temporary", None).await;
        let version = temporary["version"].as_str().unwrap();
        let stale = (version.parse::<u128>().unwrap() - 1).to_string();
        let delete = |version: &str| {
            json!({
                "tool_calls": [{
                    "tool": "delete_role",
                    "input": { "name": "temporary", "version": version },
                }],
            })
        };
        let script = json!({
            "steps": [delete(&stale), delete(version), delete(version), { "text": "Done." }],
        });

        start(fleet, "admin", &script).await;

        let results: Vec<_> = events(fleet, "1")
            .await
            .into_iter()
            .filter(|event| event["type"] == "tool_result")
            .map(|event| {
                let output = &event["output"];
                json!({ "text": output["content"][0]["text"], "is_error": output["is_error"] })
            })
            .collect();
        assert_eq!(
            Value::from(results),
            json!([
                {
                    "text": "Role temporary changed since it was read. Read it again before deleting it.",
                    "is_error": true,
                },
                { "text": "Role temporary was deleted.", "is_error": false },
                { "text": "Role temporary does not exist.", "is_error": true },
            ]),
        );
        let (status, _) = call(fleet, "GET", "/roles/temporary", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    });
}

#[test]
fn delegated_agents_share_the_parent_workspace() {
    run(async |fleet| {
        let helper = json!({
            "steps": [
                { "tool_calls": [{ "tool": "read", "input": { "path": "notes.txt" } }] },
                { "text": "Read the notes." },
            ],
        });
        let script = json!({
            "steps": [
                { "tool_calls": [{ "tool": "write", "input": { "path": "notes.txt", "content": "shared" } }] },
                {
                    "tool_calls": [{
                        "tool": "delegate",
                        "input": { "role": "helper", "prompt": helper.to_string() },
                    }],
                },
                { "text": "Delegated." },
            ],
        });

        start(fleet, "assistant", &script).await;

        let (_, children) = call(fleet, "GET", "/agents?parent=1", None).await;
        let child = children["agents"][0]["id"].as_str().unwrap();
        let read = events(fleet, child)
            .await
            .into_iter()
            .find(|event| event["type"] == "tool_result")
            .unwrap();
        assert_eq!(read["output"]["content"][0]["text"], "shared");
        assert_eq!(read["output"]["is_error"], false);
        let delegated = events(fleet, "1")
            .await
            .into_iter()
            .rfind(|event| event["type"] == "tool_result")
            .unwrap();
        assert_eq!(delegated["output"]["content"][0]["text"], "Read the notes.");
        assert_eq!(reply(fleet).await, "Delegated.");
    });
}

#[test]
fn delegated_tasks_can_be_awaited_and_cancelled() {
    run(async |fleet| {
        let delegate = |task: &str, step: Value| {
            json!({
                "tool": "delegate_async",
                "input": {
                    "role": "helper",
                    "prompt": json!({ "steps": [step] }).to_string(),
                    "task": task,
                },
            })
        };
        let wait = json!({ "tool": "wait", "input": { "tasks": ["fast"] } });
        let script = json!({
            "steps": [
                {
                    "tool_calls": [
                        delegate("slow", json!({ "text": "Too late.", "delay_ms": 30000 })),
                        delegate("fast", json!({ "text": "Helped." })),
                        wait.clone(),
                    ],
                },
                { "tool_calls": [{ "tool": "cancel", "input": { "tasks": ["slow", "missing"] } }] },
                { "tool_calls": [{ "tool": "cancel", "input": { "tasks": ["slow", "fast"] } }] },
                { "tool_calls": [delegate("fast", json!({ "text": "Again." })), wait] },
                { "text": "Done." },
            ],
        });

        start(fleet, "assistant", &script).await;

        let results = loop {
            let results: Vec<_> = events(fleet, "1")
                .await
                .into_iter()
                .filter(|event| event["type"] == "tool_result")
                .map(|event| {
                    let output = &event["output"];
                    (
                        output["content"][0]["text"].as_str().unwrap().to_owned(),
                        output["is_error"].as_bool().unwrap(),
                    )
                })
                .collect();
            if results.len() == 7 {
                break results;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let interrupted = (
            "The tool call was interrupted. Its remaining agent work is being stopped.".into(),
            true,
        );
        let stopped = ("Stopped: slow.\nAlready finished: fast.".into(), false);
        let position = |result| results.iter().position(|other| other == result).unwrap();
        assert!(position(&interrupted) < position(&stopped));
        let mut results = results;
        results.sort();
        assert_eq!(
            results,
            [
                (
                    "Another call in this conversation already uses the task name fast. Choose \
                     a different name."
                        .into(),
                    true,
                ),
                ("Finished: fast.".into(), false),
                ("Helped.".into(), false),
                (
                    "More than one call in this conversation uses these task names: fast.".into(),
                    true,
                ),
                (
                    "No call in this conversation uses these task names: missing.".into(),
                    true,
                ),
                stopped,
                interrupted,
            ]
        );
    });
}

#[test]
fn closed_subagents_release_only_their_own_workspaces() {
    run_with(json!({}), async |fleet, providers| {
        let worker = json!({
            "model": { "name": "sandbox:scripted" },
            "tools": { "write": { "name": "tools:write" } },
            "workspace": { "name": "sandbox:default" },
        });
        let (status, _) = call_with(
            fleet,
            "PUT",
            "/roles/worker",
            &[("if-none-match", "*")],
            Some(&worker),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let write = |path: &str| json!({ "tool_calls": [{ "tool": "write", "input": { "path": path, "content": path } }] });
        let delegate = |role: &str, step: Value| {
            json!({
                "tool_calls": [{
                    "tool": "delegate",
                    "input": { "role": role, "prompt": json!({ "steps": [step] }).to_string() },
                }],
            })
        };
        let script = json!({
            "steps": [
                write("notes.txt"),
                delegate("helper", write("helped.txt")),
                delegate("worker", write("own.txt")),
                { "text": "Delegated." },
            ],
        });

        start(fleet, "assistant", &script).await;

        let (_, children) = call(fleet, "GET", "/agents?parent=1", None).await;
        let worker = children["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|child| child["role"] == "worker")
            .unwrap()["id"]
            .as_str()
            .unwrap();
        let result = events(fleet, worker)
            .await
            .into_iter()
            .find(|event| event["type"] == "tool_result")
            .unwrap();
        assert_eq!(
            result["output"]["content"][0]["text"],
            "Wrote 7 bytes to own.txt."
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            wait_for_release(providers, worker, "own.txt").await;
            let store = fleet.blob_store();
            while !store
                .workspace_releases(u64::MAX, None)
                .await
                .unwrap()
                .0
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            workspace_file(providers, "1", "notes.txt").await.as_deref(),
            Some("notes.txt")
        );
        assert_eq!(
            workspace_file(providers, "1", "helped.txt")
                .await
                .as_deref(),
            Some("helped.txt")
        );
    });
}

#[test]
fn settled_agents_release_their_workspace_and_reopen_it_when_used() {
    run_with(
        json!({ "release_settled_workspaces_after_ms": 0 }),
        async |fleet, providers| {
            let write = json!({
                "tool_calls": [{ "tool": "write", "input": { "path": "notes.txt", "content": "kept" } }],
            });
            start(fleet, "assistant", &write).await;
            assert_eq!(
                workspace_file(providers, "1", "notes.txt").await.as_deref(),
                Some("kept")
            );
            let (_, agents) = call(fleet, "GET", "/agents", None).await;
            let version = format!("\"{}\"", agents["agents"][0]["version"].as_str().unwrap());
            let (status, _) = call_with(
                fleet,
                "PUT",
                "/agents/1/inbox",
                &[("if-match", &version)],
                Some(&json!({ "state": "settled" })),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);

            tokio::time::timeout(
                Duration::from_secs(10),
                wait_for_release(providers, "1", "notes.txt"),
            )
            .await
            .unwrap();
            send(fleet, 2, &write).await;
            assert_eq!(
                workspace_file(providers, "1", "notes.txt").await.as_deref(),
                Some("kept")
            );
        },
    );
}

#[test]
fn deleted_agents_release_their_workspace() {
    run_with(json!({}), async |fleet, providers| {
        let write = json!({
            "tool_calls": [{ "tool": "write", "input": { "path": "notes.txt", "content": "kept" } }],
        });
        start(fleet, "assistant", &write).await;
        assert!(workspace_file(providers, "1", "notes.txt").await.is_some());

        let (status, _) = call(fleet, "DELETE", "/agents/1", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        tokio::time::timeout(
            Duration::from_secs(10),
            wait_for_release(providers, "1", "notes.txt"),
        )
        .await
        .unwrap();
    });
}

#[test]
fn scripts_continue_across_compaction() {
    run(async |fleet| {
        let role = json!({
            "model": { "name": "sandbox:scripted" },
            "tools": {
                "read": { "name": "tools:read" },
                "write": { "name": "tools:write" },
            },
            "workspace": { "name": "sandbox:default" },
            "compaction": { "threshold": 100 },
        });
        let (status, _) = call_with(
            fleet,
            "PUT",
            "/roles/compacting",
            &[("if-none-match", "*")],
            Some(&role),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let script = json!({
            "steps": [
                { "tool_calls": [{ "tool": "write", "input": { "path": "notes.txt", "content": "kept" } }] },
                { "tool_calls": [{ "tool": "read", "input": { "path": "notes.txt" } }] },
                { "text": "Done after compaction." },
            ],
        });

        let end = start(fleet, "compacting", &script).await;

        assert_eq!(end.payload, TurnEnd::Completed);
        let events = events(fleet, "1").await;
        assert!(events.iter().any(|event| event["type"] == "compaction"));
        let read = events
            .iter()
            .rfind(|event| event["type"] == "tool_result")
            .unwrap();
        assert_eq!(read["output"]["content"][0]["text"], "kept");
        assert_eq!(reply(fleet).await, "Done after compaction.");
    });
}

#[test]
fn sign_in_reaches_plugins_and_refreshes_tokens() {
    run(async |fleet| {
        let connection = "/connections/sandbox:account";
        let credential = json!({ "credential": true });

        start(fleet, "assistant", &credential).await;
        assert_eq!(reply(fleet).await, "Not signed in.");

        let key = json!({ "method": "key", "key": "secret" });
        let (status, _) = call(
            fleet,
            "PUT",
            &format!("{connection}/credential"),
            Some(&key),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        send(fleet, 2, &credential).await;
        assert_eq!(reply(fleet).await, "Signed in with API key secret.");

        let authorize = json!({ "method": "oauth", "redirect_uri": "http://127.0.0.1:9/callback" });
        let (status, authorization) = call(
            fleet,
            "POST",
            &format!("{connection}/authorizations"),
            Some(&authorize),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let url: Url = authorization["url"].as_str().unwrap().parse().unwrap();
        assert_eq!(url.origin().ascii_serialization(), "http://127.0.0.1:1337");
        let browser = Request::get(&url[url::Position::BeforePath..])
            .body(Full::new(Bytes::new()))
            .unwrap();
        let token = AdminToken::new(TOKEN.as_ref()).unwrap();
        let redirect = crate::admin_auth::handle(token, fleet.clone(), browser)
            .await
            .unwrap();
        assert_eq!(redirect.status(), StatusCode::FOUND);
        let callback = json!({ "url": redirect.headers()[header::LOCATION].to_str().unwrap() });
        let id = authorization["id"].as_str().unwrap();
        let (status, completed) = call(
            fleet,
            "POST",
            &format!("{connection}/authorizations/{id}/callback"),
            Some(&callback),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(completed["status"], "complete");

        send(fleet, 3, &credential).await;
        assert_eq!(
            reply(fleet).await,
            "Signed in with OAuth token sandbox-access-1."
        );
        send(fleet, 4, &json!({ "refresh": true })).await;
        assert_eq!(
            reply(fleet).await,
            "Signed in with OAuth token sandbox-access-2."
        );
    });
}

#[test]
fn plugins_reaching_outside_fleet_are_rejected() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let provider = |capabilities| {
                json!({
                    "plugins": { "host": { "path": "host.wasm" } },
                    "providers": { "local": { "plugin": "host", "capabilities": capabilities } },
                })
            };
            for (config, access) in [
                (
                    json!({ "plugins": { "host": { "url": "https://example.com/host.wasm" } } }),
                    "plugin host uses a remote URL",
                ),
                (
                    provider(json!({ "process": true })),
                    "provider local uses process",
                ),
                (
                    provider(json!({ "process": [{ "program": "true" }] })),
                    "provider local uses process",
                ),
                (
                    provider(json!({ "http": ["https://example.com"] })),
                    "provider local uses HTTP",
                ),
                (
                    provider(json!({
                        "filesystem": [{ "host_path": ".", "guest_path": "/", "read_only": true }],
                    })),
                    "provider local uses filesystem",
                ),
            ] {
                let error = open(serde_json::from_value(config).unwrap(), &directory())
                    .await
                    .err()
                    .unwrap();
                assert_eq!(
                    error.to_string(),
                    format!("{access}, which the sandbox does not allow")
                );
            }
        });
}

#[test]
fn providers_of_one_plugin_have_their_own_capabilities() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let path = plugins().join("fleet_plugin_roles.wasm");
            let config = json!({
                "plugins": { "roles": { "path": path.to_str().unwrap() } },
                "providers": {
                    "roles": { "plugin": "roles", "capabilities": { "roles": true } },
                    "unapproved": { "plugin": "roles" },
                },
            });
            let (_fleet, providers) = open(serde_json::from_value(config).unwrap(), &directory())
                .await
                .unwrap();
            let context = |name: &str| ToolContext {
                tool: Configured {
                    name: name.into(),
                    config: JsonObject::new(),
                },
                agent_id: 1.into(),
                call_id: "call".into(),
                role_extensions: JsonObject::new(),
            };

            let output = providers
                .execute(&context("roles:list_roles"), false, &JsonObject::new())
                .await
                .unwrap();
            let error = providers
                .execute(&context("unapproved:list_roles"), false, &JsonObject::new())
                .await
                .unwrap_err();

            assert!(!output.is_error);
            assert_eq!(
                error.to_string(),
                "failed to list roles: this provider was not granted roles"
            );
        });
}

#[test]
fn plugins_load_without_outside_access_they_import() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let path = |plugin: &str| {
                let path = plugins().join(format!("fleet_plugin_{plugin}.wasm"));
                json!({ "path": path })
            };
            let config = json!({
                "plugins": { "host": path("host"), "sprites": path("sprites") },
                "providers": {
                    "host": { "plugin": "host" },
                    "sprites": { "plugin": "sprites" },
                },
            });
            let (_fleet, providers) = open(serde_json::from_value(config).unwrap(), &directory())
                .await
                .unwrap();
            let context = WorkspaceContext {
                workspace: Configured {
                    name: "host:default".into(),
                    config: json!({ "cwd": "/" }).as_object().unwrap().clone(),
                },
                agent_id: 1.into(),
                id: 1.into(),
            };

            let error = providers.open(&context).await.unwrap_err();

            assert_eq!(error.to_string(), "no filesystem mount covers /");
        });
}

/// Puts a `broken` role whose `missing` tool does not exist, or the same role without it.
async fn put_broken_role(fleet: &Fleet, fixed: bool) {
    let mut role = json!({
        "model": { "name": "sandbox:scripted" },
        "tools": { "missing": { "name": "tools:missing" } },
    });
    let (status, current) = call(fleet, "GET", "/roles/broken", None).await;
    let status = if status == StatusCode::OK {
        assert!(fixed);
        role["tools"] = json!({});
        let version = format!("\"{}\"", current["version"].as_str().unwrap());
        call_with(
            fleet,
            "PUT",
            "/roles/broken",
            &[("if-match", &version)],
            Some(&role),
        )
        .await
        .0
    } else {
        call_with(
            fleet,
            "PUT",
            "/roles/broken",
            &[("if-none-match", "*")],
            Some(&role),
        )
        .await
        .0
    };
    assert!(status.is_success());
}

/// Creates agent 1, which delegates to an assistant that delegates to a `broken` agent, and
/// waits until the `broken` agent is stalled. Returns the IDs of the root's descendants.
async fn start_stalled_chain(fleet: &Fleet) -> [String; 2] {
    put_broken_role(fleet, false).await;
    let delegate = |role: &str, prompt: &Value| {
        json!({
            "tool_calls": [{
                "tool": "delegate",
                "input": { "role": role, "prompt": prompt.to_string() },
            }],
        })
    };
    let child = json!({ "steps": [delegate("broken", &json!({})), { "text": "Child done." }] });
    let root = json!({ "steps": [delegate("assistant", &child), { "text": "Root done." }] });
    let agent = json!({ "id": "1", "role": "assistant", "message": message(1, &root) });
    let (status, _) = call(fleet, "POST", "/agents", Some(&agent)).await;
    assert_eq!(status, StatusCode::CREATED);

    tokio::time::timeout(Duration::from_secs(10), async {
        while call(fleet, "GET", "/agents/1/activity", None).await.1["stalled_descendants"] != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let (_, children) = call(fleet, "GET", "/agents?parent=1", None).await;
    let child = children["agents"][0]["id"].as_str().unwrap().to_owned();
    let (_, children) = call(fleet, "GET", &format!("/agents?parent={child}"), None).await;
    let grandchild = children["agents"][0]["id"].as_str().unwrap().to_owned();
    [child, grandchild]
}

/// Returns each agent's own stall and its count of stalled descendants.
async fn stalls(fleet: &Fleet, agents: &[&str]) -> Vec<(Value, Value)> {
    let mut stalls = Vec::new();
    for agent in agents {
        let (_, activity) = call(fleet, "GET", &format!("/agents/{agent}/activity"), None).await;
        stalls.push((
            activity["stalled"].clone(),
            activity["stalled_descendants"].clone(),
        ));
    }
    stalls
}

fn event_types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect()
}

/// Asserts that a stalled chain shows its stall only through activity and counts.
async fn assert_stalled_chain(fleet: &Fleet, [child, grandchild]: &[String; 2]) {
    let stalled = json!({
        "kind": "tool_not_found",
        "role": "broken",
        "alias": "missing",
        "tool": "tools:missing",
    });
    assert_eq!(
        stalls(fleet, &["1", child, grandchild]).await,
        [
            (Value::Null, json!(1)),
            (Value::Null, json!(1)),
            (stalled, json!(0)),
        ]
    );
    for agent in ["1", child] {
        let events = events(fleet, agent).await;
        assert_eq!(event_types(&events), ["input", "model_response"]);
    }
    assert_eq!(event_types(&events(fleet, grandchild).await), ["input"]);
    let (_, agents) = call(fleet, "GET", "/agents", None).await;
    assert_eq!(agents["agents"][0]["inbox"], "busy");
    assert_eq!(agents["agents"][0]["stalled_descendants"], 1);
}

/// Fixes the `broken` role, resumes the root, and waits for the root's turn to end.
async fn resume_stalled_chain(fleet: &Fleet, [child, grandchild]: &[String; 2]) {
    put_broken_role(fleet, true).await;
    let (status, _) = call(fleet, "POST", "/agents/1/resumes", Some(&json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let end = fleet.wait_for_turn_end(1.into(), 1.into()).await.unwrap();
    assert_eq!(end.payload, TurnEnd::Completed);
    assert_eq!(reply(fleet).await, "Root done.");
    assert_eq!(
        stalls(fleet, &["1", child, grandchild]).await,
        vec![(Value::Null, json!(0)); 3]
    );
}

#[test]
fn resuming_a_root_resumes_its_stalled_subagents() {
    run(async |fleet| {
        let chain = start_stalled_chain(fleet).await;

        assert_stalled_chain(fleet, &chain).await;
        resume_stalled_chain(fleet, &chain).await;
    });
}

#[test]
fn stalled_subagents_stay_stalled_across_restarts() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::memory().unwrap();
            let mut chain = None;

            for restarted in [false, true] {
                let (fleet, providers) = open_store(config(json!({})), &directory(), store.clone())
                    .await
                    .unwrap();
                tokio::time::timeout(
                    Duration::from_secs(60),
                    future::join(fleet.run(&*providers, &*providers, &*providers), async {
                        if restarted {
                            let chain = chain.as_ref().unwrap();
                            assert_stalled_chain(&fleet, chain).await;
                            resume_stalled_chain(&fleet, chain).await;
                        } else {
                            chain = Some(start_stalled_chain(&fleet).await);
                        }
                        fleet.shutdown();
                    }),
                )
                .await
                .unwrap();
                providers.shutdown().await;
            }
        });
}

#[test]
fn deleting_a_running_root_stops_its_subagents() {
    run(async |fleet| {
        let [child, grandchild] = start_stalled_chain(fleet).await;

        let (status, _) = call(fleet, "DELETE", "/agents/1", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        for agent in ["1", &child, &grandchild] {
            let (status, _) = call(fleet, "GET", &format!("/agents/{agent}"), None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        let script = json!({ "steps": [{ "text": "Done." }] });
        let agent = json!({ "id": "2", "role": "assistant", "message": message(1, &script) });
        let (status, _) = call(fleet, "POST", "/agents", Some(&agent)).await;
        assert_eq!(status, StatusCode::CREATED);
        let end = fleet.wait_for_turn_end(2.into(), 1.into()).await.unwrap();
        assert_eq!(end.payload, TurnEnd::Completed);
        let (_, page) = call(fleet, "GET", "/agents", None).await;
        assert_eq!(page["agents"].as_array().unwrap().len(), 1);
    });
}

#[test]
fn interrupting_a_root_stops_its_stalled_subagents() {
    run(async |fleet| {
        let [child, grandchild] = start_stalled_chain(fleet).await;
        let interrupt = json!({ "include_async_tool_calls": false });

        let (status, _) = call(fleet, "POST", "/agents/1/interrupts", Some(&interrupt)).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        let end = fleet.wait_for_turn_end(1.into(), 1.into()).await.unwrap();
        assert_eq!(end.payload, TurnEnd::Interrupted);
        tokio::time::timeout(Duration::from_secs(10), async {
            while stalls(fleet, &["1", &child, &grandchild]).await
                != vec![(Value::Null, json!(0)); 3]
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        for agent in [&child, &grandchild] {
            let events = events(fleet, agent).await;
            assert_eq!(events.last().unwrap()["type"], "turn_end");
        }
    });
}
