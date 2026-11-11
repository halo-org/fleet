//! A WebAssembly plugin providing [OpenAI](https://openai.com) models and [ChatGPT](https://chatgpt.com) sign-in.

mod authentication;
mod bindings;
mod config;
mod convert;
mod host_blobs;
mod models;
mod wasi_http;

pub(crate) use self::{
    authentication::HostCredentials,
    bindings::exports::fleet::providers::models::{ModelsError, ModelsErrorKind},
    config::Config,
    host_blobs::HostBlobs,
    wasi_http::WasiHttp,
};

struct OpenAi;

#[cfg(target_arch = "wasm32")]
bindings::export!(OpenAi with_types_in bindings);

impl From<fleet_models::OpenAiError> for ModelsError {
    fn from(error: fleet_models::OpenAiError) -> Self {
        use fleet_core::{Error as _, ModelsErrorKind as Kind};

        Self {
            kind: match error.kind() {
                Kind::InvalidContinuation => ModelsErrorKind::InvalidContinuation,
                Kind::InvalidCompaction => ModelsErrorKind::InvalidCompaction,
                Kind::ContextTooLong => ModelsErrorKind::ContextTooLong,
                Kind::Other => ModelsErrorKind::Other,
            },
            message: error_chain(&error),
        }
    }
}

fn error(error: impl core::error::Error) -> ModelsError {
    ModelsError {
        kind: ModelsErrorKind::Other,
        message: error_chain(&error),
    }
}

fn error_chain(error: &dyn core::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}
