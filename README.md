# Rust Git MCP

Workspace-confined Git MCP server for Aira/ChatGPT development workflows.

The server exposes strongly typed Git operations rather than arbitrary shell or raw `git args...` execution. Its default workflow is aligned with `dev-git-control`: protected `main`, short-lived work branches, Conventional Commits, branch-history normalization before merge, explicit `--no-ff` integration, release preparation as the final work-branch commit, and annotated release tags after merge.

## Workspace model

`--root` is a workspace boundary, not a single Git repository. Every operation takes a workspace-relative repository selector such as:

```json
{ "repo": "riscvx" }
```

Repository selectors must resolve exactly to a Git repository root below the configured workspace boundary.

## Safety model

The MCP intentionally does **not** expose arbitrary Git commands, shell execution, interactive rebase, `reset --hard`, forced branch deletion, or force push.

Core protections:

- `main` is the protected trunk.
- normal `git_commit` operations on `main` are rejected; only completion of an explicit merge-in-progress is permitted there.
- authored commit/reword/squash messages must follow Conventional Commits.
- branch-local rewrite operations reject `main`, dirty worktrees, detached HEAD, unsupported merge-aware replay, and published rewritten commits under `local_only`.
- merge is `--no-ff` only, runs on `main`, requires a clean tree, and uses a Conventional Commit-compatible merge message.
- release tagging has a dedicated `git_release_tag` operation that requires clean `main` with a two-parent merge at `HEAD`; enrolled repositories also require exact trusted release-gate evidence.
- release-history recovery is exposed only through guarded semantic operations; arbitrary ref reset/tag movement is not exposed.

## Remote permissions

Remote reads and writes are separated:

```text
--allow-remote-read   # fetch, pull, publication verification
--allow-remote-write  # push
--allow-remote        # backward-compatible: enables both
```

`git_push` never accepts force refspecs.

Publication-sensitive recovery operations default to `publication_guard=verify_remote`. This refreshes the selected remote and blocks rewriting when the merge/tag is already published.

`publication_guard=caller_confirmed_unpublished` is an explicit escape hatch for environments where publication was verified externally. It never pushes or force-pushes.

## dev-git-control workflow

The intended structured workflow is:

```text
git_history_plan(repo, base=main)

# Normalize when branch-local history is too large or malformed.
git_squash_commits(..., rewrite_policy=local_only)
git_reword_commits(..., rewrite_policy=local_only)

# Project tooling updates VERSION/changelog and commits release preparation.
git_commit("chore(release): prepare vX.Y.Z")

git_dev_control_readiness(repo, version=X.Y.Z)

git_switch(branch=main)
git_merge(source=<work-branch>, mode=no_ff)

# Run project-native post-merge checks outside this MCP.
# Enrolled repositories must produce exact PASS evidence for current main HEAD.
git_release_tag(version=X.Y.Z, release_evidence_path=<relative evidence path>)
git_delete_branch(<work-branch>)
```

`git_dev_control_readiness` hard-codes the policy-critical merge gate:

- current branch is a named work branch, not `main`;
- clean worktree/index;
- work branch contains current `main` tip;
- 1..5 branch-local commits;
- every branch-local commit follows Conventional Commits;
- final commit is exactly `chore(release): prepare vX.Y.Z`.

The lower-level `git_validate_merge_readiness` remains available for diagnostic/custom policy use, but its commit limit cannot be relaxed above the `dev-git-control` maximum of 5.

## History normalization

### `git_history_plan`

Reports:

- branch and merge-base;
- branch-local commits oldest-to-newest;
- clean/dirty state;
- configured upstream;
- remote-tracking refs containing branch-local commits;
- Conventional Commit status;
- rewrite blockers.

### `git_reword_commits`

Rewrites selected Conventional Commit messages using semantic commit replay. Descendants are replayed because parent IDs change. The final `HEAD^{tree}` must equal the original tree before the branch ref is changed.

### `git_squash_commits`

