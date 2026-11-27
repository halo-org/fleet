# Tasks plugin

Provides `wait` and `cancel` for an agent's tasks: its async tool calls named by a
task handle, such as `oneshot_agent_async` calls in the
[agents plugin](../fleet-plugin-agents). The plugin needs tool call access. It does
not need process, filesystem or HTTP access.

- `wait` returns once the named tasks finish, or any of them with `"until": "any"`.
  It also returns once a steering message arrives that the agent had not seen when
  it called `wait`, and the message is read after the wait. Queued messages do not
  end a wait: they are delivered once the turn ends, and the wait is part of the
  turn. Each task's result arrives as its own tool result.
- `cancel` interrupts the named tasks that are still running and returns once they
  stop. Tasks that already finished are reported as such.

Both tools are synchronous and have no timeout. Send a steering message or
interrupt the call to stop a wait early. To wait for whichever comes first, a task
or a queued message, a model can end its turn instead: both start a new turn.

Tasks are looked up among the calls the model could see when it called the tool.
Unknown names, and names used by more than one call, are returned as errors
without waiting or cancelling anything. A condition that ended a wait stays true,
so a retried wait also returns, unless the agent's role or tools change in between.

## Build

From the repository root, with `wasm32-unknown-unknown` installed:

```sh
cargo build -p fleet-plugin-tasks --release --target wasm32-unknown-unknown
```

## Configure

Copy `target/wasm32-unknown-unknown/release/fleet_plugin_tasks.wasm` into Fleet's
config directory as `plugins/tasks.wasm` and add:

```json
{
  "plugins": {
    "tasks": {
      "path": "./plugins/tasks.wasm"
    }
  },
  "providers": {
    "tasks": {
      "plugin": "tasks",
      "capabilities": {
        "tool_calls": true
      }
    }
  }
}
```

Give the calling role the tools:

```yaml
tools:
  wait:
    name: tasks:wait
  cancel:
    name: tasks:cancel
```
