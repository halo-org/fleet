pub mod roles {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "role-provider",
    });
}

pub use roles::fleet::providers::types;

pub mod models {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "model-provider",
        with: {
            "fleet:providers/types": super::types,
        },
    });
}

pub mod tools {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "tool-provider",
        with: {
            "fleet:providers/types": super::types,
        },
    });
}

pub mod prompts {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "prompt-provider",
        with: {
            "fleet:providers/types": super::types,
        },
    });
}
