use crate::{
    Config, Directory, Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceOutput,
    WorkspaceWrite, WorkspacesError, WorkspacesErrorKind,
    bindings::exports::fleet::providers::workspaces::Guest,
};

pub struct Host;

impl Guest for Host {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Workspace>, WorkspacesError> {
        if name != "default" {
            return Ok(None);
        }
        Ok(Some(Workspace {
            name,
            extensions: serde_json::json!({
                "fleet": {
                    "title": "Host",
                    "description": "Run commands and access files on the Fleet host.",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "cwd": {
                                "type": "string",
                                "title": "Working directory",
                            },
                        },
                        "required": ["cwd"],
                        "additionalProperties": false,
                    },
                },
            })
            .to_string(),
        }))
    }

    async fn open(context: WorkspaceContext) -> Result<(), WorkspacesError> {
        let config = config(&context)?;
        Directory::open(&config.cwd, false).await?;
        let state = Directory::open("/state", false).await?;
        let id = binding_name(&context);
        match check_binding(&state, &id, &config.cwd).await {
            Err(WorkspacesError {
                kind: WorkspacesErrorKind::NotFound,
                message: _,
            }) => {}
            result => return result,
        }
        state
            .publish(&id, config.cwd.as_bytes().to_vec(), false)
            .await?;
        check_binding(&state, &id, &config.cwd).await
    }

    async fn read(
        context: WorkspaceContext,
        path: String,
        limit: u64,
    ) -> Result<Vec<u8>, WorkspacesError> {
        let config = load(&context).await?;
        crate::filesystem::read(&config.path(&path)?, limit).await
    }

    async fn write(context: WorkspaceContext, file: WorkspaceWrite) -> Result<(), WorkspacesError> {
        let config = load(&context).await?;
        let path = config.path(&file.path)?;
        let (parent, name) = path.rsplit_once('/').expect("absolute path has a root");
        if matches!(name, "" | "." | "..") {
            return Err(crate::error(
                WorkspacesErrorKind::InvalidConfiguration,
                "path must name a file",
            ));
        }
        if let Some(expected) = file.expected {
            let limit = u64::try_from(expected.len()).expect("byte length fits in u64");
            match crate::filesystem::read(&path, limit).await {
                Ok(bytes) if bytes == expected => {}
                Ok(_)
                | Err(WorkspacesError {
                    kind: WorkspacesErrorKind::FileNotFound | WorkspacesErrorKind::FileTooLarge,
                    message: _,
                }) => {
                    return Err(crate::error(
                        WorkspacesErrorKind::FileChanged,
                        "file does not match expected contents",
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        let directory = Directory::open(&format!("{parent}/"), file.create_parents).await?;
        directory.publish(name, file.content, true).await?;
        Ok(())
    }

    async fn exec(
        context: WorkspaceContext,
        is_retry: bool,
        command: WorkspaceCommand,
    ) -> Result<WorkspaceOutput, WorkspacesError> {
        if is_retry {
            return Err(crate::error(
                WorkspacesErrorKind::UnsafeToRetry,
                "the command may already have run",
            ));
        }
        let config = load(&context).await?;
        let cwd = config.path(command.cwd.as_deref().unwrap_or(""))?;
        crate::process::run(cwd, command).await
    }
}

fn config(context: &WorkspaceContext) -> Result<Config, WorkspacesError> {
    if context.workspace.name != "default" {
        return Err(crate::error(
            WorkspacesErrorKind::NotFound,
            "workspace was not found",
        ));
    }
    Config::parse(&context.workspace.config)
}

fn binding_name(context: &WorkspaceContext) -> String {
    format!("{:016x}{:016x}", context.id.high, context.id.low)
}

async fn check_binding(state: &Directory, id: &str, cwd: &str) -> Result<(), WorkspacesError> {
    let limit = u64::try_from(cwd.len()).expect("path length fits in u64");
    match state.read(id, limit).await {
        Ok(value) if value == cwd.as_bytes() => Ok(()),
        Ok(_)
        | Err(WorkspacesError {
            kind: WorkspacesErrorKind::FileTooLarge,
            message: _,
        }) => Err(crate::error(
            WorkspacesErrorKind::InvalidConfiguration,
            "workspace ID is already bound to a different directory",
        )),
        Err(WorkspacesError {
            kind: WorkspacesErrorKind::FileNotFound,
            message: _,
        }) => Err(crate::error(
            WorkspacesErrorKind::NotFound,
            "workspace has not been opened",
        )),
        Err(error) => Err(error),
    }
}

async fn load(context: &WorkspaceContext) -> Result<Config, WorkspacesError> {
    let config = config(context)?;
    let state = Directory::open("/state", false).await?;
    check_binding(&state, &binding_name(context), &config.cwd).await?;
    Ok(config)
}
