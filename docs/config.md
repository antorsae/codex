# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

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
