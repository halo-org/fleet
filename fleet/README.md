# Fleet configuration

Fleet reads `config.json` from its platform config directory.
On Linux this is `$XDG_CONFIG_HOME/fleet`, or `~/.config/fleet`.
On macOS it is `~/Library/Application Support/fleet`.
On Windows it is `%APPDATA%\fleet`.

Set `FLEET_DIR` to an absolute path to keep everything in one place instead. Fleet then uses its
`config`, `data` and `cache` subdirectories, so `config.json` is at `$FLEET_DIR/config/config.json`.

`FLEET_ADMIN_TOKEN` is required to start the server. It must be at least 32 characters from
`A-Za-z0-9-._~+/=`. Clients send it as a bearer token or sign in through `POST /session`.

```json
{
  "listen": "127.0.0.1:1337",
  "plugins": {
    "models": {
      "path": "./plugins/models.wasm",
      "capabilities": {
        "http": ["https://api.example.com"],
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

Capabilities are approved per plugin and default to no access. HTTP lists the
origins that outgoing requests may use, each a scheme, host and optional port.
Requests to other origins are denied. Credentials enables access to that
provider's saved credentials.
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

Approve `"workers": true` in a plugin's capabilities to let it retain private WASM
workers between calls. The component must import `fleet:providers/workers@0.0.0`
and export `fleet:providers/worker@0.0.0` alongside its normal provider interfaces.
Workers belong to a configured provider, not to the compiled plugin or another
namespace. Other plugins continue to use its normal interfaces.

Workers receive approved static capabilities, but no blob access or ability to
start nested workers. They have no automatic idle timeout; plugins may retire
idle workers themselves. Fleet stops worker admission during shutdown and allows
five seconds for cleanup before dropping their instances. Worker state does not
survive a restart. See `fleet-wasm/wit/workers.wit` for the connection contract
and resource limits. Ordinary calls run without fuel instrumentation; worker
execution uses a separately compiled variant that yields for bounded shutdown.

Plugins with process access can use `process.spawn-group` for SDKs or helpers
that launch descendants. On Linux and macOS, resource drop and forced worker
shutdown terminate processes remaining in that group. Waiting for the leader
also terminates remaining group members before returning. Other platforms return
an error rather than falling back to child-only cleanup. Descendants must not
daemonize or leave the group. Existing `process.spawn` remains child-only.

## Remote plugins

Replace a plugin's `path` with an HTTP or HTTPS `url`, or an OCI reference such as
`oci://ghcr.io/owner/name:tag` for a component published in the
[Wasm OCI artifact](https://tag-runtime.cncf.io/wgs/wasm/deliverables/wasm-oci-artifact/)
format, for example with `wkg oci push`. Registries must allow anonymous pulls.
Run `fleet lock`
(`cargo run -p fleet -- lock` from this repository) to download remote components,
validate them, and write their SHA-256 digests to `plugins.lock`.
Running the command again updates all remote plugins. Locking trusts the
downloaded component, so use HTTPS unless the network is trusted.

Startup requires a matching lock entry. Components are cached by digest in the
platform cache directory and verified before compilation. Missing cached files
are downloaded from the locked URL. Local components are read directly and are
not locked.

Changing the lockfile never grants additional capabilities. A component requiring
an unapproved interface fails to load.

The plugins in this repository are published for each commit on `main`, for example
`oci://ghcr.io/halo-org/fleet-plugin-tools:<commit>` with the full commit hash.

## Roles

Roles are stored in `fleet.db` and edited through the API. A role definition is
a JSON object:

```json
{
  "model": {
    "name": "work:example",
    "config": {}
  },
  "tools": {},
  "system_prompt": "You are a helpful assistant."
}
```

Definitions support `title`, `description`, `hidden`, `model`, `tools`,
`workspace`, `parameters`, `compaction` and `system_prompt`. Only `model` is
required. Each model or tool selection has a `name` and an optional `config`
object. Tools are keyed by their alias. `workspace` selects a workspace for the
tools, as described by the tools plugin.

OpenAI web search is enabled through the model's configuration:

```json
{
  "model": {
    "name": "openai:gpt-6-astra",
    "config": {
      "tools": [{ "type": "web_search" }]
    }
  }
}
```

These tools run within the model provider and can be used alongside the role's
ordinary tools. Each definition accepts OpenAI's web search options, such as
`search_context_size` and `filters`. Missing, null or empty `tools` disables
provider tools. Role parameters can target `["tools"]` in the model configuration.
Provider tool calls are retained for replay with the same provider and model.
They are omitted when either changes.

Parameters have a `target` and optional `title`, `description` and `schema`.
Targets use `"type": "model"`, `"type": "tool"` or `"type": "workspace"`, with a
`path` array of object keys. Tool targets also specify the tool alias in `tool`.
Workspace paths start at the role's `workspace`, for example `["config", "cwd"]`.
Compaction accepts `threshold` and `instructions`.

Role names follow the same naming rules as providers. Agents look up their role
on each turn, so edits apply to running agents without restarting Fleet.

`GET /providers` lists provider names and their plugin names. `GET /roles` lists
roles with their display metadata and parameters. `GET /roles/{name}` returns a
role's full definition with its `name` and `version`. `GET /connections` includes
each connection's owning provider. These endpoints do not expose plugin
configuration or credentials.

`PUT /roles/{name}` stores a definition, without `name` or `version`, and requires
an `Idempotency-Key`. Send `If-None-Match: *` to create a role or `If-Match` with
the quoted version to replace one. `DELETE /roles/{name}` also requires an
`Idempotency-Key` and `If-Match`. A changed or missing role returns
`precondition_failed`.
Versions are unique within the database, so a recreated role does not match an
earlier version. To import a role, send its definition:

```sh
curl -X PUT http://127.0.0.1:1337/roles/assistant \
  -H "Authorization: Bearer $FLEET_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: import-assistant" \
  -H "If-None-Match: *" \
  --data @assistant.json
```

Role definitions, including model and tool config and system prompts, are visible
to API clients. Keep credentials in connections, not in roles.

### Role selection

Roles and parameters have optional titles and descriptions. Hidden roles remain
usable by name but do not appear in the role picker. Parameters may describe
their values with a JSON Schema:

```json
{
  "title": "Reviewer",
  "description": "Review code changes.",
  "hidden": false,
  "model": {
    "name": "work:example",
    "config": { "effort": "high" }
  },
  "parameters": {
    "effort": {
      "title": "Reasoning effort",
      "schema": {
        "type": "string",
        "oneOf": [
          { "const": "low", "title": "Low" },
          { "const": "high", "title": "High" }
        ]
      },
      "target": { "type": "model", "path": ["effort"] }
    }
  }
}
```

The API exposes `title`, `description`, `hidden` and `parameters` as ordinary
fields. Each parameter exposes its title, description, schema and configured
default, when present. Model and tool config, workspaces, parameter targets and
system prompts are left to the role's definition. Parameters marked `writeOnly`
do not expose their defaults in the list.

The UI uses choices for `enum` or `oneOf` with `const`, toggles for booleans,
numeric fields for numbers, and checkboxes for unique arrays of choices. String
formats `date`, `time`, `email` and `uri` use native controls. A string with a
`text/*` content media type uses a multiline field. Other shapes use JSON input.
Schema titles and descriptions also work as field labels and help text.

Untouched parameters are omitted from requests, preserving the role's config.
Reset removes an override. A supplied `null` is an explicit value, and a schema's
`default` annotation does not change the role's config. Schemas guide the form.
They do not add validation to core.

### Plugin access

Approve `"roles": true` in a plugin's `capabilities` and have its component import
`fleet:providers/roles@0.0.0`. Tool calls can then list, read and write every stored role.
Model calls and metadata lookups receive no role access. Fleet accepts only valid names and
roles that the API can represent as definitions. Idempotency keys are scoped to the provider
and agent. The capability grants no deletion and applies no further policy. Plugins such as
[`fleet-plugin-roles`](../fleet-plugin-roles) read their limits from provider config, which
only administrators control.

Agents look up their role on every turn. Write access lets an agent change its own model,
tools and system prompt from its next turn, and those of every agent sharing a writable role.
Give it only to agents trusted with the access of any role they can write.

## Subagents

Approve `"agents": true` in a plugin's `capabilities` and have its component import
`fleet:providers/agents@0.0.0`. Every call can read any agent and its events and wait for its
turns. Reads are not restricted to related agents. Tool calls can also create agents, such as
those made by [`fleet-plugin-agents`](../fleet-plugin-agents).

An agent created by a tool call records the calling agent and call in its `fleet.parent`
extension. The API returns it as `parent`, with the agent as a decimal string, and does not let
clients set or change it. Creation derives the agent and message IDs from the provider, agent
and call, so retries return the same agent. Images in the initial message must already be
attached to the calling agent.

Children do not appear in the inbox and do not send notifications. `GET /agents?parent=1`
scans the children of agent `1`, and the collection update stream includes them. Interrupting
an agent also interrupts busy children created by its pending calls, recursively. A child
created after its call was interrupted or resolved is interrupted at once.

## Images and blobs

Messages and tool results can contain `{"type":"image","blob_id":"123"}` blocks alongside
text. Blob IDs, like agent IDs, are decimal strings. Image bytes are immutable and stored in
separate raw tables in `fleet.db`; events contain references, not base64 payloads. The database
uses format 5. Older Fleet binaries cannot open it.

Image uploads accept PNG, JPEG, GIF, and WebP, up to 10 MiB per image. The server validates the
actual format and decodes with resource limits (8,192 pixels per edge and 32 million pixels).
Messages allow up to 20 images and 40 MiB of image data. These are resource limits, not a promise
that every model can consume that much image context. Images are not resized or recompressed.

### Resumable HTTP uploads

Uploads use the same trusted API boundary as other Fleet operations. There are no signed URLs
or new user-authentication mechanisms. Keep the API on loopback or behind an authenticated
proxy; do not expose it directly to untrusted clients. The web proxy also rejects cross-origin
requests.

1. `POST /uploads` with JSON `{"id":"123","media_type":"image/png","size":12345}`.
   Allocate a random 128-bit ID and reuse it when retrying. Matching creation retries succeed.
2. `HEAD /uploads/123` returns `Upload-Offset` and `Upload-Length`.
3. `PATCH /uploads/123` with `Content-Type: application/offset+octet-stream`, an `Upload-Offset`
   header, and at most 1 MiB of raw bytes. The offset advances atomically. A mismatched offset
   returns 409 without modifying the upload. After an uncertain response, HEAD before resending.
4. `POST /uploads/123/complete` with JSON `{}` validates and publishes the image, returning
   `{"blob_id":"123"}`. Completion is idempotent. Until this succeeds, the upload is not readable
   as a blob and cannot be attached to a message.
5. `GET /blobs/123` retrieves the original bytes; HEAD returns their media type and length.

`DELETE /uploads/123` discards staging data or a completion receipt, never a published blob.
Sessions survive server restarts and expire after 24 hours. Expired sessions are reclaimed when
new uploads are created. The store permits 64 unfinished sessions and reserves at most 256 MiB
for their declared lengths. Published blobs are retained; automatic blob collection is not yet
implemented. This is a Fleet upload protocol, not a tus-compatible endpoint.

### Plugin access

Approve `"blobs": true` in a plugin's `capabilities` and have its component import
`fleet:providers/blobs@0.0.0`. The host supplies metadata, bounded reads, and immutable writes.
Model generation and compaction get read-only access to the configured blob provider, including
older blobs that are no longer in the current input. Compaction does not revoke access.
Tools get access to the executing agent's attachments and can create new blobs. A model plugin
must not assume any particular attachment tool exists. No filesystem paths or tools are injected.

Native model providers use `OpenAiModels::with_blobs(...)`. WASM model plugins can implement the
`fleet_kit::Blobs` interface using their generated blob-import bindings. Blob failures use the
existing model error and retry handling. Images also carry a model-visible reference so
independently configured tools can accept the same identity.

The OpenAI adapter replaces retained image data URLs with blob references before storing
compaction. Those references remain inside its provider-specific state.

Rebuild existing provider components against the updated WIT files before using this version.

## Notifications

Fleet sends a Web Push notification to every registered device when an agent completes a turn
with no queued messages. Interrupted turns, snooze expiry and restoring an agent do not notify.
The payload is a [Declarative Web Push](https://www.w3.org/TR/push-api/#declarative-push-message)
message such as

```json
{
  "web_push": 8030,
  "notification": {
    "title": "Fix the build",
    "body": "The build passes again.",
    "navigate": "https://fleet.example/#agent/123",
    "tag": "agent-123"
  }
}
```

The title is the agent's title, or its role and the end of its ID. The body is the final reply as
plain text with whitespace collapsed, or "Finished" when the reply has no text. Markdown is not
removed. `navigate` opens the agent at `/#agent/{id}` on the origin that registered the device. It
is relative when that origin is not HTTPS. Browsers with declarative push show the notification
themselves. Others pass it to the service worker's `push` event. Messages are padded to 2048 bytes
so their size does not reveal the reply length, and long replies are shortened to fit.

1. `GET /notifications` returns `{"web_push":{"public_key":"..."}}`, the base64url VAPID public
   key to pass as `applicationServerKey` when subscribing.
2. `PUT /notifications/devices/123` with JSON
   `{"transport":"web_push","label":"Chrome on Android","endpoint":"https://...","keys":{"p256dh":"...","auth":"..."}}`
   registers or replaces a device and returns 204. Allocate a random 128-bit ID per browser and
   reuse it. Endpoints must use HTTPS and cannot be localhost or a non-public IP address.
3. `DELETE /notifications/devices/123` removes a device and returns 204, even if it is unknown.

Notifications are stored in `fleet.db`, one per agent, and delivered at least once. Before
sending, Fleet checks that the agent has no newer events and nothing queued. Pushes use a 12 hour
TTL and a per-agent `Topic`, so a newer notification replaces an undelivered one. Temporary push
service failures are retried three times, then dropped. Devices whose subscriptions have expired
are removed.

The VAPID key pair is generated on first start and kept in a credential store in the
`notifications` directory beside `fleet.db`. Deleting it invalidates every subscription. Clients
resubscribe when they see a new key.
