# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

MCP tools default to serialized calls. To mark every tool exposed by one server
as eligible for parallel tool calls, set `supports_parallel_tool_calls` on that
server:

```toml
[mcp_servers.docs]
command = "docs-server"
supports_parallel_tool_calls = true
```

Only enable parallel calls for MCP servers whose tools are safe to run at the
same time. If tools read and write shared state, files, databases, or external
resources, review those read/write race conditions before enabling this setting.

## MCP tool approvals

Codex stores approval defaults and per-tool overrides for custom MCP servers
under `mcp_servers` in `~/.codex/config.toml`. Set
`default_tools_approval_mode` on the server to apply a default to every tool,
and use per-tool `approval_mode` entries for exceptions:

```toml
[mcp_servers.docs]
command = "docs-server"
default_tools_approval_mode = "approve"

[mcp_servers.docs.tools.search]
approval_mode = "prompt"
```

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

Codex stores "never show again" choices for tool suggestions in `config.toml`:

```toml
[tool_suggest]
disabled_tools = [
  { type = "plugin", id = "slack@openai-curated" },
  { type = "connector", id = "connector_google_calendar" },
]
```

## Notify

`notify` is deprecated and will be removed in a future release. Existing configurations still work for compatibility, but new automation should use lifecycle hooks instead.

Codex can run a legacy notification command when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## Subagent Auth Home

Spawned agents normally use the same authentication manager as the session that
created them. This is usually what you want, but it can be limiting when you use
separate Codex homes for different authentication modes.

For example, you may start the TUI with `CODEX_HOME=~/.codex-api` so the
orchestrator can use API authentication and a model available through that API
account, while you still want spawned agents to use ChatGPT subscription
authentication stored in `~/.codex`. Configure that from the parent session's
config file:

```toml
[agents]
auth_codex_home = "~/.codex"
```

This setting is auth-only. It tells Codex where to load credentials for spawned
agents' model requests. Spawned agents still use the parent session's configured
Codex home for thread state, logs, skills, plugins, rollouts, and other runtime
files.

You can combine this with role config files to run an API-authenticated
orchestrator on a model such as `gpt-5.5-pro`, while forcing spawned agents to
use a ChatGPT-subscription model such as `gpt-5.5` with `xhigh` reasoning. Put
the auth and role declarations in the parent session's config, for example
`~/.codex-api/config.toml`:

```toml
[agents]
auth_codex_home = "~/.codex"

[agents.default]
description = "Default subagent using ChatGPT auth with GPT-5.5 xhigh."
config_file = "./agents/gpt-5.5-xhigh.toml"

[agents.explorer]
description = "Explorer subagent using ChatGPT auth with GPT-5.5 xhigh."
config_file = "./agents/gpt-5.5-xhigh.toml"

[agents.worker]
description = "Worker subagent using ChatGPT auth with GPT-5.5 xhigh."
config_file = "./agents/gpt-5.5-xhigh.toml"
```

Then create `~/.codex-api/agents/gpt-5.5-xhigh.toml`:

```toml
model = "gpt-5.5"
model_reasoning_effort = "xhigh"
```

Launch the orchestrator with the API-backed Codex home and the API-only model:

```bash
CODEX_HOME=~/.codex-api codex -m gpt-5.5-pro
```

Spawned agents that use the configured roles above will load credentials from
`~/.codex` but keep runtime state under `~/.codex-api`. Full-history forks still
inherit the parent agent type, model, and reasoning effort by design; use normal
spawns when the child must switch from `gpt-5.5-pro` to `gpt-5.5`.

Role-specific settings override the global subagent auth home:

```toml
[agents.researcher]
description = "Research-focused role."
auth_codex_home = "~/.codex-research"
```

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key) or the
`CODEX_SQLITE_HOME` environment variable. When unset, WorkspaceWrite sandbox
sessions default to a temp directory; other modes default to `CODEX_HOME`.

## Custom CA Certificates

Codex can trust a custom root CA bundle for outbound HTTPS and secure websocket
connections when enterprise proxies or gateways intercept TLS. This applies to
login flows and to Codex's other external connections, including Codex
components that build reqwest clients or secure websocket clients through the
shared `codex-client` CA-loading path and remote MCP connections that use it.

Set `CODEX_CA_CERTIFICATE` to the path of a PEM file containing one or more
certificate blocks to use a Codex-specific CA bundle. If
`CODEX_CA_CERTIFICATE` is unset, Codex falls back to `SSL_CERT_FILE`. If
neither variable is set, Codex uses the system root certificates.

`CODEX_CA_CERTIFICATE` takes precedence over `SSL_CERT_FILE`. Empty values are
treated as unset.

The PEM file may contain multiple certificates. Codex also tolerates OpenSSL
`TRUSTED CERTIFICATE` labels and ignores well-formed `X509 CRL` sections in the
same bundle. If the file is empty, unreadable, or malformed, the affected Codex
HTTP or secure websocket connection reports a user-facing error that points
back to these environment variables.

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

## Realtime start instructions

`experimental_realtime_start_instructions` lets you replace the built-in
developer message Codex inserts when realtime becomes active. It only affects
the realtime start message in prompt history and does not change websocket
backend prompt settings or the realtime end/inactive message.

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
