# Rust Git MCP

Workspace-confined, multi-repository Git MCP server for Aira/ChatGPT development workflows.

The server exposes strongly typed Git operations instead of an arbitrary `git args...` or shell execution endpoint. A single MCP process can safely operate on multiple Git repositories below one configured workspace boundary.

## Workspace model

`--root` is a **workspace boundary**, not a Git repository.

```text
/Users/xiivth/workspaces/signs/
├── sagittarius/
├── riscvx/
├── mcp-server/
└── scripts/
```

Start the server with:

```bash
rust-mcp-git --root /Users/xiivth/workspaces/signs
```

Every repository-specific operation uses a workspace-relative selector such as:

```json
{
  "repo": "riscvx"
}
```

The selector must resolve to the Git repository root exactly; selecting a subdirectory of a repository is rejected.

## Security model

For every repository operation the server:

1. rejects empty, overlong, absolute, parent-traversing, NUL/control-character repository selectors;
2. canonicalizes `workspace_root/repo` and proves it remains below the configured workspace root;
3. verifies the target is a directory and a Git repository;
4. canonicalizes Git's `--show-toplevel` and requires an exact repository-root selector;
5. invokes Git directly without a shell;
6. disables repository hooks with `-c core.hooksPath=/dev/null`;
7. rejects option-like user tokens where they would be unsafe;
8. uses bounded, typed MCP schemas rather than arbitrary Git argument arrays.

Additional controls:

- repository discovery does not follow directory symlinks;
- file/path operations reject absolute paths, `..`, NUL, and control characters;
- branch names are checked with `git check-ref-format --branch`;
- commit-ish inputs are validated before use;
- force push, forced branch deletion, arbitrary reset, raw Git execution, shell execution, and interactive rebase are not exposed;
- remote operations require `--allow-remote`;
- history-changing operations are blocked in `--read-only` mode.

> Git repositories themselves are trusted input. Repository configuration and attributes can influence Git behavior.

## Read-only semantics

`--read-only` means **no local Git repository mutation**.

Rejected in read-only mode include:

- `git_stage`
- `git_unstage`
- `git_restore`
- `git_commit`
- `git_tag`
- `git_switch`
- `git_create_branch`
- `git_merge`
- `git_delete_branch`
- `git_fetch`
- `git_pull`
- `git_amend_commit`
- `git_reword_commits`
- `git_squash_commits`

`git_history_plan` and `git_validate_merge_readiness` are read-only and remain available.

`git_push` does not mutate local state but still requires `--allow-remote`. Force push is never exposed.

## MCP tools

### Discovery and read operations

- `git_list_repositories`
- `git_status`
- `git_diff`
- `git_log`
- `git_show`
- `git_branches`
- `git_history_plan`
- `git_validate_merge_readiness`

### Local branch lifecycle

- `git_switch`
- `git_create_branch`
- `git_merge` (`no_ff` only)
- `git_delete_branch` (`git branch -d` semantics only)

### Local file/index and release mutation

- `git_stage`
- `git_unstage`
- `git_restore`
- `git_commit`
- `git_tag`

### Branch-local history normalization

- `git_amend_commit`
- `git_reword_commits`
- `git_squash_commits`

`git_reorder_commits` is intentionally deferred from the first normalization release.

### Remote

- `git_fetch`
- `git_pull` (fast-forward only)
- `git_push` (never force)

## History normalization model

History normalization is designed for short-lived development branches used by `dev-git-control` and trunk-based development.

The API does **not** expose interactive rebase. Instead, reword/squash operations:

1. resolve the current named branch and reject protected `main`;
2. validate the supplied base and determine its merge-base with `HEAD`;
3. enumerate commits reachable from `HEAD` but not from the base;
4. prove the selected commits are branch-local;
5. require a clean worktree/index for suffix replay;
6. reject unsupported merge-aware replay;
7. inspect the configured upstream, when present;
8. block rewrites of upstream-reachable commits under the default `local_only` policy;
9. create replacement commit objects with controlled `git commit-tree` semantics;
10. preserve author/committer metadata for replayed commits;
11. verify that the replacement `HEAD^{tree}` is identical to the original `HEAD^{tree}`;
12. move the checked-out branch ref once using compare-and-swap `git update-ref <ref> <new> <old>` semantics.

