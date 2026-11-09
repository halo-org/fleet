use crate::bindings::{
    models::exports::fleet::providers::models, tools::exports::fleet::providers::tools,
    workspaces::exports::fleet::providers::workspaces,
};
use core::fmt;
use fleet_core::{ModelName, ModelsErrorKind, PromptName, ToolName, ToolsErrorKind};
use fleet_kit::{WorkspaceName, WorkspacesErrorKind};

/// An error returned by the WebAssembly host or a provider.
#[derive(Debug)]
pub struct WasmError(Kind);

#[derive(Debug)]
enum Kind {
    Runtime(wasmtime::Error),
    Json(serde_json::Error),
    InvalidNamespace(String),
    DuplicateNamespace(String),
    NoProviderInterfaces,
    ModelNotFound(ModelName),
    ToolNotFound(ToolName),
    PromptNotFound(PromptName),
    WorkspaceNotFound(WorkspaceName),
    Provider(String),
    Model {
        kind: ModelsErrorKind,
        message: String,
    },
    Tool {
        kind: ToolsErrorKind,
        message: String,
    },
    Workspace {
        kind: WorkspacesErrorKind,
        message: String,
    },
    IncompleteResponse,
}

impl WasmError {
    pub(crate) const fn runtime(error: wasmtime::Error) -> Self {
        Self(Kind::Runtime(error))
    }

    pub(crate) fn invalid_namespace(namespace: &str) -> Self {
        Self(Kind::InvalidNamespace(namespace.into()))
    }

    pub(crate) const fn duplicate_namespace(namespace: String) -> Self {
        Self(Kind::DuplicateNamespace(namespace))
    }

    pub(crate) const fn no_provider_interfaces() -> Self {
        Self(Kind::NoProviderInterfaces)
    }

    pub(crate) fn model_not_found(name: &ModelName) -> Self {
        Self(Kind::ModelNotFound(name.clone()))
    }

    pub(crate) fn tool_not_found(name: &ToolName) -> Self {
        Self(Kind::ToolNotFound(name.clone()))
    }

    pub(crate) fn prompt_not_found(name: &PromptName) -> Self {
        Self(Kind::PromptNotFound(name.clone()))
    }

    pub(crate) fn workspace_not_found(name: &WorkspaceName) -> Self {
        Self(Kind::WorkspaceNotFound(name.clone()))
    }

    pub(crate) const fn provider(message: String) -> Self {
        Self(Kind::Provider(message))
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

    pub(crate) fn from_workspace(error: workspaces::WorkspacesError) -> Self {
        Self(Kind::Workspace {
            kind: match error.kind {
                workspaces::WorkspacesErrorKind::NotFound => WorkspacesErrorKind::NotFound,
                workspaces::WorkspacesErrorKind::InvalidConfiguration => {
                    WorkspacesErrorKind::InvalidConfiguration
                }
                workspaces::WorkspacesErrorKind::FileNotFound => WorkspacesErrorKind::FileNotFound,
                workspaces::WorkspacesErrorKind::FileTooLarge => WorkspacesErrorKind::FileTooLarge,
                workspaces::WorkspacesErrorKind::FileChanged => WorkspacesErrorKind::FileChanged,
                workspaces::WorkspacesErrorKind::UnsafeToRetry => {
                    WorkspacesErrorKind::UnsafeToRetry
                }
                workspaces::WorkspacesErrorKind::Other => WorkspacesErrorKind::Other,
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
            Kind::InvalidNamespace(namespace) => {
                write!(f, "invalid provider namespace \"{namespace}\"")
            }
            Kind::DuplicateNamespace(namespace) => {
                write!(f, "provider namespace {namespace} is already registered")
            }
            Kind::NoProviderInterfaces => {
                f.write_str("component does not export models, tools, prompts or workspaces")
            }
            Kind::ModelNotFound(name) => write!(f, "model {name} was not found"),
            Kind::ToolNotFound(name) => write!(f, "tool {name} was not found"),
            Kind::PromptNotFound(name) => write!(f, "prompt {name} was not found"),
            Kind::WorkspaceNotFound(name) => write!(f, "workspace {name} was not found"),
            Kind::Provider(message)
            | Kind::Model { message, kind: _ }
            | Kind::Tool { message, kind: _ }
            | Kind::Workspace { message, kind: _ } => f.write_str(message),
            Kind::IncompleteResponse => f.write_str("model response is incomplete"),
        }
    }
}

impl core::error::Error for WasmError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.0 {
            Kind::Runtime(error) => Some(error.as_ref()),
            Kind::Json(error) => Some(error),
            Kind::InvalidNamespace(_)
            | Kind::DuplicateNamespace(_)
            | Kind::NoProviderInterfaces
            | Kind::ModelNotFound(_)
            | Kind::ToolNotFound(_)
            | Kind::PromptNotFound(_)
            | Kind::WorkspaceNotFound(_)
            | Kind::Provider(_)
            | Kind::Model {
                kind: _,
                message: _,
            }
            | Kind::Tool {
                kind: _,
                message: _,
            }
            | Kind::Workspace {
                kind: _,
                message: _,
            }
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
            Kind::Workspace {
                kind: WorkspacesErrorKind::UnsafeToRetry,
                message: _,
            } => ToolsErrorKind::UnsafeToRetry,
            _ => ToolsErrorKind::Other,
        }
    }
}

impl fleet_core::Error<WorkspacesErrorKind> for WasmError {
    fn kind(&self) -> WorkspacesErrorKind {
        match &self.0 {
            Kind::WorkspaceNotFound(_) => WorkspacesErrorKind::NotFound,
            Kind::Workspace { kind, message: _ } => *kind,
            _ => WorkspacesErrorKind::Other,
        }
    }
}

impl From<serde_json::Error> for WasmError {
    fn from(error: serde_json::Error) -> Self {
        Self(Kind::Json(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::Error as _;

    #[test]
    fn workspace_errors_preserve_retry_safety() {
        let error = WasmError::from_workspace(workspaces::WorkspacesError {
            kind: workspaces::WorkspacesErrorKind::UnsafeToRetry,
            message: "The command may still be running".into(),
        });
        let kind: WorkspacesErrorKind = error.kind();
        let tool_kind: ToolsErrorKind = error.kind();

        assert_eq!(kind, WorkspacesErrorKind::UnsafeToRetry);
        assert_eq!(tool_kind, ToolsErrorKind::UnsafeToRetry);
        assert_eq!(error.to_string(), "The command may still be running");
    }
}
