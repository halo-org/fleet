use crate::{
    Config, Sprite, Sprites, Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit,
    WorkspaceOutput, WorkspaceWrite, WorkspacesError, WorkspacesErrorKind,
    bindings::{self, exports::fleet::providers::workspaces::Guest},
    config::{Credential, HOME, ProviderConfig, Variable},
};
use std::collections::BTreeMap;

const WORKSPACE_MARKERS: &str = "/home/sprite/.fleet/workspaces";
const MARKER_LIMIT: u64 = 1024 * 1024;
const PROVISION_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const PROVISION_OUTPUT_LIMIT: u64 = 16 * 1024;

/// Runs a setup script unless it is the last one that succeeded on the sprite.
///
/// Uses no shell variables, so workspace variables reach the script unchanged.
const SETUP: &str = r#"set -euo pipefail
mkdir -p /home/sprite/.fleet
exec 9>/home/sprite/.fleet/setup.lock
flock 9
if printf '%s' "$1" | cmp -s - /home/sprite/.fleet/sprite; then exit 0; fi
bash -lc "$1" 9>&-
printf '%s' "$1" > /home/sprite/.fleet/sprite.tmp
mv -T -- /home/sprite/.fleet/sprite.tmp /home/sprite/.fleet/sprite
"#;

/// Clones a repository if needed and adds a worktree on a new branch from the latest default branch.
///
/// Takes the URL, bare clone, worktree and branch. Uses no shell variables, so workspace variables
/// reach git unchanged.
const REPO: &str = r#"set -euo pipefail
set -- "$@" "/home/sprite/.fleet/tmp/${3##*/}"
mkdir -p -- /home/sprite/.fleet/tmp "${2%/*}" "${3%/*}"
exec 9>/home/sprite/.fleet/repos.lock
flock 9
if [ -e "$3" ]; then exit 0; fi
if [ ! -d "$2" ]; then
  rm -rf -- /home/sprite/.fleet/tmp/repo
  git clone --bare --quiet -- "$1" /home/sprite/.fleet/tmp/repo
  git -C /home/sprite/.fleet/tmp/repo config remote.origin.fetch '+refs/heads/*:refs/remotes/origin/*'
  mv -T -- /home/sprite/.fleet/tmp/repo "$2"
fi
git -C "$2" fetch --quiet --prune origin
git -C "$2" remote set-head origin --auto >/dev/null
rm -rf -- "$5"
git -C "$2" worktree prune
if git -C "$2" show-ref --verify --quiet "refs/heads/$4"; then
  git -C "$2" worktree add --quiet "$5" "$4"
else
  git -C "$2" worktree add --quiet --no-track -b "$4" "$5" origin/HEAD
fi
git -C "$2" worktree move "$5" "$3"
"#;

