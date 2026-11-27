pub mod models {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "model-provider",
    });
}

pub use models::fleet::providers::{configuration, types};

pub mod blob_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "blob-host",
        with: { "fleet:providers/types": super::types },
    });
}

pub use blob_host::fleet::providers::blobs;

pub mod role_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "role-host",
        with: { "fleet:providers/types": super::types },
    });
}

pub use role_host::fleet::providers::roles;

pub mod agent_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "agent-host",
        with: { "fleet:providers/types": super::types },
    });
}

pub use agent_host::fleet::providers::agents;

pub mod tool_call_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "tool-call-host",
        with: { "fleet:providers/types": super::types },
    });
}

pub use tool_call_host::fleet::providers::tool_calls;

pub mod authentication {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "authentication-provider",
        with: { "fleet:providers/configuration": super::configuration },
    });
}

pub mod credential_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "credential-host",
    });
}

pub use credential_host::fleet::providers::credentials;

pub mod process_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "process-host",
        imports: { default: store | trappable },
        with: { "fleet:providers/process.child": crate::wasm_process::WasmProcess },
    });
}

pub use process_host::fleet::providers::process;

pub mod worker_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "worker-host",
        imports: { default: store | trappable },
        with: {
            "fleet:providers/workers.connection": crate::wasm_worker_connection::WasmWorkerConnection,
            "fleet:providers/workers.listener": crate::wasm_workers::WasmWorkerListener,
        },
    });
}

pub use worker_host::fleet::providers::workers;

pub mod worker {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "worker-provider",
        require_store_data_send: true,
        imports: { default: store | trappable },
        with: {
            "fleet:providers/configuration": super::configuration,
            "fleet:providers/workers": super::workers,
        },
    });
}

pub mod workspace_access_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "workspace-access-host",
        with: { "fleet:providers/types": super::types },
    });
}

pub use workspace_access_host::fleet::providers::workspace_access;

pub mod workspace_resolvers {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "workspace-resolver-provider",
        require_store_data_send: true,
        with: {
            "fleet:providers/configuration": super::configuration,
            "fleet:providers/types": super::types,
            "fleet:providers/workspaces": super::workspace_access_host::fleet::providers::workspaces,
        },
    });
}

pub mod tools {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "tool-provider",
        with: {
            "fleet:providers/configuration": super::configuration,
            "fleet:providers/types": super::types,
        },
    });
}

pub mod prompts {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "prompt-provider",
        with: {
            "fleet:providers/configuration": super::configuration,
            "fleet:providers/types": super::types,
        },
    });
}

pub mod workspaces {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "workspace-provider",
        with: {
            "fleet:providers/configuration": super::configuration,
            "fleet:providers/types": super::types,
        },
    });
}
