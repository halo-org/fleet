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

The workspace config may supply `setup`, a script run with `bash -lc` after the
sprite is created. Opening a workspace stores its config in
`/home/sprite/.fleet/workspace`. Reopening is safe, but changing the config for
an existing ID is rejected. Setup must be safe to rerun. It runs again if an
earlier open failed, and concurrent first opens may both run it.

Relative file paths and command working directories start in `/home/sprite`.
File writes go to a temporary sibling that is then renamed over the destination.
A failed rename may leave the temporary file behind.

Commands run through a bash wrapper that reads the command from standard input.
Output is spooled to files on the sprite, and up to the output limit from each
stream is returned when the command ends. Timeouts kill the command's process
group. Exits caused by signals are reported as codes above 128.

## Limitations

- Sprites are never deleted. They accumulate until removed in Sprites.
- Retried commands return `unsafe-to-retry` because Sprites exec is not idempotent.
- Spooled output is not capped on the sprite's disk.
- The API key is visible to the plugin. HTTP is limited to `api.sprites.dev`.
- Sprites returns 404 for both a missing file and a missing sprite, so a missing
  file costs an extra request.
- Only API keys are supported. The workspace directory is not configurable.