/// Removes a workspace ID's record, worktrees and branches from a shared sprite.
///
/// Takes the ID. Holds the repository lock, so it does not interleave with provisioning. Removes
/// worktrees in the workspace directory and elsewhere on the ID's branch from each bare clone.
const RELEASE: &str = r#"set -euo pipefail
shopt -s nullglob dotglob
if [ ! -d /home/sprite/.fleet ]; then exit 0; fi
exec 9>/home/sprite/.fleet/repos.lock
flock 9
rm -f -- "/home/sprite/.fleet/workspaces/$1"
for repo in /home/sprite/repos/*/; do
  if [ "$(git --git-dir="$repo" rev-parse --is-bare-repository 2>/dev/null)" != true ]; then
    continue
  fi
  trees=()
  while read -r key value; do
    case $key in
      worktree)
        tree=$value
        case $tree/ in "/home/sprite/worktrees/$1/"*) trees+=("$tree") ;; esac
        ;;
      branch)
        if [ "$value" = "refs/heads/fleet/$1" ]; then trees+=("$tree"); fi
        ;;
    esac
  done < <(git --git-dir="$repo" worktree list --porcelain)
  for tree in "${trees[@]}"; do
    git --git-dir="$repo" worktree unlock "$tree" 2>/dev/null || true
    rm -rf -- "$tree"
  done
  git --git-dir="$repo" worktree prune
  if git --git-dir="$repo" show-ref --verify --quiet "refs/heads/fleet/$1"; then
    git --git-dir="$repo" branch --quiet -D "fleet/$1"
  fi
done
rm -rf -- "/home/sprite/worktrees/$1" "/home/sprite/.fleet/tmp/$1"
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
                "fleet.title": "Sprites",
                "fleet.description": "Run commands and access files in a Sprites VM.",
                "fleet.schema": {
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
                        "repos": {
                            "title": "Repositories",
                            "description": "Git URLs to clone. Each workspace gets a worktree of each on its own branch. A list puts each worktree in a subdirectory named after its repository.",
                            "oneOf": [
                                {"type": "string"},
                                {"type": "array", "items": {"type": "string"}, "minItems": 1},
                            ],
                        },
                        "env": {
                            "type": "object",
                            "title": "Environment",
                            "description": "Variables for setup, repositories and commands. A value is a string or the credential of a connection.",
                            "propertyNames": {"pattern": "^[A-Za-z_][A-Za-z0-9_]*$"},
                            "additionalProperties": {
                                "oneOf": [
                                    {"type": "string"},
                                    {
                                        "type": "object",
                                        "properties": {"connection": {"type": "string"}},
                                        "required": ["connection"],
                                        "additionalProperties": false,
                                    },
                                ],
                            },
                        },
                    },
                    "additionalProperties": false,
                },
            })
            .to_string(),
        }))
    }

    async fn open(context: WorkspaceContext) -> Result<(), WorkspacesError> {
        let (config, sprite) = load(&context)?;
        let id = crate::hex(&context.id);
        let marker = format!("{WORKSPACE_MARKERS}/{id}");
        let exists = match sprite.read(&marker, MARKER_LIMIT).await {
            Ok(marker)
                if Config::parse(&String::from_utf8_lossy(&marker))
                    .is_ok_and(|stored| stored == config) =>
            {
                return Ok(());
            }
            Ok(_)
            | Err(WorkspacesError {
                kind: WorkspacesErrorKind::FileNotFound,
                message: _,
            }) => true,
            Err(WorkspacesError {
                kind: WorkspacesErrorKind::NotFound,
                message: _,
            }) => false,
            Err(error) => return Err(error),
        };
        let env = env(&config).await?;
        if !exists {
            sprite.create().await?;
        }
        if let Some(setup) = &config.setup {
            let args = vec!["-c".into(), SETUP.into(), "bash".into(), setup.clone()];
            provision(&sprite, &env, "setup", args).await?;
        }
        for (url, tree) in config.worktrees(&context.id) {
            let args = vec![
                "-c".into(),
                REPO.into(),
                "bash".into(),
                url.to_owned(),
                crate::config::repository(url),
                tree,
                format!("fleet/{id}"),
            ];
            provision(&sprite, &env, &format!("repo {url}"), args).await?;
        }
        let bytes = serde_json::to_vec(&config).expect("config serializes");
        replace(&sprite, &marker, bytes, true).await
    }

    async fn release(context: WorkspaceContext) -> Result<(), WorkspacesError> {
        let (config, sprite) = parse(&context)?;
        if config.sprite.is_none() {
            return sprite.delete().await;
        }
        let args = vec![
            "-c".into(),
            RELEASE.into(),
            "bash".into(),
            crate::hex(&context.id),
        ];
        match provision(&sprite, &[], "release", args).await {
            Err(WorkspacesError {
                kind: WorkspacesErrorKind::NotFound,
                message: _,
            }) => Ok(()),
            result => result,
        }
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
        let env = env(&config).await?;
        run(&sprite, &config.directory(&context.id), &env, command).await
    }
}

fn load(context: &WorkspaceContext) -> Result<(Config, Sprite), WorkspacesError> {
    let (config, sprite) = parse(context)?;
    let provider = ProviderConfig::load()
        .map_err(|message| crate::error(WorkspacesErrorKind::Other, message))?;
    config.check(&provider)?;
    Ok((config, sprite))
}

/// Parses the workspace config without checking it against the provider config.
fn parse(context: &WorkspaceContext) -> Result<(Config, Sprite), WorkspacesError> {
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

/// Returns the configured environment with the current credentials of connections.
async fn env(config: &Config) -> Result<Vec<(String, String)>, WorkspacesError> {
    let mut credentials = BTreeMap::<&String, String>::new();
    let mut env = Vec::new();
    for (name, variable) in &config.env {
        let value = match variable {
            Variable::Value(value) => value.clone(),
            Variable::Credential(Credential { connection }) => {
                if let Some(credential) = credentials.get(connection) {
                    credential.clone()
                } else {
                    let credential = crate::authentication::credential(connection)
                        .await?
                        .ok_or_else(|| {
                            crate::error(
                                WorkspacesErrorKind::Other,
                                format!("connection {connection} is not signed in"),
                            )
                        })?;
                    credentials.insert(connection, credential.clone());
                    credential
                }
            }
        };
        env.push((name.clone(), value));
    }
    Ok(env)
}

async fn provision(
    sprite: &Sprite,
    env: &[(String, String)],
    name: &str,
    args: Vec<String>,
) -> Result<(), WorkspacesError> {
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
    let output = run(sprite, HOME, env, command).await?;
    let failure = match output.exit {
        WorkspaceExit::Code(0) => return Ok(()),
        WorkspaceExit::Code(code) => format!("exited with code {code}"),
        WorkspaceExit::Terminated => "was terminated".to_owned(),
        WorkspaceExit::TimedOut => "timed out".to_owned(),
    };
    let mut stderr = output.stderr.head;
    if output.stderr.omitted > 0 {
        stderr.extend_from_slice(" … ".as_bytes());
        stderr.extend(output.stderr.tail);
    }
    Err(crate::error(
        WorkspacesErrorKind::Other,
        format!(
            "{name} {failure}: {}",
            String::from_utf8_lossy(&stderr).trim(),
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
    env: &[(String, String)],
    mut command: WorkspaceCommand,
) -> Result<WorkspaceOutput, WorkspacesError> {
    command.env.splice(0..0, env.iter().cloned());
    let cwd = crate::config::path(directory, command.cwd.as_deref().unwrap_or(""))?;
    let timeout_ms = command.timeout_ms;
    let response = sprite
        .exec(crate::exec::body(command, &cwd)?, timeout_ms)
        .await?;
    crate::exec::parse(&response)
}
