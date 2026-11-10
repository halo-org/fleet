use core::fmt;
use fleet_core::{BlobId, ModelsErrorKind};
use http::StatusCode;

/// An error from an [`OpenAiModels`](crate::OpenAiModels) provider.
#[derive(Debug)]
pub struct OpenAiError {
    kind: ModelsErrorKind,
    detail: Detail,
}

#[derive(Debug)]
enum Detail {
    Message(&'static str),
    Source(&'static str, Box<dyn core::error::Error + Send + Sync>),
    Image {
        id: BlobId,
        source: Box<dyn core::error::Error + Send + Sync>,
    },
    Api {
        status: Option<StatusCode>,
        code: Option<String>,
        message: Option<String>,
        request_id: Option<String>,
    },
}

impl OpenAiError {
    pub(crate) const fn new(kind: ModelsErrorKind, message: &'static str) -> Self {
        Self {
            kind,
            detail: Detail::Message(message),
        }
    }

    pub(crate) fn source(
        message: &'static str,
        source: impl core::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: ModelsErrorKind::Other,
            detail: Detail::Source(message, Box::new(source)),
        }
    }

    pub(crate) fn blob(
        kind: ModelsErrorKind,
        id: BlobId,
        source: impl core::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            detail: Detail::Image {
                id,
                source: Box::new(source),
            },
        }
    }

    pub(crate) const fn api(
        status: Option<StatusCode>,
        code: Option<String>,
        message: Option<String>,
        request_id: Option<String>,
        kind: ModelsErrorKind,
    ) -> Self {
        Self {
            kind,
            detail: Detail::Api {
                status,
                code,
                message,
                request_id,
            },
        }
    }

    pub(crate) fn is_unsupported_route(&self) -> bool {
        matches!(
            &self.detail,
            Detail::Api {
                status: Some(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
                code: Some(code),
                ..
            } if matches!(code.as_str(),
                "hardened_oauth_rule_missing" | "subscription_sharing_route_not_supported"
            )
        )
    }
}

impl fmt::Display for OpenAiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Detail::Image { id, source: _ } => write!(f, "could not load image {id}"),
            Detail::Message(message) | Detail::Source(message, _) => f.write_str(message),
            Detail::Api {
                status,
                code,
                message,
                request_id,
            } => {
                f.write_str("OpenAI request failed")?;
                if let Some(status) = status {
                    write!(f, " with HTTP {status}")?;
                }
                if let Some(code) = code {
                    write!(f, " ({code})")?;
                }
                if let Some(request_id) = request_id {
                    write!(f, " for request {request_id}")?;
                }
                if let Some(message) = message {
                    write!(f, ": {message}")?;
                }
                Ok(())
            }
        }
    }
}

impl core::error::Error for OpenAiError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.detail {
            Detail::Source(_, source) | Detail::Image { source, .. } => Some(source.as_ref()),
            Detail::Message(_) | Detail::Api { .. } => None,
        }
    }
}

impl fleet_core::Error<ModelsErrorKind> for OpenAiError {
    fn kind(&self) -> ModelsErrorKind {
        self.kind
    }
}
