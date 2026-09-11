# Kiro CLI v2 and v3

Boomux provides separate `kiro-v2` and `kiro-v3` integrations. **Typing
`kiro-cli` preserves Kiro's own default engine and your exact arguments.**
Boomux never adds `--v3`. If your Kiro settings select v3 by default, those
settings still apply.

| Integration | Setup | Start in a managed ShellRun |
| --- | --- | --- |
| Kiro v2 | `boomux integration setup kiro-v2` | `kiro-cli chat --agent-engine v2 --agent boomux-v2` |
| Kiro v3 | `boomux integration setup kiro-v3` | `kiro-cli --v3` |

These are engine versions; an installed CLI with a `2.x` package version can
also offer the opt-in v3 engine. See [dated validation](lifecycle-validation.md)
for the exact versions and behavior exercised.

## Installation and selection

Automatic integration maintenance prepares both assets. Setup, status, install,
and uninstall can address each integration independently:

```console
boomux integration status kiro-v2
boomux integration status kiro-v3
boomux integration uninstall kiro-v2
```

V2 owns `${KIRO_HOME:-$HOME/.kiro}/agents/boomux-v2.json`, a dedicated profile
with embedded hooks. Select it explicitly with `--agent boomux-v2`; installing
it does not change your default agent, selected model, or engine. It enables
built-in tools without granting blanket tool trust. Existing custom profiles
are untouched. To use your own profile, copy the four hook groups from the
bundled profile into your profile and keep the `boomux kiro hook-v2` commands.
Only profiles containing those hooks report v2 activity. Project profiles can
shadow global profiles with the same name; check which profile Kiro loads.

V3 owns `${KIRO_HOME:-$HOME/.kiro}/hooks/boomux.json`, the existing standalone
hook asset. The old setup name `kiro` remains an alias for `kiro-v3`, including
its existing ownership receipt and uninstall preference. V3 Agent records keep
the legacy `kiro` integration key, preserving history and exact resume. V2
records use `kiro-v2`; they are never resumed with a v3 command.

The runtime status cannot infer an engine from `kiro-cli` alone. It reports
`not_observable` until exact hook evidence identifies a version; a reporting v2
Session is not treated as a broken v3 installation, or vice versa. To verify a
specific live Shell, use `boomux integration verify kiro-v2 --shell <id>` or
the equivalent `kiro-v3` command.

Both installers preserve modified assets unless explicitly replaced with
`--force`. Uninstalling one leaves the other installed. Reopen an existing
managed ShellRun after upgrading Boomux to refresh its launcher shim.

## Lifecycle and notification limits

| Evidence | v2 profile | v3 standalone hooks |
| --- | --- | --- |
| Startup | Unknown | Unknown |
| Prompt submission, tool start, tool return | Working | Working |
| Finished responding | Not established | Stop reports Idle |
| Permission wait or error | No Blocked report | No Blocked report |
| Host exit | No profile exit cleanup | Final supervised holder release reports Inactive |
| Permanent Session completion | No Done report | No Done report |

On CLI `2.21.1`, the v2 terminal probe emitted startup and prompt hooks for
the same Session. Headless mode emitted a prompt hook but no startup hook.
Tool-event decoding is covered by fixtures; tool execution was not part of
these host probes.

V2 deliberately does not install a Stop handler. The legacy documentation calls
`agentStop`/`stop` a Session-end boundary, while v3 defines Stop as finishing a
response. We have not established a reliable v2 turn-idle or inactivity signal.
V2 can therefore remain Working after a response or after returning to the
shell prompt; it does not provide reliable ready-for-input notifications. Quiet
output, a returned tool, and process exit are not inferred completion signals.
V2 has no automatic exact Session resume or title discovery capability.

V3 Idle means a resumable turn has finished, not that the Session is permanently
Done. Notifications still depend on your Boomux notification settings. V3
tracking requires a current hook asset, the managed launcher, and an explicit
leading `--v3`. Cloud execution, service commands, absolute executable paths
typed in a login shell, and PATH changes that bypass the shim do not establish
local v3 lifecycle authority. On CLI `2.21.1`, the tested terminal UI emitted
SessionStart, UserPromptSubmit, and Stop for the same Session, but the
`--no-interactive` probe emitted none of the capture hooks despite completing
its response. Do not rely on v3 headless notifications on this host version.
No exact Kiro Web handoff is provided.

V2 hooks use the canonical `session_id` and exact managed ShellRun environment;
missing identity or unsupported events fail open without reporting a guessed
Session. V3 hooks additionally require the supervised Launch Holder. Hooks
produce no stdout and do not decide tool permissions. Missing or delayed host
hooks limit what Boomux can report; they are not reconstructed from terminal
text.

Upstream references: [CLI 2.x hooks](https://kiro.dev/docs/cli/2x-reference/),
[v3 hooks](https://kiro.dev/docs/cli/hooks/), and
[engine migration](https://kiro.dev/docs/cli/v3/).
