# Bind Kiro V3 Hooks To Managed Runs

Status: Accepted; Stop lifecycle semantics superseded by ADR 0009

Kiro CLI v3 supports global standalone hooks whose payload contains the
canonical `session_id`, but a global hook alone does not establish which Boomux
ShellRun or concurrent Kiro process owns that Session. Boomux therefore accepts
Kiro lifecycle reports only through a bounded private Launch Holder acquired by
the managed launcher for its exact PID/start identity and current ShellRun.

After the dedicated global Boomux hook asset is installed, an eligible bare
`kiro-cli` invocation in a managed Shell launches the v3 harness. Explicit
leading `--v3` invocations, scheduled dispatch, and cold recovery use the same
launcher without duplicating the flag. Kiro v2, unrelated commands, absolute
paths, modified PATHs, and missing or modified hook assets run unchanged and
untracked. The launcher preserves an exact configured executable through
`BOOMUX_REAL_KIRO`, supervises exact argv with inherited terminal behavior, and
strips private holder and launcher provenance from unrelated children.
It installs a kernel parent-death signal on the exact child rather than changing
the holder's signal handlers. Foreground Ctrl+C therefore retains normal process
group and shell-status semantics, while direct holder termination cannot orphan
the managed Kiro child.

Prompt and tool events establish Working. Kiro runs hooks in isolated sub-agent
executions, while standalone SessionStart and Stop payloads do not identify the
root execution. Those boundary events therefore establish Unknown rather than
Idle and cannot cause completion notifications. Kiro exposes no authoritative
permission-wait, inactivity, or permanent root Session completion event through
these hooks, so Boomux does not infer Blocked, Idle, or Done. Managed process exit
is separate direct evidence: release or confirmed death of the final holder for
one canonical Session reports Inactive at lifecycle integration authority. More
than one concurrent holder may own the same Session, and release of a non-final
holder leaves it active. Late hooks from dead holders are rejected by Boomux but
remain fail-open to Kiro. If a
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
- Bind only to ShellRun identity, because concurrent Kiro processes in one run
  need distinct authority and independently observed exit.
- Add trust-all flags to scheduled work, because Kiro's permission policy belongs
  to the user and host configuration.
- Guess a Kiro Web route from a Session ID, because no exact handoff contract is
  documented.

Protocol 45 adds closed holder acquire, hook report, and release operations.
Handoff generation 7 transfers only live exact process holders and their bounded
Session/Agent associations. Cold recovery inherits none. The design adds no
durable-state representation and does not change `STATE_VERSION`.
The daemon bounds this state to 256 holders and 16 Sessions per holder and reaps
dead holders autonomously once per second, with immediate reconciliation for
holder operations and handoff. Acquire and handoff import both revalidate the
exact current ShellRun at their final authority boundary. Dead-holder cleanup may
signal only an acquired holder-led process group that still contains a process
carrying that holder's private capability.
