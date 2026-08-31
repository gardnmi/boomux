# Record Bounded Agent Working Contexts

Status: Accepted

An Agent can be launched from one directory while working across several Git
worktrees. The registration-time Agent `cwd`, retained Shell cwd, and Workspace
default cwd are launch configuration, not evidence of later work. Boomux records
working context separately instead of changing those established meanings or
making an Agent Session durable.

Only lifecycle integrations bound to an exact active Agent, Shell, and ShellRun
may submit an observation. They submit structured absolute cwd, directory, or
allowlisted tool-path fields. The owning Node canonicalizes an existing path and
resolves its Git worktree root, common-repository label, and branch. Transcript
text, terminal output, command strings, process trees, relative paths, arbitrary
tool payloads, and paths interpreted by another Node are rejected as authority.

Each Agent durably retains at most eight canonical roots newest-first. A repeated
current root/repository/branch tuple is an event-free no-op; changed metadata for
one root replaces and promotes that root. Persistence precedes the resulting
Agent event, and persistence failure restores the previous list. State schema 16
explicitly migrates schema 15 with empty lists, so upgrade does not reinterpret
historical activity.

Agent Sessions continue to be projections. The owner deduplicates roots across
the exact Agent occurrences, excludes the canonical launch root already
represented by launch context, exposes at most four newest remaining
repository/branch/time items and their total distinct count, and omits root paths
from Session list JSON. Explicit exact Session inspection may expose up to 64
deduplicated items so clients can present the detail hidden by the list summary;
only its first four receive response-time Git push and worktree inspection. The
Agent retains launch-root evidence. Catalog-only Sessions receive no contexts.
The presentation is bounded evidence, not a completeness claim, current lifecycle
signal, credential, or instruction to resume from a different directory.

The rejected alternatives are replacing `source_cwd`, inspecting only the launch
directory at read time, parsing transcripts or shell commands, storing
unbounded paths, and resolving remote paths on the presenting Node. They
respectively break launch semantics, miss later work, trust unstructured data,
permit state growth, or violate Node authority.