Squashes one contiguous oldest-to-newest branch-local range into one Conventional Commit. Descendants are replayed, and final-tree identity is mandatory.

### `git_amend_commit`

Amends only `HEAD` on a non-main, unpublished work branch. The resulting subject must remain Conventional Commit-compatible.

All three operations use compare-and-swap ref updates and never push.

## Publication verification

`git_verify_unpublished` is the explicit remote proof operation for release recovery.

Example:

```json
{
  "repo": "riscvx",
  "remote": "origin",
  "commits": ["<merge-sha>"],
  "tags": ["v0.12.0"]
}
```

The operation fetches/prunes remote refs and tags, then reports whether any requested commit is reachable from the selected remote-tracking refs or any requested tag exists on the remote.

It requires `--allow-remote-read` and never pushes.

## Guarded release recovery

These commands exist specifically for repairing a local unpublished release without exposing generic destructive Git primitives.

### `git_rewind_merge`

Moves the current branch from an **expected two-parent merge commit** back to that merge's first parent using `git reset --keep`.

Guards:

- clean tree/index;
- current `HEAD` must exactly match `expected_merge`;
- expected commit must have exactly two parents;
- default remote verification must prove the merge unpublished;
- after the operation, `HEAD` must equal first parent and tree/index must remain clean.

This is intentionally not a generic reset command.

### `git_replace_local_tag`

Atomically replaces an existing local tag using compare-and-swap `update-ref` semantics.

Guards:

- current tag target must equal `expected_old_target`;
- default remote verification must prove the tag unpublished;
- replacement is an annotated tag;
- the replacement tag object is created first, then the real tag ref is swapped only if the old object still matches;
- final tag target is verified.

This is intended for recovery of a known-bad **local unpublished** release tag, not routine retagging.

## Merge behavior

`git_merge`:

- only operates when the current branch is `main`;
- requires a clean tree/index;
- supports `no_ff` only;
- accepts an optional Conventional Commit merge message;
- otherwise derives a message from the source branch, e.g. `feature/foo` -> `feat: merge feature/foo`.

Pre-merge readiness should be checked with `git_dev_control_readiness` before calling it.

## Release tagging

Prefer `git_release_tag` for releases rather than generic `git_tag`.

Example:

```json
{
  "repo": "riscvx",
  "version": "0.12.0",
  "message": "Release v0.12.0"
}
```

It requires:

- exact `MAJOR.MINOR.PATCH` version;
- current branch `main`;
- clean tree/index;
- two-parent merge at `HEAD`;
- target tag does not already exist;
- when the repository is enrolled with `--release-evidence-required-repo`, a trusted JSON evidence file below `--release-evidence-root` must prove `sonarqube-main` `PASS` for the exact current `main` HEAD and include a non-empty Sonar analysis ID.

Evidence enforcement is opt-in per repository so a baseline can be established and approved before the control becomes mandatory. Missing, stale, mismatched, or non-PASS evidence fails closed.

The resulting tag is annotated and named `vMAJOR.MINOR.PATCH`.

## Tool groups

### Inspection / policy

- `git_list_repositories`
- `git_status`
- `git_diff`
- `git_log`
- `git_show`
- `git_branches`
- `git_history_plan`
- `git_validate_merge_readiness`
- `git_dev_control_readiness`
- `git_verify_unpublished`

### Branch/history lifecycle

- `git_switch`
- `git_create_branch`
- `git_amend_commit`
- `git_reword_commits`
- `git_squash_commits`
- `git_merge`
- `git_delete_branch`

### Release recovery

- `git_rewind_merge`
- `git_replace_local_tag`

### Index/commit/tag

- `git_stage`
- `git_unstage`
- `git_restore`
- `git_commit`
- `git_tag`
- `git_release_tag`

### Remote

- `git_fetch`
- `git_pull`
- `git_push`

## Build and verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --all-targets --all-features
cargo audit
cargo build --release --locked
```

After changing tool schemas, rebuild the release binary and reload the Aira gateway so the tool catalog is regenerated.