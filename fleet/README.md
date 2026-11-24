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
A path can name a component or a core module with an embedded component type, such
as a `wit-bindgen` plugin built by cargo for `wasm32-unknown-unknown`.
The same compiled component is shared by providers using the same plugin.
Restart Fleet after changing plugin or provider configuration.

`release_settled_workspaces_after_ms` releases a root agent's workspace once the
agent has been settled this many milliseconds, as described in
[Workspaces](#workspaces).

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

## Sandbox

`fleet sandbox <directory>` runs Fleet without effects outside its process, for
testing clients and changes. It reads `config.json` from the directory in the usual
format and keeps every store in memory, so each start is fresh. Plugins must be
local and may use only capabilities provided by Fleet: blobs, credentials, workers,
roles, agents, tool calls and workspace providers. HTTP, filesystem and process
access are rejected. Push notifications are unavailable.

JSON files in the directory's `roles` subdirectory are stored as roles on startup,
each named after its file. Without `FLEET_ADMIN_TOKEN`, Fleet generates an admin
token and prints it.

Sign-in reaches only the sandbox's OAuth provider, under `/sandbox/oauth` on Fleet's
own address; authorizations with other endpoints are rejected. Its `authorize`
endpoint approves every request and redirects back immediately to a callback on
127.0.0.1. Browsers reach it without the admin token. Fleet's requests to its
`token` endpoint stay in process: codes are exchanged with PKCE, and refresh tokens
rotate. Clients can pass their own redirect URI as usual, or Fleet listens for the
callback on a loopback port.

[`fleet/sandbox`](sandbox) configures the agents, roles, tools and
[sandbox](../fleet-plugin-sandbox) plugins with two roles that follow scripts
sent as messages. From the repository root:

```sh
cargo plugins
cargo run -p fleet -- sandbox fleet/sandbox
```

`cargo plugins` builds this repository's plugins into
`target/wasm32-unknown-unknown/release`.

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
Use `"roles": "read"` to allow discovery without writes.
Model calls and metadata lookups receive no role access. Fleet accepts only valid names and
roles that the API can represent as definitions. Idempotency keys are scoped to the provider
and agent. The capability grants no deletion and applies no further policy. Plugins such as
[`fleet-plugin-roles`](../fleet-plugin-roles) read their limits from provider config, which
only administrators control.

Agents look up their role on every turn. Write access lets an agent change its own model,
tools and system prompt from its next turn, and those of every agent sharing a writable role.
Give it only to agents trusted with the access of any role they can write.

## Workspaces

A role's `workspace` selects a workspace provider and its config. Tools open the
environment the first time they need it. An agent owns the environment of a
provider workspace configured without an `id`, which uses the agent's ID. An `id`
or a resolver, such as the agents plugin's `parent`, selects an environment the
agent does not own.

Releasing an environment asks its provider to discard what it holds for it, such
as a sprite. A later open provisions it again. Fleet releases only an agent's own
environment, resolved from the agent's current role. It never opens or provisions
one to release it, and does nothing for an agent that owns no environment. No
record is kept of what an agent opened. If the role or its workspace config was
edited or deleted since, the release may do nothing, and the old environment must
be removed by hand.

Fleet releases a subagent's workspace when its scope closes, whether its call
finished or was interrupted. A root agent's workspace is released once it has been
settled for `release_settled_workspaces_after_ms`:

```json
{
  "release_settled_workspaces_after_ms": 604800000
}
```

The time counts from when the agent settled, so a changed setting also applies to
agents that are already settled. Without the setting, root agents' workspaces are
never released. Restoring an agent to pending, snoozing it or sending it a message
cancels its pending release, and settling it again starts a new one. Settling and
unsettling only change `fleet.db` and never call a workspace provider. The agent
stays settled after the release, and its next tool call opens the workspace again.

A release starts only while its agent has no turn, input or tool call in progress
and no tool call is using the environment, including calls of agents sharing it.
Tool calls wait to use the environment until a release that has started ends.
Agents sharing an environment through an `id` lose its contents when its owner's
workspace is released. Pending releases survive restarts, and deadlines that
passed while Fleet was stopped are acted on when it starts. They do not keep Fleet
from shutting down when idle, but shutdown waits for releases already in progress.
Failures are retried with backoff, separately for each agent. A release whose role
or workspace config is rejected as invalid, for example because the role was
deleted, is logged and dropped.

Workspace providers implement `release`. Rebuild provider components against the
updated WIT files before using this version.

## Subagents

Approve `"agents": true` in a plugin's capabilities and import
`fleet:providers/agents@0.0.0`. Tool calls can create agents, queue messages, read
permitted history and wait for the first turn end after a message is consumed.
Metadata access covers relatives; history access covers the caller and its descendants.
Workspace resolvers receive metadata access only. See [the agents plugin](../fleet-plugin-agents)
for the simple delegation tools and parent workspace configuration.

An agent runs only within the call that created it, including queued input and
background work. Durable resolution or interruption closes that scope recursively;
a response or parent turn end does not. Closing the scope releases the agent's
own workspace. Cleanup survives restarts. Conversations
remain readable afterwards. There is no explicit close operation. Managed children cannot receive unscoped
HTTP messages, including messages attached to interrupts.

Creation records `fleet.parent`, exposed as `parent` by the API. Clients cannot
set or change it.
Operation keys are scoped to the calling agent and call, not provider names.
Images sent to a child must be readable by its caller or an agent created by the same call.

Children do not appear in the root inbox or notifications. `GET /agents?parent=1`
scans agent `1`'s children; collection updates include them. Subagent execution and
workspace resolution live above core, using kit contracts, the store's scope
reconciler and `fleet-workspaces`. Plugins select workspace sources; the host
loads current role instances and enforces its approved provider list.

## Tool calls

Approve `"tool_calls": true` in a plugin's capabilities and import
`fleet:providers/tool-calls@0.0.0`. A tool call can read the state of its agent's other
unresolved tool calls, wait for that state to change and interrupt them. The driving call is
excluded. Access closes once the driving call is absent, resolved or being interrupted.

A tool declares the input property that names its calls with the `fleet.task_handle`
extension, for example `{"fleet.task_handle": "task_handle"}`. A call whose input holds a
string in that property has that handle. Fleet resolves handles when the state is read, using
the agent's current role and the declarations of its tools. Handles only need to be unique
within the driving call's context: the pending calls of the latest compaction before the
driving call's response, and the calls of the responses since, up to and including that
response. A call never sees later calls that reuse its handle, so a tool can check that its
own handle is unique when it runs. Calls in the same response see each other, and calls made
after a duplicate see the handle as duplicate.

The state reports each requested handle as unknown, pending, completed or duplicate. Fleet
does not reject duplicates. A tool that declares a handle can read its own handle and return
an error, and a tool that waits on or cancels handles reports duplicate and unknown handles in
its own words. Handles are recomputed from stored history after a restart. Changing a role or
updating a plugin can change how handles resolve, like any other change to a running agent's
configuration, and does not wake waiting calls.

The state also counts the messages the agent had not seen when the driving call was made,
including those sent since. While access is open, the counts never decrease, even as messages
are consumed, and completed handles stay completed unless the configuration changes. Waits on
these conditions do not depend on timing. Waiting for input suits synchronous waits: a
steering message is consumed once the wait returns, while queued messages still wait for the
turn to end. For calls made before this version of Fleet, messages consumed before upgrading
are not counted.

The [tasks plugin](../fleet-plugin-tasks) provides `wait` and `cancel` tools on this
interface, and `oneshot_agent_async` in the [agents plugin](../fleet-plugin-agents) names its
calls with a `task` handle.

## Stalled agents

Fleet retries failed work with backoff and logs each failure, since time often fixes it. Some
errors need a change first, such as a role naming a missing model or tool, agent parameters
the role cannot apply, or a provider that needs the user to sign in. These stall the agent
instead. Fleet logs the error once, records why the agent is stalled, and stops working on it.
Stalling adds no event and does not end the turn, so an agent waiting on a stalled subagent
keeps waiting. Stalling does not change the inbox or itself send a notification. Stalled agents
stay stalled when Fleet restarts.

Sending a message to an agent, interrupting it or its tool calls, or resuming it clears its
stall. Fleet then works on the agent again, and it stalls again if that work hits the same
error. Changing roles, plugins, credentials, or configuration does not clear stalls. A subagent
whose driving call ends has its stall cleared as its scope closes.

Agent activity and summaries report `stalled`, the reason or `null`, and
`stalled_descendants`, how many agents created by the agent's tool calls, and theirs, are
stalled. Both update through the agent and collection subscriptions. A reason has a `kind`
and the names involved, never a message:

| `kind` | Fields |
| --- | --- |
| `role_not_found` | `role` |
| `model_not_found` | `role`, `model` |
| `tool_not_found` | `role`, `alias`, `tool` |
| `invalid_model_parameters` | `role` |
| `invalid_tool_parameters` | `role` |
| `invalid_extension_parameters` | `role` |
| `model_error` | `model` |
| `tool_error` | `tool` |
| `unknown_tool_error` | `alias` |

For example, `GET /agents/1/activity` returns

```json
{
  "turn": "active",
  "interrupt_pending": false,
  "interrupt_includes_async_tool_calls": false,
  "stalled": {
    "kind": "tool_not_found",
    "role": "researcher",
    "alias": "search",
    "tool": "web:search"
  },
  "stalled_descendants": 0
}
```

`POST /agents/1/resumes` with JSON `{}` and an `Idempotency-Key` resumes agent `1` and every
stalled agent below it, returning 202. Fleet retries their outstanding work. Agents that
aren't stalled are not woken, though their `stalled_descendants` counts update.

Model and tool providers report these errors with the `rejected` error kind. The OpenAI
plugin does so while no one has signed in with ChatGPT. Rebuild provider components against
the updated WIT files before using this version.

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
Tools get access to the executing agent's attachments and can create new blobs. They can also
read blobs held by agents their call created, so delegation tools can return a child's images.
Images enter a conversation only as content, and content may carry an image only if its author
can read it. A model plugin must not assume any particular attachment tool exists. No filesystem
paths or tools are injected.

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
