use crate::bindings::{
    models::exports::fleet::providers::models, tools::exports::fleet::providers::tools,
};
use core::fmt;
use fleet_core::{ModelsErrorKind, RoleParameterName, ToolAlias, ToolsErrorKind};

/// An error returned by the WebAssembly host or a provider.
#[derive(Debug)]
pub struct WasmError(Kind);

#[derive(Debug)]
enum Kind {
    Runtime(wasmtime::Error),
    Json(serde_json::Error),
    Provider(String),
    Model {
        kind: ModelsErrorKind,
        message: String,
    },
    Tool {
        kind: ToolsErrorKind,
        message: String,
    },
    DuplicateRoleParameter(RoleParameterName),
    DuplicateToolAlias(ToolAlias),
    IncompleteResponse,
}

impl WasmError {
    pub(crate) const fn runtime(error: wasmtime::Error) -> Self {
        Self(Kind::Runtime(error))
    }

    pub(crate) const fn provider(message: String) -> Self {
        Self(Kind::Provider(message))
    }

    pub(crate) const fn duplicate_role_parameter(name: RoleParameterName) -> Self {
        Self(Kind::DuplicateRoleParameter(name))
    }

    pub(crate) const fn duplicate_tool_alias(alias: ToolAlias) -> Self {
        Self(Kind::DuplicateToolAlias(alias))
    }

    pub(crate) const fn incomplete_response() -> Self {
        Self(Kind::IncompleteResponse)
    }

    pub(crate) fn from_model(error: models::ModelsError) -> Self {
        Self(Kind::Model {
            kind: match error.kind {
                models::ModelsErrorKind::InvalidContinuation => {
                    ModelsErrorKind::InvalidContinuation
                }
                models::ModelsErrorKind::InvalidCompaction => ModelsErrorKind::InvalidCompaction,
                models::ModelsErrorKind::ContextTooLong => ModelsErrorKind::ContextTooLong,
                models::ModelsErrorKind::Other => ModelsErrorKind::Other,
            },
            message: error.message,
        })
    }

    pub(crate) fn from_tool(error: tools::ToolsError) -> Self {
        Self(Kind::Tool {
            kind: match error.kind {
                tools::ToolsErrorKind::UnsafeToRetry => ToolsErrorKind::UnsafeToRetry,
                tools::ToolsErrorKind::Other => ToolsErrorKind::Other,
            },
            message: error.message,
        })
    }
}

impl fmt::Display for WasmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Kind::Runtime(_) => f.write_str("WebAssembly operation failed"),
            Kind::Json(_) => f.write_str("WebAssembly provider returned an invalid JSON object"),
            Kind::Provider(message)
            | Kind::Model { message, kind: _ }
            | Kind::Tool { message, kind: _ } => f.write_str(message),
            Kind::DuplicateRoleParameter(name) => write!(f, "role repeats parameter {name}"),
            Kind::DuplicateToolAlias(alias) => write!(f, "role repeats tool alias {alias}"),
            Kind::IncompleteResponse => f.write_str("model response is incomplete"),
        }
    }
}

impl core::error::Error for WasmError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.0 {
            Kind::Runtime(error) => Some(error.as_ref()),
            Kind::Json(error) => Some(error),
            Kind::Provider(_)
            | Kind::Model {
                kind: _,
                message: _,
            }
            | Kind::Tool {
                kind: _,
                message: _,
            }
            | Kind::DuplicateRoleParameter(_)
            | Kind::DuplicateToolAlias(_)
            | Kind::IncompleteResponse => None,
        }
    }
}

impl fleet_core::Error<ModelsErrorKind> for WasmError {
    fn kind(&self) -> ModelsErrorKind {
        match &self.0 {
            Kind::Model { kind, message: _ } => *kind,
            _ => ModelsErrorKind::Other,
        }
    }
}

impl fleet_core::Error<ToolsErrorKind> for WasmError {
    fn kind(&self) -> ToolsErrorKind {
        match &self.0 {
            Kind::Tool { kind, message: _ } => *kind,
            _ => ToolsErrorKind::Other,
        }
    }
}

impl From<serde_json::Error> for WasmError {
    fn from(error: serde_json::Error) -> Self {
        Self(Kind::Json(error))
    }
}
