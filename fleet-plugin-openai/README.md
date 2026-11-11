# OpenAI plugin

Provides models through the OpenAI Responses API, using `fleet-models`. The plugin
supplies WASI HTTP, host credentials and blob access. It declares a `chatgpt`
connection for signing in with ChatGPT.

Build from the repository root with the `wasm32-unknown-unknown` target and
`wasm-tools` installed:

```sh
cargo build -p fleet-plugin-openai --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/fleet_plugin_openai.wasm -o openai.wasm
```

Copy `openai.wasm` into Fleet's config directory under `plugins/`, then add:

```json
{
  "plugins": {
    "openai": {
      "path": "./plugins/openai.wasm",
      "capabilities": {
        "http": ["https://api.openai.com"],
        "credentials": true,
        "blobs": true
      }
    }
  },
  "providers": {
    "openai": {
      "plugin": "openai",
      "config": {
        "base_url": "https://api.openai.com/v1",
        "provider_id": "openai",
        "models": [
          {
            "name": "gpt-6.1-sol",
            "context_window": 1050000,
            "compaction_threshold": 400000
          }
        ]
      }
    }
  }
}
```

Sign in through Fleet's `openai:chatgpt` connection. The host handles OAuth and
token renewal. Keep the provider name unchanged when replacing an existing
plugin so it retains access to its saved credentials.

The model list, context windows and compaction thresholds come from configuration.
Add models to this list as needed. Each model also accepts an `extensions` object.
Keep `provider_id` stable for compatible saved state, and use a distinct value for
endpoints or accounts whose state cannot be shared. The base URL must match an
approved HTTP origin. It receives the connection's bearer credentials.

A role selects a model and its runtime options:

```yaml
model:
  name: openai:gpt-6.1-sol
  config:
    effort: xhigh
    tools:
      - type: web_search
```

Model config supports `effort`, `speed` and provider `tools`. Missing or null
values leave the corresponding option unset. Availability depends on the selected
model and account. Ordinary Fleet tools can be used alongside provider tools.

Responses are not stored by the API. Reasoning and compaction state are retained
in Fleet's history. Compaction, continuation recovery, provider tools and image
encoding are handled by `fleet-models`. Images are read through the host's blob
capability. Filesystem and process access are not required.
