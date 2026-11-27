# Sprites plugin

Provides a `default` workspace on [Sprites](https://sprites.dev). Each workspace
ID gets its own sprite named `fleet-` followed by the ID in hex.

Build from the repository root with the `wasm32-unknown-unknown` target installed:

```sh
cargo build -p fleet-plugin-sprites --release --target wasm32-unknown-unknown
```

Copy `target/wasm32-unknown-unknown/release/fleet_plugin_sprites.wasm` into Fleet's
config directory as `plugins/sprites.wasm`, then add:

```json
{
  "plugins": {
    "sprites": {
      "path": "./plugins/sprites.wasm"
    }
  },
  "providers": {
    "sprites": {
      "plugin": "sprites",
      "config": {
        "connections": {
          "github": { "title": "GitHub" }
        }
      },
      "capabilities": {
        "http": ["https://api.sprites.dev"],
        "credentials": true
      }
    }
  }
}
```

This registers `sprites:default` and a `sprites` connection. Sign in with a
Sprites API key before opening a workspace. The optional `connections` object
declares more API key connections by name, each with a `title` and an optional
`description`. Their keys are kept with Fleet's other credentials and can be
passed to workspaces through `env`. The name `sprites` is reserved.

The workspace config accepts these optional fields:

- `sprite` names a sprite shared by every workspace using it. It may contain
  lowercase letters, digits and hyphens.
- `setup` is a script run with `bash -lc` when a workspace opens and it differs
  from the last script that succeeded on the sprite.
- `repos` is a git URL or a list of them. With one URL, each workspace ID gets
  a worktree in `/home/sprite/worktrees/<id>`. With a list, that directory
  holds a worktree of each repository, named after the last segment of its URL
  without `.git`. Names must be distinct. Each worktree is on a new branch
  `fleet/<id>` from the latest default branch, without an upstream. An existing
  branch of that name is reused. Bare clones live in `/home/sprite/repos/`
  under the URL with `%` and `/` percent-encoded, and are fetched before each
  new worktree.
- `env` sets environment variables for setup, repository provisioning and
  commands. Names must be shell variable names. Each value is a string or
  `{ "connection": "<name>" }`, which passes the current key of a connection
  declared in the provider config. Variables set by a command take precedence.

```json
{
  "sprite": "dev",
  "repos": [
    "https://github.com/halo-org/fleet.git",
    "https://github.com/halo-org/fleet-web.git"
  ]
}
```

For private GitHub repositories, put a token in the `github` connection and let
setup make git read it through the GitHub CLI:

```json
{
  "setup": "gh auth setup-git",
  "repos": "https://github.com/halo-org/fleet.git",
  "env": {
    "GH_TOKEN": { "connection": "github" },
    "GIT_AUTHOR_NAME": "Josh Cooper",
    "GIT_AUTHOR_EMAIL": "josh@cooper.dev",
    "GIT_COMMITTER_NAME": "Josh Cooper",
    "GIT_COMMITTER_EMAIL": "josh@cooper.dev"
  }
}
```

Setup runs before the repositories are cloned, so the clone can use the
credential helper it installs. `gh` and the helper read `GH_TOKEN` when they
run, so the token is not written to the sprite.

Opening a workspace stores its config in `/home/sprite/.fleet/workspaces/<id>`,
and reopening it with the same config does nothing more. The stored `env` keeps
string values as written and connection names rather than keys, so use
connections for secrets. A changed config is provisioned again without removing
anything. Setup holds a lock on the sprite and records the script in
`/home/sprite/.fleet/sprite` once it succeeds. It must be safe to rerun, because
it runs again after a failure or a change. Repository provisioning holds another
lock and resumes after a failed or cancelled open. Provisioning sets
`GIT_TERMINAL_PROMPT=0` so git fails instead of waiting for credentials.

Each command uses the `env` of the workspace config it is called with and reads
connection keys from Fleet when it runs, so a changed key or `env` applies to
later commands without reopening. A changed `env` does not run setup again
unless the script also changed, so setup should configure tools to read
variables when they run rather than copy their values. A referenced connection
missing from the provider config is an invalid configuration. A referenced
connection without a key fails commands and provisioning until a key is added.

Releasing a workspace without `sprite` deletes its sprite, including any
uncommitted or unpushed work. With `sprite`, the shared sprite and its setup
are kept. Release holds the repository lock and removes the workspace's record,
the worktrees in its directory or on its `fleet/<id>` branch, and that branch
from every bare clone. Other branches are kept. A sprite that does not exist is
not created, and a sprite Fleet has not provisioned is not changed. Releasing
again, or releasing a workspace that was never opened, succeeds. A later open
provisions the workspace again.

Relative file paths and command working directories start in
`/home/sprite/worktrees/<id>` with `repos`, and in `/home/sprite` otherwise.
File writes go to a temporary sibling that is then renamed over the destination.
A failed rename may leave the temporary file behind.

Commands run through a bash wrapper that reads the command and its environment
from standard input, so values are not part of request URLs or process arguments.
Output is spooled to files on the sprite, and the start and end of each stream
within the output limit are returned when the command ends. Timeouts kill the command's process
group. Exits caused by signals are reported as codes above 128.

## Limitations

- Only releasing removes sprites, worktrees and branches. Bare clones are kept.
  A workspace that is never released, or whose sprite or ID changed since it
  was opened, must be removed by hand.
- Release does not stop commands still running in the workspace.
- Changing `sprite` for an existing ID opens a new environment, because the
  previous sprite is not consulted.
- Changing `repos` for an existing ID keeps existing worktrees, which still use
  their previous clones. Switching between one URL and a list nests worktrees
  inconsistently. Removing `setup` or `repos` does not undo them.
- A changed setup runs while other workspaces may be using the sprite. Workspaces
  sharing a sprite with different setups run them in turn as they open.
- Retried commands return `unsafe-to-retry` because Sprites exec is not idempotent.
- Spooled output is not capped on the sprite's disk.
- Keys are visible to the plugin, and keys passed through `env` are visible to
  every command in the workspace. HTTP is limited to `api.sprites.dev`.
- Bash's read-only variables such as `UID` cannot be set.
- Sprites returns 404 for both a missing file and a missing sprite, so a missing
  file costs an extra request.
- Only API keys are supported.
