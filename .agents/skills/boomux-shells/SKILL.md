---
name: boomux-shells
description: Read output and logs from Boomux workspace shells. Use when asked to inspect another shell by name, read shell2, examine terminal output, check logs from another terminal, or inspect a Herdr terminal ID.
compatibility: Requires boomux and herdr on PATH. Shell-name lookup requires running inside a Boomux-managed Herdr pane.
metadata:
  author: boomux
  version: "1"
---

# Boomux Shells

Use Boomux to inspect output from another persistent shell without asking the
user to copy terminal contents.

## Read A Shell

When the user provides a shell name or terminal ID, run:

```console
boomux read "<name-or-terminal-id>" --lines 200
```

Use the returned text to answer the user's question. Increase `--lines` when
the relevant output is older. This reads retained terminal scrollback, which
may not contain a process's complete historical log.

## Discover Shells

When the target is missing, unclear, or not found, run:

```console
boomux shells
```

Match the user's wording against the displayed shell names. Ask for
clarification only when multiple shells remain plausible.

Shell names are resolved within the current Boomux workspace. Exact Herdr
terminal IDs can be read directly.
