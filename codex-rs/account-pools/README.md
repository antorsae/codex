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

`--account ALIAS` selects a single account and waits when it runs out of quota. It cannot be combined with `--pool NAME`. Explicit selection overrides a saved session selection, then the user default. Resume and fork restore the saved account within the selected pool. With no explicit or saved selection, defaults apply to locally managed ChatGPT authentication; other authentication modes retain their existing behavior.

## Commands

- `account add ALIAS [--device-auth]`, `account import ALIAS`, `account list`, and `account remove ALIAS` manage logins. Remove an account from its pools before deleting it.
- `account select ALIAS` and `account select --clear` set or clear the default for future sessions.
- `account usage [ALIAS] [--model MODEL]` shows identity, plan, memberships, quota windows, remaining percentages, reset times, banked credits, observation time and lookup errors. Unknown observations remain unknown.
- `account redeem ALIAS [--model MODEL]` explicitly redeems an existing usable credit. It does not purchase credits.
- `pool create NAME ALIAS...`, `pool update NAME ALIAS...`, `pool list`, `pool inspect NAME`, `pool remove NAME`, and `pool select NAME` manage pools. `pool select --clear` clears the default.
- Add `--no-auto-reset` to pool creation or update to wait for weekly recovery without spending banked credits. Updating a pool replaces its ordered membership and reset policy.
- Account and pool commands support `--json`; read-only list, usage and inspect commands also support `--watch` with a 60-second interval. Interrupt to stop watching.

TUI has `/accounts` and `/pools` with the corresponding commands. `/usage` shows named-account usage when a named selection is active. Account switches, verified redemptions and waiting deadlines appear in the conversation. Pool changes affect future sessions; an existing session retains its own selection and policy.

## Recovery policy

The current account remains selected until a model request is blocked. An alternative must have fresh usage, support the current model and have capacity in every applicable window reported by the backend, including model-specific limits. Accounts with only a short or only a weekly ordinary quota window are supported. Candidates rank by remaining quota in the exhausted window; weekly exhaustion takes precedence. Pool order breaks ties.

When all otherwise eligible accounts are weekly exhausted, automatic reset policy chooses the account with the most banked resets and its earliest-expiring usable credit. A short-window block with weekly capacity remaining waits without spending a reset. Failed or incomplete usage reads cannot authorize switching or redemption. Backend window durations, model availability, credit expiry and reset outcomes are authoritative.

Waiting sessions check every 60 seconds or at an earlier advertised reset. Network failures use bounded backoff up to five minutes. Pools contain at most 128 distinct accounts; bounded concurrent reads keep large-pool observations fresh.

Recovery happens at a model-request boundary after outstanding tool work settles, including during compaction. It retains conversation history, partial assistant output and completed tool results, clears account-bound transport state, and retries the same model. Completed tools are not replayed. Cancellation stops monitoring. Durable recovery state is consulted only when a session is explicitly resumed and a new turn is submitted; no monitor survives process exit.

## Storage and safety

User-only definitions live in `$CODEX_HOME/accounts/accounts.json`. Repository configuration cannot supply account or pool definitions. Credentials use the existing file, keyring or auto credential store under identity-specific homes; ephemeral credential storage cannot persist named accounts. No credential values are included in account/pool API responses or usage errors.

Configuration writes, token refreshes and reset redemptions use interprocess locks. A durable reset intent and idempotency key are written before a request is sent. A timeout or process death leaves that intent available for reconciliation with the same credit and key. A successful reset must produce verified usable quota before another reset generation can begin. If usage remains stale, recovery waits rather than consuming another credit. These guarantees require a local filesystem that supports advisory locks and atomic replacement.

The configuration schema is `config.schema.json`. Regenerate it from `codex-rs` with:

```sh
cargo run -p codex-account-pools --example write_schema > account-pools/config.schema.json
```

## Validation map

The account-pools tests cover combined windows, ordering, model limits, unknown observations, expired credits, controlled-clock waits, cancellation, identity deduplication, session isolation and legacy refresh compatibility. Subprocess tests cover concurrent refresh and reset recovery after process death. Shared integration fixtures exercise HTTP and partial-stream recovery with completed tools, WebSocket transport replacement, compaction, app-server interruption/resume and `exec` JSON events. TUI snapshots cover controls, usage and recovery notices.
