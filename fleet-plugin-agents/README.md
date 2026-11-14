# Agents plugin

Provides `oneshot_agent` and `oneshot_agent_async`, which delegate a task to a new
agent, and a `parent` workspace that lets that agent work in its parent's
workspace. The plugin needs agent and role access, and workspace access to the
providers it forwards to. It does not need process, filesystem or HTTP access.

- `oneshot_agent` creates an agent with the given role, parameters, prompt and
  optional title, waits for its first turn to end and returns its final text
  response. The structured content holds the agent ID.
- `oneshot_agent_async` does the same as an async tool call, so the calling agent
  can continue while the new agent works.

The new agent sees only the prompt and its role, not the calling conversation.
An unknown or disallowed role returns the available roles to the model, as does
an unknown parameter. A new agent that is interrupted returns an error result
with its ID. Interrupting the calling agent interrupts the new agent.

Fleet derives the new agent from the calling agent and tool call, so retries
reuse the same agent and its turn. Children are listed under their parent rather
than in the inbox.

## Build

From the repository root, with `wasm32-unknown-unknown` and `wasm-tools` installed:

```sh
cargo build -p fleet-plugin-agents --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/fleet_plugin_agents.wasm -o agents.wasm
```

## Configure

Copy `agents.wasm` into Fleet's config directory under `plugins/`. With a `host`
workspace provider and the basic tools plugin configured, add:

```json
{
  "plugins": {
    "agents": {
      "path": "./plugins/agents.wasm",
      "capabilities": {
        "agents": true,
        "roles": true,
        "workspaces": ["host"]
      }
    },
    "tools": {
      "path": "./plugins/tools.wasm",
      "capabilities": {
        "blobs": true,
        "workspaces": ["host", "agents"]
      }
    }
  },
  "providers": {
    "agents": {
      "plugin": "agents",
      "config": { "roles": ["reviewer", "worker-*"] }
    },
    "tools": {
      "plugin": "tools"
    }
  }
}
```

Provider config and per-tool config accept `roles`, a list of role names that
new agents may use. A trailing `*` matches a prefix. Tool config overrides
provider config, and no list allows every role. The role capability also grants
role writes, which this plugin does not use.

Give the calling role the tool:

```json
{
  "tools": {
    "delegate": { "name": "agents:oneshot_agent" }
  }
}
```

## Parent workspace

When an agent is created, the calling agent's role workspace is recorded in the
new agent's `fleet.parent_workspace` extension. The environment ID is pinned to
the calling agent's environment. A calling agent that itself uses `agents:parent`
passes on the workspace it inherited. A workspace from a provider this plugin
cannot access is not inherited.

Select the parent workspace in the new agent's role:

```json
{
  "workspace": { "name": "agents:parent" }
}
```

Opens, reads, writes and commands are forwarded to the recorded workspace with
the new agent's ID. Workspace config and IDs from the role are ignored. An agent
without a recorded workspace gets an invalid configuration error.

The workspace is fixed when the agent is created. Later changes to the calling
agent's role do not move existing children.
