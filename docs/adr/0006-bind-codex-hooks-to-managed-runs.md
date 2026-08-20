# Bind Codex Hooks To Managed Runs

Status: Accepted

Codex hooks are configured globally, so hook execution alone does not prove
which Boomux ShellRun owns a thread. Hook `session_id` is authoritative thread
identity only after the current host process has run-scoped authority. Boomux
therefore accepts Codex lifecycle reports only when a managed Codex launcher
explicitly sets `BOOMUX_CODEX_RUN_SCOPED=1` alongside the exact Shell and run
environment.

Eligible bare chat, `resume`, and `exec` invocations in managed login Shells pass
through a scoped executable shim and hidden launcher. When Boomux's installed
hook handlers are current, that launcher prepends `--enable hooks` and sets the
run-scoped marker. Option-led invocations including explicit `--remote`, other
subcommands, use outside a managed ShellRun, and absent or modified hook
installation run stock Codex without lifecycle authority. A configured primary
Codex command retains its exact executable through `BOOMUX_REAL_CODEX` while the
launcher adds hooks. Scheduled dispatch and cold recovery use the same launcher
while retaining their exact descriptor argument vectors and stdin.

The integration merges only exact `boomux codex hook` handlers into Codex's
shared `hooks.json`. It preserves unrelated fields and handlers, requires force
to replace modified Boomux handlers, and removes only Boomux-owned entries on
uninstall. Users must restart Codex and explicitly review and trust the hook with
`/hooks`; file installation is not treated as runtime trust.

Rejected alternatives were:

- Accept every global Codex hook, because an inherited daemon environment could
  assign lifecycle evidence to the wrong ShellRun.
- Infer ownership from process ID, working directory, thread recency, or catalog
  state, because none proves the exact current run.
- Replace the complete hooks file, because it is shared user configuration and
  Boomux does not own unrelated handlers.
- Construct a Codex Remote URL from the thread ID, because Codex documents no
  authoritative thread-specific Remote handoff contract.

The bounded experimental app-server catalog remains presentation-only. It may
project historical threads with Unknown state, but cannot grant lifecycle
authority. This design reuses existing Agent, Session, Schedule, and recovery
contracts and adds no protocol or durable-state representation.
