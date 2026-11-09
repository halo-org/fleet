pub mod models {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "model-provider",
    });
}

pub use models::fleet::providers::{configuration, types};

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

pub mod workspace_host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "workspace-host",
        with: { "fleet:providers/types": super::types },
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
