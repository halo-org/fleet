# Roles plugin

Provides `list_roles`, `read_role`, `write_role` and `edit_role` through Fleet's
roles interface. The plugin needs role access. It does not need process, filesystem
or HTTP access.

- `list_roles` returns the name and version of each stored role.
- `read_role` returns a role and its version.
- `write_role` creates a role when no `version` is given and replaces it when
  the version matches. A changed or existing role is reported to the model.
- `edit_role` changes part of an existing role, in the shape `read_role` returns.
  It first deletes the fields at the JSON Pointers in `remove`. It then merges
  `merge` into the role: objects merge recursively, and other values, including
  arrays and `null`, replace the current value. Last, it applies the exact
  replacements in `system_prompt_edits`, where each `old` text must match exactly
  once. A given `version` must match the current role. Roles cannot be renamed.

Writes and edits return the new version unless the role changed again
afterwards. Versions are decimal strings. Writes use the tool call ID as their
idempotency key, so retries do not repeat a completed write. A concurrent change
is reported to the model, which should read the role again. Roles are never
deleted.

Roles use Fleet's stored shape:

```json
{
  "name": "reviewer",
  "model": {
    "name": "work:example",
    "config": { "effort": "high" }
  },
  "tools": {
    "bash": { "name": "tools:bash" }
  },
  "system_prompt": "Review code changes.",
  "parameters": {
    "effort": {
      "target": { "type": "model", "path": ["effort"] },
      "extensions": { "fleet.title": "Reasoning effort" }
    }
  },
  "compaction": { "threshold": 100000 },
  "extensions": { "fleet.title": "Reviewer" }
}
```

Parameter targets have the type `model`, `tool` or `extensions`. Tool targets
also name the tool alias in `tool`. Fleet accepts the role extensions
`fleet.title`, `fleet.description`, `fleet.hidden` and `fleet.workspace`, and the
parameter extensions `fleet.title`, `fleet.description` and `fleet.schema`.
Extension parameter targets must start with `fleet.workspace`.

Roles saved through the HTTP API or UI omit defaults such as `"fleet.hidden": false`
and an empty `compaction`. They read back without them.

## Build

From the repository root, with `wasm32-unknown-unknown` and `wasm-tools` installed:

```sh
cargo build -p fleet-plugin-roles --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/fleet_plugin_roles.wasm -o roles.wasm
```

## Configure

Copy `roles.wasm` into Fleet's config directory under `plugins/` and add:

```json
{
  "plugins": {
    "roles": {
      "path": "./plugins/roles.wasm",
      "capabilities": {
        "roles": true
      }
    }
  },
  "providers": {
    "roles": {
      "plugin": "roles",
      "config": {
        "write": ["drafts-*"]
      }
    }
  }
}
```

`write` lists the roles the provider may write, by exact name or by a prefix
ending in `*`. Without `write`, every role is writable. An empty list makes the
provider read-only. Configure several providers of the plugin for different
limits, and give each role the tools it needs, such as `roles:read_role` or
`roles-admin:write_role`. Limits come only from provider config, so an agent
cannot widen them by editing a role. Future approval settings belong there too.

Agents look up their role on every turn. An agent that can write its own role
can change its own model, tools and prompt from its next turn. Writable roles
can include any configured tool, so give write access only to agents trusted
with every tool. If agents can also create agents with writable roles, write
access amounts to unrestricted tool access.