No worktree-destructive reset is used, no untracked files are deleted, and no rebase/cherry-pick state is created.

If replacement-object creation fails before the ref swap, the branch remains at the original HEAD. If the compare-and-swap fails because HEAD changed concurrently, the operation reports failure rather than overwriting the newer branch state.

### Rewrite policy

The policy enum is:

```text
local_only       # default
allow_published  # explicit opt-in; still never pushes
```

`local_only` blocks a rewrite if any commit whose ID would be replaced is already reachable from the configured upstream.

This distinction matters for a branch that has an upstream but also has newer unpublished local commits: rewriting only an unpublished suffix is permitted, while rewriting the published prefix is blocked.

`git_amend_commit` has fixed local-only behavior and blocks amendment if current HEAD is already reachable from the configured upstream.

## `git_history_plan`

Input:

```json
{
  "repo": "riscvx",
  "base": "main"
}
```

Representative result:

```json
{
  "branch": "feature/reqa-uart-check",
  "base": "main",
  "merge_base": "<sha>",
  "detached_head": false,
  "dirty": false,
  "upstream": null,
  "published": false,
  "commit_count": 11,
  "commits": [
    {
      "sha": "<sha>",
      "subject": "Add isolated UART smoke test",
      "parents": 1,
      "author": "Name <mail@example.invalid>",
      "timestamp": "2026-09-16T10:00:00+07:00",
      "conventional": false
    }
  ],
  "can_rewrite": true,
  "blocking_reasons": []
}
```

`can_rewrite` is a conservative whole-branch signal. A specific reword/squash may still be safe for an unpublished linear suffix even when an older part of the branch is published or contains a merge.

## Conventional Commit validation

The validator recognizes:

```text
feat:
fix:
docs:
test:
refactor:
perf:
build:
ci:
chore:
style:
revert:
```

with optional scope and breaking marker, for example:

```text
feat(nfc): add RATS state machine
fix(git)!: tighten published-history guard
```

Conventional Commits are **not** globally required. Enforcement is opt-in through `git_validate_merge_readiness`.

## `git_validate_merge_readiness`

Example:

```json
{
  "repo": "riscvx",
  "base": "main",
  "max_commits": 5,
  "require_conventional_commits": true,
  "require_clean_tree": true
}
```

The response reports:

- whether the current branch is a named non-`main` branch;
- actual clean-tree state;
- branch-local commit count and configured limit;
- Conventional Commit status;
- machine-readable violations such as `TOO_MANY_COMMITS`, `NON_CONVENTIONAL_COMMIT`, `DIRTY_TREE`, or `DETACHED_HEAD`.

Exactly five commits pass a `max_commits: 5` limit; six do not.

## `git_amend_commit`

Message-only amendment:

```json
{
  "repo": "riscvx",
  "message": "test(fpga): add UART smoke test",
  "include_index": false
}
```

Include the current staged index while keeping the existing message:

```json
{
  "repo": "riscvx",
  "message": null,
  "include_index": true
}
```

The operation:

- acts on HEAD only;
- rejects detached HEAD, `main`, root commits, and merge commits;
- rejects published HEAD;
- does not expose arbitrary amend flags;
- updates the branch ref atomically after creating the replacement commit.

## `git_reword_commits`

```json
{
  "repo": "riscvx",
  "base": "main",
  "changes": [
    {
      "commit": "<sha>",
      "message": "test(fpga): add UART smoke test"
    }
  ],
  "rewrite_policy": "local_only"
}
```

The earliest changed commit and all of its descendants on the branch are replayed because changing a commit ID necessarily changes descendant parent IDs. The result therefore includes an old-to-new SHA mapping for the complete rewritten suffix, with `message_changed` identifying the commits whose messages were explicitly changed.

The final tree must match the original HEAD tree before the branch ref is changed.

## `git_squash_commits`

