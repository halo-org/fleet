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
