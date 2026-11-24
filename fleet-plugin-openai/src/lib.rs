//! A WebAssembly plugin providing [OpenAI](https://openai.com) models and [ChatGPT](https://chatgpt.com) sign-in.

mod authentication;
mod bindings;
mod config;
mod convert;
mod host_blobs;
mod models;
mod wasi_http;

pub(crate) use self::{
    authentication::{CredentialsError, HostCredentials},
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

        let signed_out =
            core::iter::successors(core::error::Error::source(&error), |error| error.source())
                .any(|error| matches!(error.downcast_ref(), Some(CredentialsError::SignedOut)));

        Self {
            kind: match error.kind() {
                _ if signed_out => ModelsErrorKind::Rejected,
                Kind::InvalidContinuation => ModelsErrorKind::InvalidContinuation,
                Kind::InvalidCompaction => ModelsErrorKind::InvalidCompaction,
                Kind::ContextTooLong => ModelsErrorKind::ContextTooLong,
                Kind::Rejected => ModelsErrorKind::Rejected,
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use fleet_core::{Configured, JsonObject, ModelContext, Models as _};
    use fleet_models::{Credentials, Http, OpenAiConfig, OpenAiModels};
    use futures_util::{FutureExt as _, stream};
    use http::{HeaderMap, Request, Response};

    struct NoHttp;

    impl Http for NoHttp {
        type Error = CredentialsError;
        type Body = stream::Empty<Result<Bytes, CredentialsError>>;

        fn send(
            &self,
            _: Request<Bytes>,
        ) -> impl Future<Output = Result<Response<Self::Body>, Self::Error>> + Send {
            core::future::ready(Err(CredentialsError::Host("unreachable".into())))
        }
    }

    struct Failing(fn() -> CredentialsError);

    impl Credentials for Failing {
        type Error = CredentialsError;

        fn get(&self) -> impl Future<Output = Result<HeaderMap, Self::Error>> + Send {
            core::future::ready(Err((self.0)()))
        }
    }

    #[test]
    fn signing_out_rejects_requests() {
        let context = ModelContext {
            model: Configured {
                name: "model".into(),
                config: JsonObject::new(),
            },
            agent_id: 1.into(),
            system_prompt: String::new(),
            tools: Vec::new(),
            role_extensions: JsonObject::new(),
        };
        let cases: [(fn() -> CredentialsError, _); 2] = [
            (|| CredentialsError::SignedOut, ModelsErrorKind::Rejected),
            (
                || CredentialsError::Host("unavailable".into()),
                ModelsErrorKind::Other,
            ),
        ];

        for (error, kind) in cases {
            let models =
                OpenAiModels::new(NoHttp, Failing(error), OpenAiConfig::default()).unwrap();

            let error = models
                .compact(&context, None, &[], None)
                .now_or_never()
                .unwrap()
                .unwrap_err();
            assert_eq!(ModelsError::from(error).kind, kind);
        }
    }
}
