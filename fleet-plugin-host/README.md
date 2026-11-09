# Host plugin

Provides a `default` workspace on the machine running Fleet. Commands run with
the host's permissions and environment. The working directory is not a sandbox.

Build from the repository root with the `wasm32-unknown-unknown` target and
`wasm-tools` installed:

```sh
cargo build -p fleet-plugin-host --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/fleet_plugin_host.wasm -o host.wasm
```

Copy `host.wasm` into Fleet's config directory under `plugins/`, then add:

```json
{
  "plugins": {
    "host": {
      "path": "./plugins/host.wasm",
      "capabilities": {
        "process": true,
        "filesystem": [
          {
            "host_path": "/",
            "guest_path": "/",
            "read_only": false
          },
          {
            "host_path": "/home/user/.local/share/fleet/plugins/host",
            "guest_path": "/state",
            "read_only": false
          }
        ]
      }
    }
  },
  "providers": {
    "host": {
      "plugin": "host"
    }
  }
}
```

This grants access to the host filesystem with the server's permissions.
Create the state directory before starting Fleet. Filesystem mounts must use
the same host and guest paths so file operations and commands agree.
Use forward slashes in configuration, including Windows paths such as `C:/project`.
Each provider needs its own persistent directory mounted at `/state`.
Clocks and randomness are supplied by the host. HTTP and credentials are not needed.

This registers `host:default`. The workspace config must supply `cwd` as an
absolute path covered by a workspace mount. Set it in the role's
`extensions.fleet.workspace.config` when using the basic tools plugin.
There is no provider-wide default directory. Opening a workspace binds its ID to
that directory. Reopening is safe, but changing the directory for an existing ID
is rejected. Different IDs may share a directory.

Relative file paths and command working directories start in the workspace directory.
Absolute paths and `..` are supported within the granted filesystem mounts.
WASI resolves file paths and rejects symlinks with absolute targets.
Commands run with the host process's permissions.
File writes replace the destination atomically. Commands enforce the requested
timeout and retain up to the output limit from each stream while draining the
rest. Retried commands return `UnsafeToRetry` because an earlier attempt may
have taken effect. Cancellation and timeouts terminate the direct child, not
its descendants.
