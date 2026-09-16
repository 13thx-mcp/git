# Changelog

All notable changes to Rust Git MCP are documented here.

## Unreleased

### Added

- `git_history_plan` to inspect branch-local commits relative to a validated base, including merge-base, oldest-to-newest commit metadata, dirty state, upstream publication detection, Conventional Commit status, and rewrite blockers.
- `git_validate_merge_readiness` for read-only enforcement/reporting of protected-branch state, optional clean-tree policy, branch commit limits, and optional Conventional Commit validation.
- `git_amend_commit` for bounded HEAD-only amendment on non-`main` branches. The implementation can replace the message and optionally commit the current index without exposing arbitrary amend flags.
- `git_reword_commits` for semantic branch-local commit-message rewriting with old-to-new SHA mapping.
- `git_squash_commits` for squashing one contiguous oldest-to-newest branch-local commit range into one meaningful commit.
- `RewritePolicy::{local_only, allow_published}` with `local_only` as the default for history-changing operations that accept a policy.
- Structured rewrite-failure responses reporting the original/current HEAD and whether the branch ref remained restored.
- Conventional Commit subject validation for `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`, `ci`, `chore`, `style`, and `revert`, including optional scopes and breaking-change `!` syntax.
- Integration-style coverage for history planning, merge readiness, amend, reword, squash, local-vs-published history safety, final-tree preservation, read-only rejection, and existing branch lifecycle behavior.

### Changed

- History normalization uses semantic Git plumbing rather than interactive rebase: replacement commit objects are created first, the resulting HEAD tree is verified against the original tree, and the checked-out branch ref is moved once with compare-and-swap `git update-ref <ref> <new> <old>` semantics.
- Rewrite operations require a clean worktree/index where suffix replay is needed and reject detached HEAD, protected `main`, unsupported merge-aware replay, branch-external selections, non-contiguous squash selections, and published rewritten suffixes under the default policy.
- Publication checks distinguish an upstream that only contains older commits from an upstream that already contains commits whose IDs would be rewritten.
- Existing Git invocations remain non-shell-based and continue disabling repository hooks with `core.hooksPath=/dev/null`.
- Commit, tag-message, path, revision, object-id, count, and history-size validation is bounded more explicitly.

### Security

- No raw Git, arbitrary Git argument, shell execution, interactive rebase, force push, force reset, or force branch deletion interface was added.
- `git_amend_commit`, `git_reword_commits`, and `git_squash_commits` are rejected in `--read-only` mode.
- `git_amend_commit` blocks rewriting HEAD when it is already reachable from the configured upstream.
- Reword/squash default to `local_only`; rewriting a suffix containing an upstream-reachable commit is blocked unless the caller explicitly selects `allow_published`.
- History normalization never pushes, and therefore never force-pushes, as part of rewriting.
- Reword and squash prove final `HEAD^{tree}` equivalence before changing the branch ref.
- Failure before the compare-and-swap ref update leaves the original branch ref unchanged; unattached replacement objects are harmless and recoverable through normal Git object retention.

### Deferred

- `git_reorder_commits` is intentionally deferred from the first history-normalization release. Squash + reword cover the required `dev-git-control` normalization workflow with a smaller mutation surface.
- Explicit `git_rewrite_abort` / `git_rewrite_continue` operations are not needed by the current implementation because it does not create rebase or cherry-pick in-progress state.
