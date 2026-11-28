# Sandbox plugin

Stands in for what reaches outside Fleet in its [sandbox](../fleet/README.md#sandbox)
and in tests: a scripted model, an in-memory workspace and a sign-in connection. It
needs credentials and workers. It does not need process, filesystem or HTTP access.

## Model

`scripted` follows the latest user message. Plain text gets the reply
`You said: <text>`. A JSON object is a script:

```json
{
  "steps": [
    {
      "reasoning": "Checking the notes.",
      "tool_calls": [{ "tool": "read", "input": { "path": "notes.txt" } }]
    },
    { "text": "The notes are ready.", "delay_ms": 50 }
  ]
}
```

Each response since that message advances to the next step, so tool results lead
to the following step. After the last step the model replies `Done.`. A single
step can be sent without `steps`. Steps accept:

- `reasoning` and `text`, streamed word by word.
- `tool_calls`, each with a role's tool alias and its `input`.
- `credential`, which appends a description of the `account` connection's credential.
- `refresh`, which first rejects the current OAuth token so Fleet refreshes it, then
  describes the credential.
- `error`, with a `message` and an optional `kind` of `invalid_continuation`,
  `invalid_compaction`, `context_too_long`, `rejected`, or `other`. Scripts are stateless, so
  retries fail again until the agent is interrupted or steered with another script. A
  `rejected` error stalls the agent instead, and resuming it stalls it again.
- `delay_ms`, the pause before each streamed chunk.

An invalid script is answered with a description of the problem. Usage counts
bytes of text as tokens. Compaction replaces the context with a short summary that
keeps the script's position. The model issues no continuations and rejects them as
invalid.

## Workspace

`default` keeps files in memory, separately for each workspace ID, until Fleet
stops or the workspace is released. Relative paths start in `/workspace`. Commands do not run: they report
`Simulated: <command>` and succeed, except `exit N`, which exits with `N`. Output
beyond the command's limit keeps its head and tail.

## Connection

`account` accepts any API key. With an `oauth` base URL in the provider config,
it also offers OAuth sign-in through that provider's `authorize` and `token`
endpoints, and a `resource` method that signs in to the protected resource at
`resource` below the base URL, whose authorization server Fleet discovers. Fleet's
sandbox serves both at `/sandbox/oauth`.

## Build

From the repository root, with `wasm32-unknown-unknown` installed:

```sh
cargo build -p fleet-plugin-sandbox --release --target wasm32-unknown-unknown
```

## Configure

```json
{
  "plugins": {
    "sandbox": {
      "path": "./plugins/sandbox.wasm"
    }
  },
  "providers": {
    "sandbox": {
      "plugin": "sandbox",
      "config": { "oauth": "http://127.0.0.1:1337/sandbox/oauth" },
      "capabilities": {
        "credentials": true,
        "workers": true
      }
    }
  }
}
```

Give a role the model `sandbox:scripted` and the workspace `sandbox:default`.
