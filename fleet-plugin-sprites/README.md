# Sprites plugin

Provides a `default` workspace on [Sprites](https://sprites.dev). Each workspace
ID gets its own sprite named `fleet-` followed by the ID in hex.

Build from the repository root with the `wasm32-unknown-unknown` target and
`wasm-tools` installed:

```sh
cargo build -p fleet-plugin-sprites --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/fleet_plugin_sprites.wasm -o sprites.wasm
```

Copy `sprites.wasm` into Fleet's config directory under `plugins/`, then add:

```json
{
  "plugins": {
    "sprites": {
      "path": "./plugins/sprites.wasm",
      "capabilities": {
        "http": ["https://api.sprites.dev"],
        "credentials": true
      }
    }
  },
  "providers": {
    "sprites": {
      "plugin": "sprites"
    }
  }
}
```

This registers `sprites:default` and a `sprites` connection. Sign in with a
Sprites API key before opening a workspace.

The workspace config accepts these optional fields:

- `sprite` names a sprite shared by every workspace using it. It may contain
  lowercase letters, digits and hyphens.
- `setup` is a script run with `bash -lc` when a workspace opens and it differs
  from the last script that succeeded on the sprite.
- `repo` is a git URL. Each workspace ID gets a worktree in
  `/home/sprite/worktrees/<id>` on a new branch `fleet/<id>` from the latest
  default branch, without an upstream. An existing branch of that name is
  reused. The bare clone lives in `/home/sprite/repos/` under the URL with `%`
  and `/` percent-encoded, and is fetched before each new worktree.

```json
{
  "sprite": "dev",
  "repo": "https://github.com/halo-org/fleet.git"
}
```

Opening a workspace stores its config in `/home/sprite/.fleet/workspaces/<id>`,
and reopening it with the same config does nothing more. A changed config is
provisioned again without removing anything. Setup holds a lock on the sprite
and records the script in `/home/sprite/.fleet/sprite` once it succeeds. It must
be safe to rerun, because it runs again after a failure or a change. Repository
provisioning holds another lock and resumes after a failed or cancelled open.
Git credentials are not managed. Configure them on the sprite.

Relative file paths and command working directories start in the worktree with
`repo`, and in `/home/sprite` otherwise.
File writes go to a temporary sibling that is then renamed over the destination.
A failed rename may leave the temporary file behind.

Commands run through a bash wrapper that reads the command from standard input.
Output is spooled to files on the sprite, and up to the output limit from each
stream is returned when the command ends. Timeouts kill the command's process
group. Exits caused by signals are reported as codes above 128.

## Limitations

- Sprites, worktrees and branches are never deleted. They accumulate until
  removed by hand.
- Changing `sprite` for an existing ID opens a new environment, because the
  previous sprite is not consulted.
- Changing `repo` for an existing ID keeps its worktree, which still uses the
  previous clone. Removing `setup` or `repo` does not undo them.
- A changed setup runs while other workspaces may be using the sprite. Workspaces
  sharing a sprite with different setups run them in turn as they open.
- Retried commands return `unsafe-to-retry` because Sprites exec is not idempotent.
- Spooled output is not capped on the sprite's disk.
- The API key is visible to the plugin. HTTP is limited to `api.sprites.dev`.
- Sprites returns 404 for both a missing file and a missing sprite, so a missing
  file costs an extra request.
- Only API keys are supported.
