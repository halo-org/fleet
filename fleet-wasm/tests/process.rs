//! Runs host plugin commands with and without the process capability.

#![cfg(test)]
#![cfg(unix)]
#![expect(
    clippy::significant_drop_tightening,
    reason = "the registry remains available through execution"
)]

use fleet_core::{Configured, JsonObject};
use fleet_kit::{WorkspaceCommand, WorkspaceContext, WorkspaceExit, Workspaces as _};
use fleet_wasm::{Wasm, WasmCapabilities, WasmMount, WasmRegistry};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Loads the host plugin from `FLEET_TEST_WASM`, or builds it if that is unset.
fn module() -> Vec<u8> {
    let directory = std::env::var_os("FLEET_TEST_WASM").map_or_else(build, PathBuf::from);
    fs::read(directory.join("fleet_plugin_host.wasm")).unwrap()
}

fn build() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let target = root.join("target/process-fixture");
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "build",
            "--offline",
            "--locked",
            "-p",
            "fleet-plugin-host",
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

#[test]
fn commands_require_the_process_grant() {
    let directory = std::env::temp_dir().join(format!("fleet-process-{}", std::process::id()));
    drop(fs::remove_dir_all(&directory));
    let wasm = Wasm::new().unwrap();
    let component = wasm.compile(&module()).unwrap();
    let mut registry = WasmRegistry::new();
    for (name, process) in [("granted", true), ("denied", false)] {
        let state = directory.join(name);
        fs::create_dir_all(&state).unwrap();
        let capabilities = WasmCapabilities {
            filesystem: vec![
                WasmMount {
                    host_path: directory.clone(),
                    guest_path: directory.to_str().unwrap().into(),
                    read_only: false,
                },
                WasmMount {
                    host_path: state,
                    guest_path: "/state".into(),
                    read_only: false,
                },
            ],
            process,
            ..WasmCapabilities::default()
        };
        let provider = wasm
            .provider(&component, &JsonObject::new(), capabilities)
            .unwrap();
        registry.insert(name, provider).unwrap();
    }
    let context = |name: &str| WorkspaceContext {
        workspace: Configured {
            name: format!("{name}:default").into(),
            config: json!({ "cwd": directory }).as_object().unwrap().clone(),
        },
        agent_id: 1.into(),
        id: 1.into(),
    };
    let command = |name: &str| WorkspaceCommand {
        id: name.into(),
        program: "/bin/sh".into(),
        args: vec!["-c".into(), format!("echo {name} >> ran")],
        env: BTreeMap::new(),
        cwd: None,
        stdin: Vec::new(),
        timeout_ms: 5000,
        output_limit: 1024,
    };

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for name in ["denied", "granted"] {
                registry.open(&context(name)).await.unwrap();
            }
            let error = registry
                .exec(&context("denied"), false, &command("denied"))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "this provider was not granted process");
            let output = registry
                .exec(&context("granted"), false, &command("granted"))
                .await
                .unwrap();
            assert_eq!(output.exit, WorkspaceExit::Code(0));
        });

    let ran = fs::read_to_string(directory.join("ran")).unwrap();
    fs::remove_dir_all(&directory).unwrap();
    assert_eq!(ran, "granted\n");
}
