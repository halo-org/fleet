use crate::{
    Config, Sprite, Sprites, Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit,
    WorkspaceOutput, WorkspaceWrite, WorkspacesError, WorkspacesErrorKind,
    bindings::{self, exports::fleet::providers::workspaces::Guest},
    config::HOME,
};

const SPRITE_MARKER: &str = "/home/sprite/.fleet/sprite";
const WORKSPACE_MARKERS: &str = "/home/sprite/.fleet/workspaces";
const MARKER_LIMIT: u64 = 1024 * 1024;
const PROVISION_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const PROVISION_OUTPUT_LIMIT: u64 = 16 * 1024;

/// Clones a repository if needed and adds a worktree on a new branch from the latest default branch.
const REPO: &str = r#"set -euo pipefail
url=$1 repo=$2 tree=$3 branch=$4
temporary=/home/sprite/.fleet/tmp
mkdir -p -- "$temporary" "${repo%/*}" "${tree%/*}"
exec 9>/home/sprite/.fleet/repos.lock
flock 9
if [ -e "$tree" ]; then exit 0; fi
if [ ! -d "$repo" ]; then
  rm -rf -- "$temporary/repo"
  git clone --bare --quiet -- "$url" "$temporary/repo"
  git -C "$temporary/repo" config remote.origin.fetch '+refs/heads/*:refs/remotes/origin/*'
  mv -T -- "$temporary/repo" "$repo"
fi
git -C "$repo" fetch --quiet --prune origin
git -C "$repo" remote set-head origin --auto >/dev/null
staged=$temporary/${tree##*/}
rm -rf -- "$staged"
git -C "$repo" worktree prune
if git -C "$repo" show-ref --verify --quiet "refs/heads/$branch"; then
  git -C "$repo" worktree add --quiet "$staged" "$branch"
else
  git -C "$repo" worktree add --quiet --no-track -b "$branch" "$staged" origin/HEAD
fi
git -C "$repo" worktree move "$staged" "$tree"
"#;

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
                            "sprite": {
                                "type": "string",
                                "title": "Sprite",
                                "description": "Shares a named sprite instead of creating one per workspace.",
                            },
                            "setup": {
                                "type": "string",
                                "title": "Setup script",
                                "description": "Runs with bash when a sprite is first opened.",
                            },
                            "repo": {
                                "type": "string",
                                "title": "Repository",
                                "description": "Git URL to clone. Each workspace gets a worktree on its own branch.",
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
        let id = crate::hex(&context.id);
        let marker = format!("{WORKSPACE_MARKERS}/{id}");
        match sprite.read(&marker, MARKER_LIMIT).await {
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
        match sprite.read(SPRITE_MARKER, MARKER_LIMIT).await {
            Ok(setup) => {
                if serde_json::from_slice::<Option<String>>(&setup).ok()
                    != Some(config.setup.clone())
                {
                    return Err(crate::error(
                        WorkspacesErrorKind::InvalidConfiguration,
                        "sprite was already set up with a different script",
                    ));
                }
            }
            Err(WorkspacesError {
                kind: WorkspacesErrorKind::FileNotFound,
                message: _,
            }) => {
                if let Some(setup) = &config.setup {
                    provision(&sprite, "setup", vec!["-lc".into(), setup.clone()]).await?;
                }
                let setup = serde_json::to_vec(&config.setup).expect("setup serializes");
                replace(&sprite, SPRITE_MARKER, setup, true).await?;
            }
            Err(error) => return Err(error),
        }
        if let Some(repo) = &config.repo {
            let args = vec![
                "-c".into(),
                REPO.into(),
                "bash".into(),
                repo.clone(),
                crate::config::repository(repo),
                config.directory(&context.id),
                format!("fleet/{id}"),
            ];
            provision(&sprite, "repo", args).await?;
        }
        let bytes = serde_json::to_vec(&config).expect("config serializes");
        replace(&sprite, &marker, bytes, true).await
    }

    async fn read(
        context: WorkspaceContext,
        path: String,
        limit: u64,
    ) -> Result<Vec<u8>, WorkspacesError> {
        let (config, sprite) = load(&context)?;
        let path = crate::config::path(&config.directory(&context.id), &path)?;
        sprite.read(&path, limit).await
    }

    async fn write(context: WorkspaceContext, file: WorkspaceWrite) -> Result<(), WorkspacesError> {
        let (config, sprite) = load(&context)?;
        let path = crate::config::path(&config.directory(&context.id), &file.path)?;
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
        let (config, sprite) = load(&context)?;
        run(&sprite, &config.directory(&context.id), command).await
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
    let sprite = Sprite::new(config.sprite(&context.id));
    Ok((config, sprite))
}

async fn provision(sprite: &Sprite, name: &str, args: Vec<String>) -> Result<(), WorkspacesError> {
    let command = WorkspaceCommand {
        id: name.into(),
        program: "bash".into(),
        args,
        env: vec![("GIT_TERMINAL_PROMPT".into(), "0".into())],
        cwd: None,
        stdin: Vec::new(),
        timeout_ms: PROVISION_TIMEOUT_MS,
        output_limit: PROVISION_OUTPUT_LIMIT,
    };
    let output = run(sprite, HOME, command).await?;
    let failure = match output.exit {
        WorkspaceExit::Code(0) => return Ok(()),
        WorkspaceExit::Code(code) => format!("exited with code {code}"),
        WorkspaceExit::Terminated => "was terminated".to_owned(),
        WorkspaceExit::TimedOut => "timed out".to_owned(),
    };
    Err(crate::error(
        WorkspacesErrorKind::Other,
        format!(
            "{name} {failure}: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
    ))
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
    directory: &str,
    command: WorkspaceCommand,
) -> Result<WorkspaceOutput, WorkspacesError> {
    let cwd = crate::config::path(directory, command.cwd.as_deref().unwrap_or(""))?;
    let limit = command.output_limit;
    let timeout_ms = command.timeout_ms;
    let response = sprite
        .exec(crate::exec::body(command, &cwd)?, timeout_ms)
        .await?;
    crate::exec::parse(&response, limit)
}
