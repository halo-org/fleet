pub mod models {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:providers/model-provider",
    });
}

pub use models::fleet::common::types;
pub use models::fleet::host::configuration;

pub mod blob_host {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:host/blob-host",
        with: { "fleet:common/types": super::types },
    });
}

pub use blob_host::fleet::host::blobs;

pub mod role_host {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:host/role-host",
        with: { "fleet:common/types": super::types },
    });
}

pub use role_host::fleet::host::roles;

pub mod agent_host {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:host/agent-host",
        with: { "fleet:common/types": super::types },
    });
}

pub use agent_host::fleet::host::agents;

pub mod tool_call_host {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:host/tool-call-host",
        with: { "fleet:common/types": super::types },
    });
}

pub use tool_call_host::fleet::host::tool_calls;

pub mod authentication {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:providers/authentication-provider",
        with: { "fleet:host/configuration": super::configuration },
    });
}

pub mod credential_host {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:host/credential-host",
    });
}

pub use credential_host::fleet::host::credentials;

pub mod process_host {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:host/process-host",
        imports: { default: store | trappable },
        with: { "fleet:host/process.child": crate::wasm_process::WasmProcess },
    });
}

pub use process_host::fleet::host::process;

pub mod worker_host {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:host/worker-host",
        imports: { default: store | trappable },
        with: {
            "fleet:host/workers.connection": crate::wasm_worker_connection::WasmWorkerConnection,
            "fleet:host/workers.listener": crate::wasm_workers::WasmWorkerListener,
        },
    });
}

pub use worker_host::fleet::host::workers;

pub mod worker {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:providers/worker-provider",
        require_store_data_send: true,
        imports: { default: store | trappable },
        with: {
            "fleet:host/configuration": super::configuration,
            "fleet:host/workers": super::workers,
        },
    });
}

pub mod workspace_host {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:host/workspace-host",
        with: { "fleet:common/types": super::types },
    });
}

pub use workspace_host::fleet::host::workspace as workspace_access;

pub mod workspace_resolvers {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:providers/workspace-resolver-provider",
        require_store_data_send: true,
        with: {
            "fleet:host/configuration": super::configuration,
            "fleet:common/types": super::types,
        },
    });
}

pub mod tools {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:providers/tool-provider",
        with: {
            "fleet:host/configuration": super::configuration,
            "fleet:common/types": super::types,
        },
    });
}

pub mod prompts {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:providers/prompt-provider",
        with: {
            "fleet:host/configuration": super::configuration,
            "fleet:common/types": super::types,
        },
    });
}

pub mod workspaces {
    wasmtime::component::bindgen!({
        path: ["wit/common", "wit/host", "wit/providers"],
        world: "fleet:providers/workspace-provider",
        with: {
            "fleet:host/configuration": super::configuration,
            "fleet:common/types": super::types,
        },
    });
}
