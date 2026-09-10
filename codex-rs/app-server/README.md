# App-server account and pool API

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
