# Fleet configuration

Fleet reads `config.json` and `roles/*.md` from its platform config directory.
On Linux this is `$XDG_CONFIG_HOME/fleet`, or `~/.config/fleet`.
On macOS it is `~/Library/Application Support/fleet`.
On Windows it is `%APPDATA%\fleet`.

```json
{
  "listen": "127.0.0.1:1337",
  "plugins": {
    "models": {
      "path": "./plugins/models.wasm",
      "capabilities": {
        "http": true,
        "credentials": true
      }
    }
  },
  "providers": {
    "personal": {
      "plugin": "models",
      "config": {}
    },
    "work": {
      "plugin": "models",
      "config": {}
    }
  }
}
```

Plugin and provider names contain ASCII letters, digits, hyphens or underscores.
Providers reference a plugin and pass it an arbitrary JSON configuration object.
The provider name is its namespace and credential scope. A model named `example`
from the `work` provider is addressed as `work:example`.

Capabilities are approved per plugin and default to no access. HTTP enables
outgoing requests. Credentials enables access to that provider's saved credentials.
Filesystem access uses a `filesystem` array of mounts:

```json
{
  "host_path": "./files",
  "guest_path": "/files",
  "read_only": true
}
```

Local component paths and host mount paths are relative to the config directory.
The same compiled component is shared by providers using the same plugin.
Restart Fleet after changing plugin or provider configuration.

## Remote plugins

Replace a plugin's `path` with an HTTPS `url`. Run `fleet lock`
(`cargo run -p fleet -- lock` from this repository) to download remote components,
validate them, and write their SHA-256 digests to `plugins.lock`.
Running the command again updates all remote plugins.

Startup requires a matching lock entry. Components are cached by digest in the
platform cache directory and verified before compilation. Missing cached files
are downloaded from the locked URL. Local components are read directly and are
not locked.

Changing the lockfile never grants additional capabilities. A component requiring
an unapproved interface fails to load.

## Roles

`roles/assistant.md` defines a role named `assistant`:

```markdown
---
model:
  name: work:example
  config: {}
tools: {}
---
You are a helpful assistant.
```

The Markdown body is the system prompt. YAML frontmatter supports `model`,
`tools`, `parameters`, `compaction` and `extensions`. Only `model` is required.
Each model or tool selection has a `name` and an optional `config` object.
Tools are keyed by their alias.

Parameters have a `target` and optional `extensions`. Targets use `type: model`,
`type: tool` or `type: extensions`, with a `path` array of object keys. Tool
targets also specify the tool alias in `tool`. Compaction accepts `threshold`
and `instructions`.

Role names follow the same naming rules as providers. Role files are read on each
lookup, so edits apply without restarting Fleet.

`GET /providers` lists provider names and their plugin names. `GET /roles` lists
role names. `GET /connections` includes each connection's owning provider.
These endpoints do not expose plugin configuration or credentials.
