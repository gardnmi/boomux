# Agent details

Select **ⓘ** on an Agent row in the Agents panel to open its details drawer.
With an Agent selected in sidebar keyboard navigation, press **i**. Clicking the
row itself still opens its Shell. **Escape** or the backdrop closes the drawer;
**1**, **2**, and **3** select Overview, Skills, and MCP.

- **Overview** shows the harness, machine, Workspace, reported state, last report,
  working directory, attention, observed Git contexts, and exact Agent/run IDs.
- **Skills** lists discovered skill names and descriptions. Expand an entry for
  its source file and scope.
- **MCP** lists server definitions, transport, and any explicit enabled/disabled
  setting. Expand an entry for its source file.

**Open terminal** navigates to the Agent's Shell. **Refresh** reads configuration
again on the owning machine. **Copy details** copies an Agent summary and the inventory
as JSON with its inspection caveat for troubleshooting.

## What the inventory means

These are standard configuration locations, not a live capability report.
Finding a skill does not prove the current run loaded it. Finding an MCP server,
including one marked enabled, does not prove it is connected or authenticated.
Model, context usage, cost, connection health, and available tools are currently
not reported. Names that occur in multiple files retain separate source entries;
the drawer does not calculate precedence or the effective configuration.

| Harness | Skill directories | MCP files |
| --- | --- | --- |
| Codex | User `~/.agents/skills`, `~/.codex/skills`; system `/etc/codex/skills`; project `.agents/skills`, `.codex/skills` | User and project `.codex/config.toml` |
| Claude Code | User and project `.claude/skills` | `~/.claude.json`, including the repository's project entry; project `.mcp.json` |
| OpenCode | User `~/.config/opencode/skills`, `~/.claude/skills`, `~/.agents/skills`; project `.opencode/skills`, `.claude/skills`, `.agents/skills` | User `~/.config/opencode/opencode.json` or `.jsonc`; project `opencode.json` or `.jsonc` |
| Pi | User `~/.pi/agent/skills`, `~/.agents/skills`; project `.pi/skills`, `.agents/skills` | Extension MCP configuration is not inspected |
| Other harnesses | Overview available; inventory unsupported | Inventory unsupported |

Project locations are searched from the reported working directory upward to
the nearest `.git` boundary, at most 16 directories. Skills use `SKILL.md`;
common scalar and folded frontmatter descriptions are supported. Pi standalone
Markdown skills, custom paths, environment overrides (including `CODEX_HOME`
and `XDG_CONFIG_HOME`), managed settings, plugins, packages, and runtime
configuration changes are not resolved. User paths use the owning daemon's home.

Coverage references: [Codex skills](https://developers.openai.com/codex/skills/),
[Codex MCP](https://developers.openai.com/codex/mcp/),
[Claude Code MCP](https://code.claude.com/docs/en/mcp),
[OpenCode skills](https://opencode.ai/docs/skills/),
[OpenCode MCP](https://opencode.ai/docs/mcp-servers/), and
[Pi skills](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/skills.md).

## Ownership, refresh, and limits

Inspection requires protocol 55 on both the local daemon and remote owner.
Remote inspection uses verified Node routing and the exact Agent ID and ShellRun
ID. Files are read on that owner, never substituted with local files. Unsupported
or unavailable owners leave the sidebar's basic Agent information usable. A
failed refresh retains the previous snapshot with a visible stale-data message.

Opening or refreshing performs one background request; there is no polling or
scan of every Agent. Closing releases the drawer's snapshot; late results cannot
replace a subsequently opened drawer. The owner releases registry locks before
reading files and does not persist the inventory or publish lifecycle events.

Each inspection limits Skills and MCP definitions to 128 entries each, directory
entries to 4,096, skill recursion to eight levels, each file to 256 KiB, total
read bytes to 4 MiB, and warnings to 16. Reached limits are reported. Symlink cycles
are deduplicated, and nonregular files are skipped using nonblocking opens.

MCP command lines, URLs, arguments, environment values, headers, and skill
instruction bodies are excluded from the response and clipboard inventory.
Parse errors omit file contents. Names, descriptions, paths, and existing Agent
observation metadata remain visible and are included when copying details.
