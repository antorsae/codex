# User verification cancellation (experimental)

Local UI clients can cancel a native user-verification RPC by sending
`userVerification/cancel` with `{requestId}` and the `experimentalApi` opt-in.
The result is an empty acknowledgment (`{}`). This API does not enable desktop
verification capability advertisement.

`requestId` is the original status, enroll, delete, or verify RPC's string or
integer ID on the same connection, not the server elicitation ID. Use fresh IDs
for each operation and a distinct ID for the cancel RPC. Unknown, finished,
unrelated, and other-connection requests are no-ops.

The acknowledgment confirms the cancellation signal without waiting for the OS
prompt to close. The original RPC completes independently, with
`cancelled/interrupted` when cancellation prevents completion. Cancellation
cannot roll back completed effects. It remains effective while a proof waits for
outbound queue capacity, but cannot retract a response already enqueued.

Canceling or resolving an elicitation does not itself stop a separate
`userVerification/verify` RPC. Clients must cancel that RPC separately and discard
late proofs after the approval is canceled or resolved. Only one native worker
runs per app-server; if an OS call remains active after cancellation or timeout,
subsequent local operations return `failed/providerError` until that worker exits.

# Hosted Codex Apps MCP protocol

The host-owned HTTP `codex_apps` server uses Legacy by default in app-server and
standalone Codex. To discover the 2026-07-28 protocol, set
`codex_apps_mcp_2026_07_28 = true` under `[features]`, or send a true runtime
override via `experimentalFeature/enablement/set`. Discovery falls back to Legacy
when the server does not support it. Explicit config takes precedence.
The dedicated setting does not apply to third-party HTTP or local `codex_app`
stdio servers. The existing `mcp_2026_07_28` flag still governs eligible other
servers, regardless of whether their names or URLs resemble hosted Apps.
App-server does not persist this selection.

# Thread removal

`thread/archive` and `thread/delete` reject attempts to remove a live internal
worker with JSON-RPC error `-32600`. The worker's owner controls its shutdown.
For example, a Guardian reviewer remains available to its parent conversation
after a client tries to archive or delete it.

After the owner releases the worker, its saved conversation can be archived or
deleted normally. Ordinary client-controlled threads keep their existing behavior.

# Amazon Bedrock authentication

If `model_providers.amazon-bedrock.aws.credential_export` is configured, Bedrock setup and
Bedrock login return an error without changing configuration or saved credentials. Remove the
exporter configuration before selecting another credential source. `aws.credential_export` and
`aws.profile` cannot be configured together.

## Locally managed accounts and pools

`account/manage` adds native multi-account operations. Its `action` is one of `add`, `import`, `list`, `select`, `remove`, `usage`, `redeem`, `resolve`, or `models`. `alias` names a stored ChatGPT account; `model` requests model-specific usage. `add` starts existing OAuth or device login (`deviceAuth: true`) and returns a login descriptor. Use `account/login/cancel` and `account/login/completed` for the login lifecycle. Credentials remain on the server.

`pool/manage` accepts `create`, `list`, `read`, `update`, `select`, or `remove`. A `pool` contains `name`, ordered `accounts`, and `redeemWeeklyResets`. Creation enables the combined weekly/short-window policy; `redeemWeeklyResets` controls automatic use of existing banked resets. Selecting with a null account alias or pool name clears the user default. Configuration is server-local and user-only.

Both management operations paginate with `cursor` and `limit` (1–100, default 25), returning `data` and `nextCursor`. Usage returns credential-free observations with nullable availability and reset-credit details. Failed reads do not imply available quota.

`thread/start`, `thread/resume`, and `thread/fork` accept an optional selection:

```json
{"accountSelection":{"type":"pool","name":"work"}}
```

The other selection type is `account`. Explicit selection takes precedence over saved session state and the user default. Each thread owns its selection. An already loaded thread rejects a conflicting selection; unload and resume it to change that selection. `account/manage` with `action: "resolve"` or `"models"`, optional `accountSelection` and `threadId`, resolves bootstrap account/model metadata for that selection without changing the global login.

`thread/accountPool/updated` reports `selected`, `switched`, `redeemed`, and `waiting` events with a `threadId`. Waiting includes a reason and Unix-second `nextCheckAt`. Quota recovery preserves the selected model and completed tool results. `turn/interrupt` cancels monitoring; resuming a thread does not start inference until a new turn is submitted. These methods do not purchase credits.
