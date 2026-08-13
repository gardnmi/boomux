# Repository Workflow

## Start Here

Use repository documentation in this order:

1. `CONTEXT.md` defines canonical product terms and distinctions. Preserve those
   semantics when names in code are less precise.
2. `docs/architecture.md` describes the current implementation boundaries and
   cross-cutting invariants.
3. Contract documents such as `docs/cli-json.md`, `docs/event-stream.md`, and
   `docs/live-pty-handoff.md` govern their named interfaces and guarantees.
4. `docs/adr/` records accepted decisions and rationale.
5. `docs/lifecycle-validation.md` records compatibility evidence from specific
   host versions; it is evidence, not a general specification.
6. `docs/roadmap.md` is non-authoritative future intent. Documents marked as
   historical explain how the design was reached but do not override current
   architecture, source, or tests.

For exact protocol and persistence versions, source and compatibility tests are
authoritative. Start a change in the owning module listed in the architecture
module map, then read its colocated tests and the relevant contract document.

## Validation

Run the same checks as CI:

```console
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --lib --bins --locked
cargo test --test native_backend --locked -- --test-threads=1
bun test integrations/opencode/boomux.test.js integrations/pi/boomux.test.js
```

Run the narrowest relevant tests while iterating, then run the complete set
before opening a PR. Native backend tests are intentionally serial because they
exercise process, socket, PTY, and daemon lifecycle behavior.

### Testing By Change Type

| Change | Expected coverage |
| --- | --- |
| Protocol field or request | Protocol serialization/defaulting and minimum-version tests, client negotiation tests, and native mixed-version behavior |
| Persisted state shape | `state_store.rs` migration tests and a cold-recovery native test |
| PTY, attachment, or process lifecycle | Colocated unit tests plus serial `native_backend` scenarios |
| Graceful handoff | Serial native tests covering rollback, reconnect, PID preservation, and later cleanup |
| TUI state or rendering | Focused `tui.rs` model, input, and rendering tests |
| OpenCode or Pi reducer | The corresponding Bun integration test |
| Host compatibility claim | Focused fixtures plus an update to `docs/lifecycle-validation.md` when validated live |

## Safety And Compatibility

- `boomux daemon stop` terminates every managed process. Prefer
  `boomux daemon restart` when testing compatible rebuilt binaries.
- Preserve exact argument vectors; do not introduce shell interpolation into
  launchers, commands, process adapters, or integration execution.
- Treat shell runs and Agent instances as run-scoped. Never infer permanent
  Agent completion from process exit or quiet terminal output.
- Do not persist attachment environments. They are ephemeral startup input and
  may contain private data.
- Preserve persistence-before-event publication and the documented daemon lock
  order when changing coordinated mutations.

### Protocol Changes

- Bump `PROTOCOL_VERSION` only when wire behavior requires it.
- Update `Request::minimum_protocol_version` for new requests.
- Define additive defaults and old-client response filtering or downgrade
  behavior.
- Add round-trip, old-peer, client-negotiation, and native compatibility tests.
- Update advertised capabilities when the new behavior is integration-visible.
- Update the protocol history in `docs/architecture.md` and any affected
  contract document.

### Persistence Changes

- Bump `STATE_VERSION` when the durable representation changes.
- Retain the previous schema and add an explicit migration; never silently
  reinterpret persisted fields.
- Add migration, invalid-state, cold-recovery, and graceful-handoff coverage as
  applicable.
- Keep state bounded, owner-validated, atomically replaced, and reproducible.
- Document changes to recovery guarantees in `docs/architecture.md`.

## Commits

- Use Conventional Commits for every commit: `type(scope): description` or
  `type: description`.
- Use `feat` for user-visible capabilities, `fix` for user-visible corrections,
  and `perf` for performance improvements. Use `docs`, `test`, `refactor`,
  `build`, `ci`, or `chore` when the change is not release-note material.
- Mark breaking changes with `!` after the type or scope and explain them in a
  `BREAKING CHANGE:` footer.
- Keep the description imperative, lowercase, and concise.

## Pull Requests And Releases

- Use a Conventional Commit title for every PR. The title must describe the
  release impact of the complete PR, for example `feat: add workspace previews`
  or `fix(agent): preserve lifecycle registration`.
- Squash-merge PRs into `main` so the conventional PR title becomes the
  main-branch commit consumed by Release Please.
- Before merging, update the PR title if its release type or scope changed.
- If a PR contains existing non-conventional commits, add a Release Please
  commit override to the PR body and squash-merge it:

  ```text
  BEGIN_COMMIT_OVERRIDE
  feat: describe the complete release-visible change
  END_COMMIT_OVERRIDE
  ```

- `feat` produces a minor release, `fix` and `perf` produce a patch release, and
  a breaking change produces a major release under the default Release Please
  versioning strategy.
