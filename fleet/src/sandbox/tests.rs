use super::*;
use crate::AdminToken;
use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use fleet_core::TurnEnd;
use fleet_kit::TurnEndEvent;
use futures_util::future;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode, body::Bytes, header};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command, sync::OnceLock};

const TOKEN: &str = "sandbox-test-token-0123456789abcdef";

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("sandbox")
}

fn plugins() -> &'static Path {
    static PLUGINS: OnceLock<PathBuf> = OnceLock::new();
    PLUGINS.get_or_init(|| {
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
            .args(["-p", "fleet-plugin-agents", "-p", "fleet-plugin-roles"])
            .args(["-p", "fleet-plugin-sandbox", "-p", "fleet-plugin-tools"])
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
    })
}

/// Loads the example config with plugins from the fixture build.
fn config() -> Config {
    let mut config: Value =
        serde_json::from_slice(&fs::read(directory().join("config.json")).unwrap()).unwrap();
    for (name, plugin) in config["plugins"].as_object_mut().unwrap() {
        let path = plugins().join(format!("fleet_plugin_{name}.wasm"));
        plugin["path"] = path.to_str().unwrap().into();
    }
    serde_json::from_value(config).unwrap()
}

fn run(test: impl AsyncFnOnce(&Fleet)) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (fleet, providers) = open(config(), &directory()).await.unwrap();
            tokio::time::timeout(
                Duration::from_secs(60),
                future::join(fleet.run(&providers, &providers), async {
                    test(&fleet).await;
                    fleet.shutdown();
                }),
            )
            .await
            .unwrap();
            providers.shutdown().await;
        });
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
            for (plugin, access) in [
                (json!({ "url": "https://example.com/host.wasm" }), "a remote URL"),
                (json!({ "path": "host.wasm", "capabilities": { "process": true } }), "process"),
                (
                    json!({ "path": "host.wasm", "capabilities": { "http": ["https://example.com"] } }),
                    "HTTP",
                ),
            ] {
                let config = json!({ "plugins": { "host": plugin } });
                let error = open(serde_json::from_value(config).unwrap(), &directory())
                    .await
                    .err()
                    .unwrap();
                assert_eq!(
                    error.to_string(),
                    format!("plugin host uses {access}, which the sandbox does not allow")
                );
            }
        });
}
