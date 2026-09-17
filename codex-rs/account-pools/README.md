# Native accounts and pools

Named accounts keep multiple locally managed ChatGPT logins in one Codex home. A pool lets each session continue with another subscription when a model request reaches its quota. TUI, `codex exec`, and app-server use the same coordinator.

```sh
codex account import acct1
codex account add acct2 --device-auth
codex pool create work acct1 acct2
codex --pool work
codex exec --pool work 'Continue the investigation'
codex account usage --model gpt-5.5 --json --watch
```

Browser OAuth is the default for `account add`. Import copies the current stored ChatGPT login and links its refresh authority, so old single-account sessions and named sessions share token rotation. Reimporting the same authenticated user and workspace updates its existing alias; it does not add another quota slot. API keys, external token providers, workload identity and custom provider credentials retain their existing behavior.

`--account ALIAS` selects a single account and waits when it runs out of quota. It cannot be combined with `--pool NAME`. Explicit selection overrides a saved session selection, then the user default. Resume and fork restore the saved account within the selected pool. With no explicit or saved selection, defaults apply to locally managed ChatGPT authentication; other authentication modes retain their existing behavior. A model provider that brings its own credentials (an `env_key`, bearer token, or other auth block, as a proxy does) keeps accounts and pools inert: saved and default selections are ignored and only an explicit `--account` or `--pool` is an error, so switching `model_provider` in `config.toml` is enough to move between the pool and a proxy. Resumed threads follow that switch too: a thread persisted with one OpenAI route (the built-in provider or a proxy that mirrors it) resumes on the currently configured OpenAI route, while threads persisted with providers that are not OpenAI routes keep their own.

Fresh launches prefer the last account that completed a model response in the selected pool. `/clear` and `/new` inherit the current conversation's account and pool, even if another running session has since used a different account. Running sessions retain independent selections; explicit account or pool overrides still take precedence. Before the first inference, known exhaustion enters quota recovery. Unknown usage leaves the selected account in place.

## Commands

- `account add ALIAS [--device-auth]`, `account import ALIAS`, `account list`, and `account remove ALIAS` manage logins. Remove an account from its pools before deleting it.
- `account select ALIAS` and `account select --clear` set or clear the default for future sessions.
- `account usage [ALIAS] [--model MODEL]` shows identity, plan, memberships, quota windows, remaining percentages, reset times, banked credits, observation time and lookup errors. Unknown observations remain unknown.
- `account redeem ALIAS [--model MODEL]` explicitly redeems an existing usable credit. It does not purchase credits.
- `pool create NAME ALIAS...`, `pool update NAME ALIAS...`, `pool list`, `pool inspect NAME`, `pool remove NAME`, and `pool select NAME` manage pools. `pool select --clear` clears the default.
- Add `--no-auto-reset` to pool creation or update to wait for weekly recovery without spending banked credits. Updating a pool replaces its ordered membership and reset policy.
- Account and pool commands support `--json`; read-only list, usage and inspect commands also support `--watch` with a five-minute interval. Interrupt to stop watching.

TUI has `/accounts` and `/pools` with the corresponding commands. `/usage` shows named-account usage when a named selection is active. Account switches, verified redemptions and waiting deadlines appear in the conversation. Recovery reloads pool membership and policy from a complete configuration snapshot, including accounts added while waiting. Existing sessions retain their selected pool when the default changes.

## Recovery policy

The current account remains selected until the initial quota check or a model request reports exhaustion. An alternative must have fresh usage, support the current model and have capacity in every applicable window reported by the backend, including model-specific limits. Accounts with only a short or only a weekly ordinary quota window are supported. Candidates rank by remaining quota in the exhausted window; weekly exhaustion takes precedence. Pool order breaks ties.

When all otherwise eligible accounts are weekly exhausted, automatic reset policy chooses the account with the most banked resets and its earliest-expiring usable credit. A short-window block with weekly capacity remaining waits without spending a reset. Failed or incomplete usage reads cannot authorize switching or redemption. Backend window durations, model availability, credit expiry and reset outcomes are authoritative.

Waiting sessions re-read the pool at the earliest advertised reset, and otherwise every ten minutes for short-window waits or missing credit details and hourly for weekly exhaustion. Failed reads back off from ten to thirty minutes between pool re-reads, still honoring an earlier advertised reset. Pools contain at most 128 distinct accounts; bounded concurrent reads keep large-pool observations fresh.

## Shared observations

Every usage read is stored next to the account's credentials (`usage.json` and `model-slugs.json` under `accounts/credentials/<identity>/`), so concurrent sessions, sub-agents, footers and preflights do not repeat the same backend requests. Recovery and preflight reuse a read that is at most five minutes old, keep a weekly-exhausted account's last read for up to an hour before its reset, and only fetch banked reset details once the weekly window is exhausted. Verification after a redemption and before activating an account always reads fresh. Model catalogs are reused for a day; a requested model missing from a catalog older than ten minutes is re-checked once. Managed sessions also cache the selected account's model catalog for an hour instead of downloading it on every start.

A read is never reused once one of its windows has reset. A model request rejected for quota invalidates the selected account's shared read and holds the account unusable for other sessions for a minute, since usage lags such rejections. Quota windows that inference responses report are recorded for the selected account. Footers and other display surfaces accept them, and accept stored reads up to ten minutes old; recovery never decides from response-derived windows. Failed reads are not repeated within two minutes unless the caller asks for a fresh read. Every ChatGPT backend-API request made by the account tooling (usage, reset credits, catalog checks) is logged at debug level under the `codex_backend_client::http` target, and catalog downloads under `codex_http_client`, so request volume can be audited from the log database.

Recovery happens at a model-request boundary after outstanding tool work settles, including during compaction. It retains conversation history, partial assistant output and completed tool results, clears account-bound transport state, and retries the same model. Completed tools are not replayed. Cancellation stops monitoring. Durable recovery state is consulted only when a session is explicitly resumed and a new turn is submitted; no monitor survives process exit.

## Storage and safety

User-only definitions live in `$CODEX_HOME/accounts/accounts.json`. Repository configuration cannot supply account or pool definitions. Credentials use the existing file, keyring or auto credential store under identity-specific homes; ephemeral credential storage cannot persist named accounts. No credential values are included in account/pool API responses or usage errors.

The optional `$CODEX_HOME/accounts/pool-state.json` stores one remembered account per pool. Successful responses update it under the existing configuration lock with atomic replacement; it contains no credentials and cannot change the configured default. Existing per-conversation recovery files remain authoritative for resume and conversation handoff.

Configuration writes, token refreshes and reset redemptions use interprocess locks. A durable reset intent and idempotency key are written before a request is sent. A timeout or process death leaves that intent available for reconciliation with the same credit and key. A successful reset must produce verified usable quota before another reset generation can begin. If usage remains stale, recovery waits rather than consuming another credit. These guarantees require a local filesystem that supports advisory locks and atomic replacement.

The configuration schema is `config.schema.json`. Regenerate it from `codex-rs` with:

```sh
cargo run -p codex-account-pools --example write_schema > account-pools/config.schema.json
```

## Validation map

The account-pools tests cover combined windows, ordering, model limits, unknown observations, expired credits, controlled-clock waits, cancellation, identity deduplication, session isolation and legacy refresh compatibility. Subprocess tests cover concurrent refresh and reset recovery after process death. Shared integration fixtures exercise HTTP and partial-stream recovery with completed tools, WebSocket transport replacement, compaction, app-server interruption/resume and `exec` JSON events. TUI snapshots cover controls, usage and recovery notices.
