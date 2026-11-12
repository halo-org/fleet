//! Basic WebAssembly tools backed by workspace providers.

mod bash;
mod bindings;
mod config;
mod files;

pub(crate) use self::{
    bindings::{
        exports::fleet::providers::tools::{Guest, ToolsError, ToolsErrorKind},
        fleet::providers::{
            blobs::{self, BlobsError, BlobsErrorKind},
            types::{AgentId, BlobId, ContentBlock, Tool, ToolContext, ToolOutput},
            workspaces::{self, WorkspaceContext, WorkspacesError, WorkspacesErrorKind},
        },
    },
    config::Config,
};

struct Tools;

#[derive(Debug)]
enum Error {
    Input(String),
    Workspace(WorkspacesError),
    Blobs(BlobsError),
}

impl Guest for Tools {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Tool>, ToolsError> {
        Ok(match name.as_str() {
            "bash" => Some(bash::metadata()),
            "read" | "write" | "edit" => files::metadata(&name),
            _ => None,
        })
    }

    async fn execute(
        context: ToolContext,
        is_retry: bool,
        input: String,
    ) -> Result<ToolOutput, ToolsError> {
        if !matches!(
            context.tool.name.as_str(),
            "bash" | "read" | "write" | "edit"
        ) {
            return Ok(error_output("The tool is not available."));
        }
        let config = Config::load(&context)?;
        let workspace = crate::config::workspace(&context)?;
        workspaces::open(workspace.clone())
            .await
            .map_err(service_error)?;
        let result = match context.tool.name.as_str() {
            "bash" => bash::execute(&context, &config, workspace, is_retry, &input).await,
            "read" => files::read(&context, &config, workspace, &input).await,
            "write" => files::write(workspace, is_retry, &input).await,
            "edit" => files::edit(&config, workspace, is_retry, &input).await,
            _ => return Ok(error_output("The tool is not available.")),
        };
        match result {
            Ok(output) => Ok(output),
            Err(Error::Input(message)) => Ok(error_output(message)),
            Err(Error::Workspace(error)) => match error.kind {
                WorkspacesErrorKind::UnsafeToRetry => Err(service_error(error)),
                _ => Ok(error_output(error.message)),
            },
            Err(Error::Blobs(error)) => Err(ToolsError {
                kind: ToolsErrorKind::Other,
                message: format!("failed to store image: {}", error.message),
            }),
        }
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn interrupted_output(
        _context: ToolContext,
        _input: String,
    ) -> Result<ToolOutput, ToolsError> {
        Ok(error_output(
            "The tool call was interrupted. Its effects may have already occurred.",
        ))
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Input(error.to_string())
    }
}

impl From<WorkspacesError> for Error {
    fn from(error: WorkspacesError) -> Self {
        Self::Workspace(error)
    }
}

impl From<BlobsError> for Error {
    fn from(error: BlobsError) -> Self {
        match error.kind {
            BlobsErrorKind::TooLarge => {
                Self::Input("The image is too large. Resize it with Bash before reading it.".into())
            }
            BlobsErrorKind::InvalidData => Self::Input("The image is invalid.".into()),
            _ => Self::Blobs(error),
        }
    }
}

fn service_error(error: WorkspacesError) -> ToolsError {
    ToolsError {
        kind: match error.kind {
            WorkspacesErrorKind::UnsafeToRetry => ToolsErrorKind::UnsafeToRetry,
            _ => ToolsErrorKind::Other,
        },
        message: error.message,
    }
}

fn success_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(text.into())],
        structured_content: None,
        is_error: false,
    }
}

fn error_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        is_error: true,
        ..success_output(text)
    }
}

fn metadata(name: &str, title: &str, description: &str, schema: &serde_json::Value) -> Tool {
    Tool {
        name: name.into(),
        description: Some(description.into()),
        is_async: false,
        input_schema: schema.to_string(),
        output_schema: None,
        extensions: serde_json::json!({
            "fleet.title": title,
        })
        .to_string(),
    }
}

#[cfg(target_arch = "wasm32")]
bindings::export!(Tools with_types_in bindings);
