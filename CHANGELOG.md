# Changelog

All notable changes to Rust Git MCP are documented here.

## Unreleased

### Added

- `git_dev_control_readiness` as the opinionated pre-merge gate for `dev-git-control`: clean non-main work branch, current `main` reconciliation, 1..5 branch-local commits, Conventional Commits, and an exact final `chore(release): prepare vMAJOR.MINOR.PATCH` commit.
- `git_verify_unpublished` to refresh a configured remote and prove selected commits/tags are unpublished before local history recovery.
- `git_rewind_merge` for guarded local release recovery from an expected unpublished two-parent merge back to its first parent using `git reset --keep`; arbitrary reset is still not exposed.
- `git_replace_local_tag` for compare-and-swap replacement of a known existing local unpublished tag after checking the expected old target.
- `git_release_tag` to create an annotated `vMAJOR.MINOR.PATCH` tag only from a clean `main` whose HEAD is a two-parent merge result.
- Independent remote-read and remote-write capability switches: `--allow-remote-read` and `--allow-remote-write`, with legacy `--allow-remote` retaining backward-compatible behavior.
- Publication evidence from remote-tracking refs in `git_history_plan`.
- `PublicationGuard::{verify_remote, caller_confirmed_unpublished}` for explicit release-recovery safety semantics.
- Integration-style coverage for dev-git-control readiness, protected-main commit policy, no-ff Conventional merge messages, guarded merge rewind, atomic tag replacement, and release tagging.

### Changed

- `git_commit`, `git_amend_commit`, `git_reword_commits`, and `git_squash_commits` now require Conventional Commit-compatible authored subjects.
- Normal commits on protected `main` are rejected; `git_commit` only permits `main` when completing an explicit merge in progress.
- `git_merge` now requires current branch `main`, a clean worktree/index, explicit no-fast-forward semantics, and a Conventional Commit-compatible merge message. If no message is supplied, one is derived from the work-branch prefix.
- `git_validate_merge_readiness` now checks non-empty branch history, current-base reconciliation, optional final release-prep commit policy, and clamps the maximum commit count to the `dev-git-control` hard limit of 5.
- Local-only history rewrite safety now considers remote-tracking refs as publication evidence in addition to a configured upstream.
- Remote permissions distinguish read/update operations from publication operations so a workflow can verify/fetch without enabling push.
- Release documentation now recommends `git_release_tag` rather than generic `git_tag` for normal releases.

### Security

- No raw Git, arbitrary argument endpoint, shell execution, interactive rebase, force push, forced branch deletion, `reset --hard`, or generic reset/ref-move interface was added.
- `git_rewind_merge` defaults to remote publication verification, requires an exact expected merge SHA, clean state, and a two-parent merge, and uses `reset --keep` rather than a destructive hard reset.
- `git_replace_local_tag` defaults to remote publication verification and uses compare-and-swap tag ref replacement after verifying the expected old target.
- Publication-sensitive recovery never pushes and never force-pushes.
- `git_release_tag` refuses to overwrite or silently move an existing release tag.
- History normalization continues to preserve final-tree identity before branch ref updates.
