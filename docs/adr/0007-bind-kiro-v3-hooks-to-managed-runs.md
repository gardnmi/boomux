# Bind Kiro V3 Hooks To Managed Runs

Status: Accepted

Kiro CLI v3 supports global standalone hooks whose payload contains the
canonical `session_id`, but a global hook alone does not establish which Boomux
ShellRun owns that Session. Boomux therefore accepts Kiro lifecycle reports only
when a managed launcher sets `BOOMUX_KIRO_RUN_SCOPED=1` alongside exact Shell and
run identity.

After the dedicated global Boomux hook asset is installed, an eligible bare
`kiro-cli` invocation in a managed Shell launches the v3 harness. Explicit
leading `--v3` invocations, scheduled dispatch, and cold recovery use the same
launcher without duplicating the flag. Kiro v2, unrelated commands, absolute
paths, modified PATHs, and missing or modified hook assets run unchanged and
untracked. The launcher preserves an exact configured executable through
`BOOMUX_REAL_KIRO` and strips private provenance from unrelated children.

Prompt and tool events establish Working. Kiro runs hooks in isolated sub-agent
executions, while standalone SessionStart and Stop payloads do not identify the
root execution. Those boundary events therefore establish Unknown rather than
Idle and cannot cause completion notifications. Kiro exposes no authoritative
permission-wait, inactivity, or permanent root Session completion event through
these hooks, so Boomux does not infer Blocked, Idle, Inactive, or Done. If a
process switches canonical Sessions without an end event, Boomux preserves both
histories and refuses ambiguous cold recovery.

Kiro cloud sessions are a separate remote execution environment. Local global
hooks do not establish cloud lifecycle authority, and Kiro documents no stable
exact-session browser URL that Boomux can derive from a local Session ID. Boomux
therefore adds no cloud catalog or native Kiro web handoff. The separate Boomux
browser-terminal controller is bound to an exact local ShellRun and does not
claim cloud or Kiro Session handoff authority.

Rejected alternatives were:

- Accept every global hook carrying inherited Boomux identity, because only the
  managed v3 launcher proves that the hook belongs to the intended host process.
- Modify Kiro's v2 custom-agent profiles, because that would alter user-selected
  agent behavior and would not provide one safe global installation target.
- Mark prior same-run Sessions inactive by recency, because one ShellRun can host
  more than one child process and Kiro provides no authoritative switch event.
- Add trust-all flags to scheduled work, because Kiro's permission policy belongs
  to the user and host configuration.
- Guess a Kiro Web route from a Session ID, because no exact handoff contract is
  documented.

This design reuses existing Agent, Session, Schedule, installation, and recovery
contracts and adds no protocol or durable-state representation.
