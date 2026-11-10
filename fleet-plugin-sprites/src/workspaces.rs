use crate::{
    Config, Sprite, Sprites, Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit,
    WorkspaceOutput, WorkspaceWrite, WorkspacesError, WorkspacesErrorKind,
    bindings::{self, exports::fleet::providers::workspaces::Guest},
};

const MARKER: &str = "/home/sprite/.fleet/workspace";
const SETUP_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const SETUP_OUTPUT_LIMIT: u64 = 16 * 1024;

impl Guest for Sprites {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Workspace>, WorkspacesError> {
        if name != "default" {
            return Ok(None);
        }
        Ok(Some(Workspace {
            name,
            extensions: serde_json::json!({
                "fleet": {
                    "title": "Sprites",
                    "description": "Run commands and access files in a Sprites VM.",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "setup": {
                                "type": "string",
                                "title": "Setup script",
                                "description": "Runs with bash when a sprite is first opened.",
                            },
                        },
                        "additionalProperties": false,
                    },
                },
            })
            .to_string(),
        }))
    }

    async fn open(context: WorkspaceContext) -> Result<(), WorkspacesError> {
        let (config, sprite) = load(&context)?;
        let limit = 1024 * 1024;
        match sprite.read(MARKER, limit).await {
            Ok(marker) => {
                return if Config::parse(&String::from_utf8_lossy(&marker)).ok() == Some(config) {
                    Ok(())
                } else {
                    Err(crate::error(
                        WorkspacesErrorKind::InvalidConfiguration,
                        "workspace ID is already bound to a different configuration",
                    ))
                };
            }
            Err(WorkspacesError {
                kind: WorkspacesErrorKind::FileNotFound,
                message: _,
            }) => {}
            Err(WorkspacesError {
                kind: WorkspacesErrorKind::NotFound,
                message: _,
            }) => sprite.create().await?,
            Err(error) => return Err(error),
        }
        if let Some(setup) = &config.setup {
            let output = run(
                &sprite,
                WorkspaceCommand {
                    id: "setup".into(),
                    program: "bash".into(),
                    args: vec!["-lc".into(), setup.clone()],
                    env: Vec::new(),
                    cwd: None,
                    stdin: Vec::new(),
                    timeout_ms: SETUP_TIMEOUT_MS,
                    output_limit: SETUP_OUTPUT_LIMIT,
                },
            )
            .await?;
            let failure = match output.exit {
                WorkspaceExit::Code(0) => None,
                WorkspaceExit::Code(code) => Some(format!("exited with code {code}")),
                WorkspaceExit::Terminated => Some("was terminated".to_owned()),
                WorkspaceExit::TimedOut => Some("timed out".to_owned()),
            };
            if let Some(failure) = failure {
                return Err(crate::error(
                    WorkspacesErrorKind::Other,
                    format!(
                        "setup {failure}: {}",
                        String::from_utf8_lossy(&output.stderr).trim(),
                    ),
                ));
            }
        }
        let marker = serde_json::to_vec(&config).expect("config serializes");
        replace(&sprite, MARKER, marker, true).await
    }

    async fn read(
        context: WorkspaceContext,
        path: String,
        limit: u64,
    ) -> Result<Vec<u8>, WorkspacesError> {
        let (_, sprite) = load(&context)?;
        sprite.read(&crate::config::path(&path)?, limit).await
    }

    async fn write(context: WorkspaceContext, file: WorkspaceWrite) -> Result<(), WorkspacesError> {
        let (_, sprite) = load(&context)?;
        let path = crate::config::path(&file.path)?;
        if let Some(expected) = file.expected {
            let limit = u64::try_from(expected.len()).expect("byte length fits in u64");
            match sprite.read(&path, limit).await {
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
        replace(&sprite, &path, file.content, file.create_parents).await
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
        let (_, sprite) = load(&context)?;
        run(&sprite, command).await
    }
}

fn load(context: &WorkspaceContext) -> Result<(Config, Sprite), WorkspacesError> {
    if context.workspace.name != "default" {
        return Err(crate::error(
            WorkspacesErrorKind::NotFound,
            "workspace was not found",
        ));
    }
    let config = Config::parse(&context.workspace.config)?;
    Ok((config, Sprite::new(&context.id)))
}

async fn replace(
    sprite: &Sprite,
    path: &str,
    content: Vec<u8>,
    create_parents: bool,
) -> Result<(), WorkspacesError> {
    let (parent, name) = path.rsplit_once('/').expect("absolute path has a root");
    if matches!(name, "" | "." | "..") {
        return Err(crate::error(
            WorkspacesErrorKind::InvalidConfiguration,
            "path must name a file",
        ));
    }
    let random = bindings::wasi::random::random::get_random_u64();
    let temporary = format!("{parent}/.{name}.fleet-{random:016x}");
    sprite.write(&temporary, content, create_parents).await?;
    sprite.rename(&temporary, path).await
}

async fn run(
    sprite: &Sprite,
    command: WorkspaceCommand,
) -> Result<WorkspaceOutput, WorkspacesError> {
    let cwd = crate::config::path(command.cwd.as_deref().unwrap_or(""))?;
    let limit = command.output_limit;
    let timeout_ms = command.timeout_ms;
    let response = sprite
        .exec(crate::exec::body(command, &cwd)?, timeout_ms)
        .await?;
    crate::exec::parse(&response, limit)
}
