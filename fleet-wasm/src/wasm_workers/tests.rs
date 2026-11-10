use super::*;
use crate::{Wasm, WasmCapabilities, WasmRegistry};
use core::assert_matches;
use fleet_core::{Configured, ContentBlock, JsonObject, PromptContext, PromptOutput, Prompts as _};
use serde_json::json;
use std::{path::Path, process::Command, sync::OnceLock};

fn run(future: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(15), future)
                .await
                .unwrap();
        });
}

fn component(wasm: &Wasm) -> crate::WasmComponent {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = BYTES.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let target = root.join("target/worker-fixture");
        let output = Command::new(env!("CARGO"))
            .current_dir(root)
            .args([
                "build",
                "--offline",
                "--locked",
                "-p",
                "fleet-wasm-test-worker",
                "--target",
                "wasm32-unknown-unknown",
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
        let module =
            std::fs::read(target.join("wasm32-unknown-unknown/debug/fleet_wasm_test_worker.wasm"))
                .unwrap();
        wit_component::ComponentEncoder::default()
            .module(&module)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap()
    });
    wasm.compile(bytes).unwrap()
}

fn provider(wasm: &Wasm, component: &crate::WasmComponent) -> crate::WasmProvider {
    wasm.provider(
        component,
        &JsonObject::new(),
        WasmCapabilities {
            workers: true,
            ..WasmCapabilities::default()
        },
    )
    .unwrap()
}

fn registry(provider: crate::WasmProvider) -> WasmRegistry {
    let mut registry = WasmRegistry::new();
    registry.insert("test", provider).unwrap();
    registry
}

fn context(config: JsonObject) -> PromptContext {
    PromptContext {
        prompt: Configured {
            name: "test:echo".into(),
            config,
        },
        agent_id: 1.into(),
    }
}

async fn echo(provider: &WasmRegistry, text: &str) -> String {
    let context = context(JsonObject::new());
    let input = json!({ "text": text }).as_object().unwrap().clone();
    let response = provider.render(&context, &input).await.unwrap();
    let PromptOutput::Assistant(blocks) = response.output.first().unwrap() else {
        panic!("expected assistant output")
    };
    let ContentBlock::Text(text) = blocks.first().unwrap() else {
        panic!("expected text")
    };
    text.clone()
}

#[test]
fn fresh_calls_share_private_workers_and_stream_with_backpressure() {
    let wasm = Wasm::new().unwrap();
    let component = component(&wasm);
    let first = provider(&wasm, &component);
    let second = provider(&wasm, &component);
    let alias = registry(first.clone());
    let first = registry(first);
    let second = registry(second);

    run(async {
        let mut replies: [String; 2] = future::join(echo(&first, "hello"), echo(&alias, "hello"))
            .await
            .into();
        replies.sort();
        assert_eq!(
            replies,
            ["1:{\"text\":\"hello\"}", "2:{\"text\":\"hello\"}"]
        );
        let separate = echo(&second, "separate").await;
        assert_eq!(separate, "1:{\"text\":\"separate\"}");

        let text = "x".repeat(256 * 1024);
        let mut replies: [String; 2] = future::join(echo(&first, &text), echo(&alias, &text))
            .await
            .into();
        replies.sort();
        assert_eq!(
            replies,
            [
                format!("3:{{\"text\":\"{text}\"}}"),
                format!("4:{{\"text\":\"{text}\"}}")
            ]
        );

        first.shutdown().await;
        second.shutdown().await;
    });
}

#[test]
fn failed_startup_and_conflicting_initialization_are_reported() {
    let wasm = Wasm::new().unwrap();
    let component = component(&wasm);
    let provider = registry(provider(&wasm, &component));
    let denied = wasm.provider(&component, &JsonObject::new(), WasmCapabilities::default());
    assert!(denied.is_err());

    run(async {
        let failed = context(json!({ "fail": true }).as_object().unwrap().clone());
        let error = provider
            .render(&failed, &JsonObject::new())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "fixture startup failure");

        echo(&provider, "ready").await;
        let different = context(json!({ "different": true }).as_object().unwrap().clone());
        let error = provider
            .render(&different, &JsonObject::new())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "worker initialization does not match the existing worker"
        );
        provider.shutdown().await;
    });
}

#[test]
fn dropping_a_call_closes_its_connection_and_retirement_is_atomic() {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model_async(true);
    let engine = wasmtime::Engine::new(&config).unwrap();
    let listener = Listener::new();
    let connections = Arc::new(Semaphore::new(1));
    let client = listener.connect(&connections).unwrap().unwrap();
    let state = client.state().clone();
    let mut store = WasmState::new("{}".into(), WasiFilesystemCtx::default(), None, None, None)
        .into_store(&engine);
    store.data_mut().table().push(client).unwrap();

    run(async {
        let server = listener.accept().await.unwrap();
        assert!(!listener.try_retire());
        drop(store);
        let closed = server.state().closed().await;
        closed.unwrap();
        assert!(!state.is_open());
        assert!(listener.try_retire());
        let connection = listener.connect(&connections).unwrap();
        assert!(connection.is_none());
        let connection = listener.accept().await;
        assert!(connection.is_none());
    });
}

#[test]
fn shutdown_stops_uncooperative_workers_and_prevents_restart() {
    let wasm = Wasm::new().unwrap();
    let component = component(&wasm);
    let provider = registry(provider(&wasm, &component));

    run(async {
        let spin = context(json!({ "spin": true }).as_object().unwrap().clone());
        let (result, ()) = future::join(provider.render(&spin, &JsonObject::new()), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            provider.shutdown().await;
        })
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.to_string(), "worker did not stop within five seconds");
        let result = provider
            .render(&context(JsonObject::new()), &JsonObject::new())
            .await;
        assert_matches!(result, Err(_));
    });
}
