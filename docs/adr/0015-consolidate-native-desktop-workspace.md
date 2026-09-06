# Consolidate the native Desktop in the Boomux workspace

Status: Accepted, 2026-09-06.

Maintaining separate repositories, revision pins, CI pipelines, and releases for
the native Desktop and its backend created unnecessary coordination. Desktop is
now a Cargo workspace member under `desktop/` in the Boomux repository. The root
package remains the default member, so CLI builds do not require GUI tooling.

Both packages share a version, lockfile, CI validation, and Release Please
proposal. A GitHub release contains CLI-only archives and a Desktop bundle with
the exact tested CLI executable. A required failure on either side blocks the
combined release. Desktop can retain experimental status under the shared version.

This supersedes the repository-separation decision in Desktop ADR 0001; its
ownership rationale remains: the daemon owns PTYs, persistence, identities, and
protocols, while Desktop owns rendering, input, layout, and pane resources.
Existing CLI/remote/AUR asset names and installed user paths remain stable.
