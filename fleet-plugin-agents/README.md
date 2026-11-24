# Agents plugin

`oneshot_agent` and `oneshot_agent_async` run a task in a new child agent and
return its response. `parent` resolves a workspace from the parent
agent's current role. The plugin needs agent, blob, tool call and read-only role
access; it does not need workspace I/O, process, filesystem or HTTP access.

## Calls and responses

Supply `role`, `prompt` and optional `parameters`, `images` and `title`. `images`
lists up to 20 image references the caller can read, attached after the prompt.
The result is the agent's response text. Any decimal number in that text that names
an image the call can read is returned after it as an image, up to 20 images and
40 MiB. The agent finishes when the call ends, and only that call can message it.

Both tools wait for the first turn end **after the queued message is consumed**.
An earlier unsolicited response does not satisfy the wait. An interruption is
returned as an error result, not skipped in favor of a later response. A final
turn without text is reported explicitly. Async tools do not delay turn completion.
`oneshot_agent_async` is an async tool, so its caller can continue while it waits.
Neither tool steers the child.

`oneshot_agent_async` also requires `task`, a name that tools such as the
[tasks plugin](../fleet-plugin-tasks) use to wait for or cancel the call. It is
declared as the call's task handle. A call whose name is already used by an
earlier call the model can see, or by another call in the same response, returns
an error without starting an agent. Calls made before `task` existed still run
without a name. The plugin imports tool call access even when only
`oneshot_agent` is used, so approve `tool_calls` for it.

The driving tool call owns execution until its result is durably committed or
it is interrupted. Ending a parent turn, retrying a call or dropping an attempt
is not scope closure. When the scope closes, Fleet withdraws unconsumed input
and interrupts remaining work, including nested agent calls. The conversation
remains readable. Scope state and operation receipts survive restarts. Children
do not appear in the root inbox or notifications, but remain available in
collection updates and `?parent=` scans.

The host capability also supports keyed creation and queued sends within a
single driving call. An orchestration plugin can send several messages and keep
background work alive between responses without an explicit close operation.
This plugin provides the simple one-message tools, not a code-mode interpreter.
A standalone oneshot call stops its child's background work when it returns.

Cleanup retries configuration and provider errors rather than treating them as
terminal. Workspace command cancellation still follows the workspace provider's
contract: it is not a guarantee that an external process has stopped.

## Build

From the repository root, with `wasm32-unknown-unknown` installed:

```sh
cargo build -p fleet-plugin-agents --release --target wasm32-unknown-unknown
```

## Configure

With a `host` workspace provider configured:

```json
{
  "plugins": {
    "agents": {
      "path": "./plugins/agents.wasm",
      "capabilities": {
        "agents": true,
        "blobs": true,
        "roles": "read",
        "tool_calls": true
      }
    },
    "tools": {
      "path": "./plugins/tools.wasm",
      "capabilities": {
        "blobs": true,
        "workspaces": ["host", "helpers"]
      }
    }
  },
  "providers": {
    "helpers": {
      "plugin": "agents",
      "config": { "roles": ["reviewer", "worker-*"] }
    },
    "tools": { "plugin": "tools" }
  }
}
```

Provider and tool config accept `roles`, a list of allowed role names. A trailing
`*` matches a prefix. Tool config overrides provider config; no list allows every
role. Unknown roles or parameters return the available choices to the model.
Agent metadata reads are restricted to relatives; conversation reads are
restricted to the caller and its descendants. An agent ID alone grants no access.

Give the calling role a tool:

```yaml
tools:
  delegate:
    name: helpers:oneshot_agent
```

## Parent workspace

A child role can inherit without a parameter or creation-time snapshot:

```yaml
workspace:
  name: helpers:parent
  config:
    directory: src
```

`directory` is optional. It changes relative paths and the default command
working directory, not the underlying environment or its config. Absolute paths
remain environment-absolute; `..` is allowed. A view is not a sandbox.

Each open loads the current role and parameters at every referenced agent.
Several levels compose: a parent's `packages` view and a child's `src` view
become `packages/src`. A tool retains its resolved binding for the operation, so
an edit's read and write use the same workspace. The next open sees role changes.
Missing roles, parent links and workspaces, cycles and inaccessible references
produce errors rather than silently falling back to a different environment.
A child using `parent` owns no environment, so releasing its workspace when its
scope closes leaves the parent's environment in place.

The resolver returns a typed agent reference, not a provider name convention.
`helpers` is an arbitrary registration name. Other resolver plugins can select a
root or ancestor, or return an explicit concrete environment and view. The host
retains the original acting agent and dispatches only through the tool plugin's
approved workspace providers.
