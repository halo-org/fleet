# Basic tools plugin

Provides `bash`, `read`, `write` and `edit` through Fleet's workspace interface.
The plugin does not need process, filesystem or HTTP access.

- `bash` runs a command in a new Bash process and returns bounded stdout and stderr.
- `read` reads a UTF-8 file within the configured size limit.
- `write` creates or replaces a UTF-8 file, creating parent directories as needed.
- `edit` applies sequential exact replacements. Each match must be unique, including
  overlapping matches. All edits are prepared before writing. The write checks the
  original contents to detect intervening changes.

Paths are relative to the workspace. Exit codes, timeouts and file errors are
returned to the model as tool output. Failed workspace setup remains retryable.
Retried Bash calls pass the retry flag to the workspace. File mutations refuse
retries because the original write may have succeeded.

## Build

From the repository root, with `wasm32-unknown-unknown` and `wasm-tools` installed:

```sh
cargo build -p fleet-plugin-tools --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/fleet_plugin_tools.wasm -o tools.wasm
```

## Configure

Copy `tools.wasm` into Fleet's config directory under `plugins/`. With a `host`
workspace provider already configured, add:

```json
{
  "plugins": {
    "tools": {
      "path": "./plugins/tools.wasm",
      "capabilities": {
        "workspaces": ["host"]
      }
    }
  },
  "providers": {
    "tools": {
      "plugin": "tools"
    }
  }
}
```

The capability lists the provider namespaces this plugin may access. Fleet loads
those providers first and rejects missing or cyclic dependencies. These can be
any workspace providers, including remote ones.

Select the workspace in the role:

```yaml
tools:
  bash:
    name: tools:bash
  read:
    name: tools:read
  write:
    name: tools:write
  edit:
    name: tools:edit
extensions:
  fleet:
    workspace:
      name: host:default
      config:
        cwd: /home/user/project
```

The agent ID identifies its workspace environment by default. An optional `id`
of 32 hexadecimal digits selects an existing environment, allowing agents to
share it. Reuse both its name and config. Role parameters can target fields under
`extensions.fleet.workspace.config`.

Workspace selection and config come only from the role's
`extensions.fleet.workspace`. Provider config and per-tool config accept `shell`, `timeout_ms`,
`output_limit` and `read_limit`. Non-null tool settings override provider settings.
An absent or null role workspace leaves the tools without a workspace.

Defaults are `bash`, 120000 milliseconds, 65536 bytes per output stream and a
65536-byte read limit. Bash input may select a shorter timeout and a relative
working directory. `read_limit` also limits the file size for editing.
