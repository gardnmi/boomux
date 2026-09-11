# Kiro CLI engines

**Use `kiro-cli` normally.** Boomux preserves the command's exact arguments and
lets the installed Kiro choose its engine and agent. Depending on the installed
release and Kiro settings, a bare launch may run v2 or v3. Boomux does not add an
engine flag, select a different agent, or require a special agent profile.

Explicit Kiro options such as `--v3` or `--agent reviewer` remain unchanged.
The package version alone does not determine the running engine: some `2.x`
packages also include the opt-in v3 engine.

## Support by engine

| Engine that Kiro runs | Automatic integration | Limits |
| --- | --- | --- |
| v2 | Ordinary terminal behavior and a foreground Kiro hint | Automatic Session lifecycle reporting and notifications are unavailable. |
| v3 | Standalone hooks report canonical Session activity through the managed launcher, including bare launches | Requires hooks to fire; see headless and notification limits below. |

The integration inventory lists `kiro-v2` and `kiro-v3` separately and explains
v2's limitation. V2 has no installable lifecycle asset. Kiro v2 embeds hooks in
agent configurations and does not provide the global hook facility needed to
instrument the normal built-in agent. Boomux leaves your agent configurations,
default agent, prompts, tools, and permissions alone. It does not infer Working,
Idle, Blocked, Inactive, or Done from v2 terminal output or process exit.

## V3 installation

Automatic integration maintenance prepares the v3 hooks. Manual inspection and
setup are available with:

```console
boomux integration status kiro-v3
boomux integration setup kiro-v3
```

The asset is `${KIRO_HOME:-$HOME/.kiro}/hooks/boomux.json`. The old setup name
`kiro` remains an alias for `kiro-v3`, including its existing ownership receipt
and uninstall preference. V3 Agent records keep the legacy `kiro` integration
key, preserving history and exact resume.

Modified assets require explicit replacement with `--force`. Uninstall with
`boomux integration uninstall kiro-v3` to opt out. Reopen existing managed
ShellRuns after upgrading Boomux to refresh their launcher shim. Then launch
Kiro as usual.

The runtime status cannot infer an engine from the `kiro-cli` foreground name
alone. Before exact v3 hook evidence exists, it reports `not_observable`.
A Launch Holder authorizes reporting from one managed process; acquiring a
holder does not create an Agent or declare that v3 is running. Only recognized
v3 hook payloads establish a v3 Session.

## V3 lifecycle and notification limits

| Evidence | Report |
| --- | --- |
| SessionStart | Unknown |
| Prompt submission, tool start, tool return | Working |
| Stop: finished responding | Idle |
| Final supervised holder exits | Inactive |
| Permission wait or error | No Blocked report |
| Permanent Session completion | No Done report |

Idle means a resumable turn has finished. Notifications also depend on your
Boomux notification settings. On CLI `2.21.1`, the tested terminal UI emitted
SessionStart, UserPromptSubmit, and Stop for the same Session. Its headless
`--no-interactive` probe emitted none of the capture hooks despite completing
its response, so headless notifications are not validated for that version.

Cloud execution, service commands, and invocations that bypass the managed
launcher do not establish local v3 lifecycle authority. Absolute executable
paths typed in a login shell and modified PATHs can bypass the shim. No exact
Kiro Web handoff is provided. Missing hooks are not reconstructed from terminal
text. Legacy v2 event names cannot be interpreted as v3 lifecycle events.

See [dated host validation](lifecycle-validation.md) for the exact test scope.
Upstream references: [CLI 2.x hooks](https://kiro.dev/docs/cli/2x-reference/),
[v3 global hooks](https://kiro.dev/changelog/cli/2-13/), and
[v3 hook events](https://kiro.dev/docs/cli/hooks/).
