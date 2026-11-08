pub mod models {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "model-provider",
    });
}

pub use models::fleet::providers::{configuration, types};

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
