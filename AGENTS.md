# Repository Workflow

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
