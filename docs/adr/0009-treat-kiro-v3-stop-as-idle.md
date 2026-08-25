# Treat Kiro V3 Stop As Idle

Status: Accepted

Kiro CLI v3 documents the standalone `Stop` hook as firing after the agent has
completed its turn and finished responding to the user. A live authenticated
Kiro CLI `2.18.0` turn produced `UserPromptSubmit`, then `Stop` after the final
assistant response and return to the input prompt. Boomux therefore reports
Working for prompt and tool activity and Idle for Stop on the exact Session
bound through the live Launch Holder.

Idle means that the observed turn finished and the Session can accept another
prompt. It does not mean the Session is permanently complete. Kiro still exposes
no standalone hook for permission waits, durable Session completion, or process
inactivity, so Boomux does not infer Blocked or Done. Final holder release remains
the authority for Inactive.

This supersedes only the Stop-to-Unknown decision in ADR 0007. Its managed-run
binding, holder authority, exact Session identity, and process-exit semantics
remain unchanged. Protocol 46 advertises the changed admission semantics; a new
client downgrades Stop to Unknown for a protocol-45 daemon. The wire shape and
durable representation are unchanged, so no persistence version change is
required.
