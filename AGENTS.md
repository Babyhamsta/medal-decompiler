# Working in this repository

Follow system safety requirements and granted permissions. Use this project's
Git workflow when it differs from general Git defaults.

- Preserve unrelated tracked and untracked files, unmerged work, and private
  evidence. Use an isolated branch and worktree when the checkout has unrelated
  changes.
- Keep changes focused and run relevant checks. Preserve existing dependency
  locks unless a dependency change is part of the task.
- Keep credentials, personal paths, private samples, and generated work reports
  out of commits. Store run evidence locally in ignored paths.
- Use finite authored controls for execution checks; do not execute unknown or
  protected samples. Parsing and recompilation alone do not establish behavior.

## Git workflow

- Push completed work for review and use PRs into `main` for integration.
- When focused checks pass and review approves the exact revision, merge the PR
  without asking for routine approval again. Recheck its head, current `main`,
  required checks, and protections first. Respect repository protections and
  execution approvals.
- Delete a PR branch only after verifying that its PR is merged, its work is
  preserved in `main`, and it contains no unmerged commits. Preserve branches
  without a verified merged PR, unfinished worktrees, and private evidence.
- Ask before permission or protection changes, force pushes, loss of unmerged
  work, or other materially risky actions. If approval is denied, stop the
  blocked action and report the exact operation and reason; do not bypass the
  denial through another route.
