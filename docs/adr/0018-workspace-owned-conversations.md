# Workspace-owned conversations

Status: Accepted.

Users need to return to conversations in the Workspace where they used them,
regardless of harness, without reviving the global provider-history catalog
retired by ADR 0014.

Desktop adds a right-side Conversations panel, opened by a sidebar button and scoped to one selected Workspace. Entries are
projections of its retained Agent records, grouped by integration and exact
external conversation identity. Node and Workspace IDs retain ownership; names,
working directories, and worktrees do not create or transfer associations.
There is no import of conversations created outside Boomux in this first version.
No additional durable schema is introduced. Workspace deletion already removes
the Agent records and therefore removes every entry. Shell deletion does not.
Desktop retains all user Workspaces until explicit removal, including those
without Shells or recorded conversations. Harness-owned conversation history is never deleted.

Protocol 55 introduces a narrowly scoped open operation. Under the owner's
mutation gate it revalidates the Agent/Workspace relationship, returns an exact
current Shell when available, or prepares a native resume command with literal
arguments in a new Shell in that existing Workspace. Pending resumes are reused,
and an operation carries an exact Shell ID for retry. Old peers reject the
operation; remote conversations are never launched on the coordinator.

Desktop performs discovery and requests off the UI thread only while the tab is
open. The existing overview worker drives a single refresh, at most once every
three seconds, for the selected Workspace. Entries are rendered in pages of 50.
Opening focuses an existing pane or adds a tiled pane in the selected Workspace.
The owner-scoped `ListWorkspaceConversations` host service enriches recorded
entries using bounded, cached harness title readers. Only exact harness and
external-session identities are matched; catalog-only records are discarded.
Agent names remain the fallback when a harness title is unavailable. Desktop
performs this read off the UI thread only while the right-side panel is open.

This supersedes ADR 0014 only for Workspace-scoped discovery and native resume.
Its prohibition on global Session catalogs and the rejection of legacy Session
list/inspect/mutation/resume APIs remain in force. Agent lifecycle state remains
run-scoped and is never inferred from the conversation entry.