```json
{
  "repo": "riscvx",
  "base": "main",
  "commits": [
    "<oldest-sha>",
    "<next-sha>",
    "<newest-selected-sha>"
  ],
  "message": "test(fpga): add isolated UART verification",
  "rewrite_policy": "local_only"
}
```

Constraints:

- at least two and at most 128 selected commits;
- supplied order must be oldest to newest;
- the selection must be one contiguous branch-local range;
- the rewritten suffix must not contain unsupported merge/root commits;
- default policy rejects upstream-reachable rewritten commits;
- the new squash commit uses the final selected commit's tree and the first selected commit's parent;
- descendants after the selected range are replayed;
- final HEAD tree must exactly match the original HEAD tree.

## Failure semantics

Mutation failures return structured information similar to:

```json
{
  "operation": "squash_commits",
  "status": "failed",
  "restored": true,
  "original_head": "<sha>",
  "current_head": "<sha>",
  "error": "..."
}
```

The implementation does not use interactive rebase or cherry-pick, so normal failures do not leave `.git/rebase-*` or cherry-pick in-progress state. `repository_in_progress_state` is reserved for the exceptional case where the observed HEAD no longer equals the recorded original HEAD after a failed attempt.

## Branch merge and release workflow

A `dev-git-control` flow can remain completely structured:

```text
git_history_plan(repo, base=main)
git_validate_merge_readiness(repo, base=main, max_commits=5, ...)

# normalize only when needed
git_squash_commits(...)
git_reword_commits(...)

# project/filesystem tooling updates version + CHANGELOG
git_stage(...)
git_commit("chore(release): prepare vX.Y.Z")

git_validate_merge_readiness(...)

git_switch(branch=main)
git_merge(source=<work-branch>, mode=no_ff)

# project-native post-merge verification happens outside this MCP
git_tag(name=vX.Y.Z)
git_delete_branch(<work-branch>)
```

The Git MCP intentionally does **not** edit `CHANGELOG.md`, Cargo/package versions, or project metadata itself.

### Realistic normalization example

```text
feature/reqa-uart-check
11 commits relative to main
    -> git_history_plan
    -> git_squash_commits / git_reword_commits
    -> <= 4 meaningful implementation commits
    -> filesystem/project tooling updates release metadata
    -> git_commit "chore(release): prepare vX.Y.Z"
    -> git_validate_merge_readiness
    -> git_switch main
    -> git_merge --no-ff
    -> external quality/post-merge checks
    -> git_tag vX.Y.Z
    -> git_delete_branch feature/reqa-uart-check
```

History normalization never force-pushes. If published history must be rewritten, `allow_published` only permits the local ref rewrite; any subsequent remote reconciliation remains a separate explicit workflow and force push is still unavailable through this MCP.

## Backward compatibility

Existing action names and schemas remain available:

- repository discovery/status/log/diff/show;
- stage/unstage/restore;
- commit/tag;
- switch/create branch;
- no-fast-forward merge;
- safe merged-branch deletion;
- fetch/pull/push under existing remote-policy controls.

The history-normalization functionality is additive.

## Build and run

```bash
cd mcp-server/git
cargo build
cargo build --release --locked
```

Run against the workspace:

```bash
cargo run -- --root /Users/xiivth/workspaces/signs
```

Read-only mode:

```bash
cargo run -- --root /Users/xiivth/workspaces/signs --read-only
```

Enable remote operations:

```bash
cargo run -- --root /Users/xiivth/workspaces/signs --allow-remote
```

## Suggested Aira configuration

```json
{
  "command": "/Users/xiivth/workspaces/signs/mcp-server/git/target/release/rust-mcp-git",
  "args": [
    "--root",
    "/Users/xiivth/workspaces/signs"
  ]
}
```

After changing source or tool schemas, rebuild the release binary and reload the Aira gateway.

## Development verification

Required gates:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --all-targets --all-features
cargo audit
cargo build --release --locked
```

Do not suppress Clippy warnings merely to make the gate pass; fix the code unless a suppression is technically justified and documented.
