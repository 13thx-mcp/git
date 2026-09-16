use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::Command as StdCommand,
};

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::{
    ErrorData as McpError, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
    transport::stdio,
};
use serde::Serialize;
use tokio::process::Command;

const MAX_COMMIT_MESSAGE_BYTES: usize = 16_384;
const MAX_HISTORY_COMMITS: usize = 512;
const DEFAULT_MAX_MERGE_COMMITS: u32 = 5;

#[derive(Debug, Parser)]
#[command(version, about = "Workspace-confined multi-repository Git MCP server")]
struct Cli {
    #[arg(long, env = "MCP_GIT_ROOT")]
    root: PathBuf,

    #[arg(long, env = "MCP_GIT_READ_ONLY", default_value_t = false)]
    read_only: bool,

    #[arg(long, env = "MCP_GIT_ALLOW_REMOTE", default_value_t = false)]
    allow_remote: bool,

    #[arg(long, env = "MCP_GIT_MAX_OUTPUT_BYTES", default_value_t = 1_048_576)]
    max_output_bytes: usize,
}

#[derive(Debug, Clone)]
struct GitServer {
    workspace_root: PathBuf,
    read_only: bool,
    allow_remote: bool,
    max_output_bytes: usize,
}

#[derive(Debug, Serialize)]
struct GitOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct GitMutationOutput {
    operation: &'static str,
    branch: String,
    head: String,
    status: String,
    stdout: String,
    stderr: String,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct RepositoryList {
    repositories: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RepoArgs {
    /// Repository path relative to the configured workspace root.
    repo: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DiffArgs {
    repo: String,
    #[serde(default)]
    staged: bool,
    path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct LogArgs {
    repo: String,
    max_count: Option<u16>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ShowArgs {
    repo: String,
    revision: String,
    #[serde(default)]
    stat_only: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PathsArgs {
    repo: String,
    paths: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CommitArgs {
    repo: String,
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TagArgs {
    repo: String,
    name: String,
    message: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RemoteArgs {
    repo: String,
    remote: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PullArgs {
    repo: String,
    remote: Option<String>,
    branch: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PushArgs {
    repo: String,
    remote: Option<String>,
    refspec: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SwitchArgs {
    repo: String,
    branch: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CreateBranchArgs {
    repo: String,
    branch: String,
    start_point: Option<String>,
    #[serde(default)]
    switch: bool,
}

#[derive(Debug, Clone, Copy, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum MergeMode {
    /// Always create a merge commit, even when a fast-forward is possible.
    #[default]
    NoFf,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MergeArgs {
    repo: String,
    source: String,
    /// Merge policy. Only `no_ff` is supported.
    #[serde(default)]
    mode: MergeMode,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DeleteBranchArgs {
    repo: String,
    branch: String,
}

#[derive(Debug, Clone, Copy, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RewritePolicy {
    /// Only rewrite commits that are not known to be reachable from the configured upstream.
    #[default]
    LocalOnly,
    /// Permit rewriting commits reachable from the configured upstream. This never force-pushes.
    AllowPublished,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct HistoryPlanArgs {
    repo: String,
    base: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ValidateMergeReadinessArgs {
    repo: String,
    base: String,
    max_commits: Option<u32>,
    #[serde(default)]
    require_conventional_commits: bool,
    #[serde(default)]
    require_clean_tree: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AmendCommitArgs {
    repo: String,
    message: Option<String>,
    #[serde(default)]
    include_index: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RewordCommit {
    commit: String,
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RewordCommitsArgs {
    repo: String,
    base: String,
    changes: Vec<RewordCommit>,
    #[serde(default)]
    rewrite_policy: RewritePolicy,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SquashCommitsArgs {
    repo: String,
    base: String,
    commits: Vec<String>,
    message: String,
    #[serde(default)]
    rewrite_policy: RewritePolicy,
}

#[derive(Debug, Clone, Serialize)]
struct HistoryCommit {
    sha: String,
    subject: String,
    parents: usize,
    author: String,
    timestamp: String,
    conventional: bool,
}

#[derive(Debug, Clone, Serialize)]
struct HistoryPlan {
    branch: String,
    base: String,
    merge_base: String,
    detached_head: bool,
    dirty: bool,
    upstream: Option<String>,
    published: bool,
    commit_count: usize,
    commits: Vec<HistoryCommit>,
    can_rewrite: bool,
    blocking_reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ReadinessChecks {
    not_on_main: bool,
    clean_tree: bool,
    commit_limit: bool,
    conventional_commits: bool,
}

#[derive(Debug, Serialize)]
struct ReadinessViolation {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
}

#[derive(Debug, Serialize)]
struct MergeReadiness {
    ready: bool,
    branch: String,
    base: String,
    commit_count: usize,
    max_allowed: u32,
    checks: ReadinessChecks,
    violations: Vec<ReadinessViolation>,
}

#[derive(Debug, Serialize)]
struct AmendCommitOutput {
    operation: &'static str,
    branch: String,
    old_head: String,
    new_head: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct RewrittenCommit {
    old: String,
    new: String,
    old_subject: String,
    new_subject: String,
    message_changed: bool,
}

#[derive(Debug, Serialize)]
struct RewordCommitsOutput {
    operation: &'static str,
    branch: String,
    old_head: String,
    new_head: String,
    rewritten: Vec<RewrittenCommit>,
    status: String,
}

#[derive(Debug, Serialize)]
struct SquashCommitsOutput {
    operation: &'static str,
    branch: String,
    old_head: String,
    new_head: String,
    squashed_count: usize,
    new_commit: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct RewriteFailure {
    operation: &'static str,
    status: &'static str,
    restored: bool,
    original_head: String,
    current_head: String,
    error: String,
}

#[derive(Debug, Clone)]
struct CommitSnapshot {
    sha: String,
    tree: String,
    parents: Vec<String>,
    author_name: String,
    author_email: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_date: String,
    message: String,
}

impl CommitSnapshot {
    fn subject(&self) -> String {
        self.message.lines().next().unwrap_or_default().to_owned()
    }
}

impl GitServer {
    fn new(cli: &Cli) -> Result<Self> {
        let workspace_root = std::fs::canonicalize(&cli.root)
            .with_context(|| format!("cannot resolve workspace root: {}", cli.root.display()))?;
        anyhow::ensure!(workspace_root.is_dir(), "workspace root is not a directory");
        Ok(Self {
            workspace_root,
            read_only: cli.read_only,
            allow_remote: cli.allow_remote,
            max_output_bytes: cli.max_output_bytes.max(1),
        })
    }

    fn success_json<T: Serialize>(&self, value: &T) -> CallToolResult {
        let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned());
        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    fn error_json<T: Serialize>(&self, value: &T) -> CallToolResult {
        let text = serde_json::to_string_pretty(value)
            .unwrap_or_else(|_| "{\"status\":\"failed\"}".to_owned());
        CallToolResult::error(vec![ContentBlock::text(text)])
    }

    fn success(&self, output: GitOutput) -> CallToolResult {
        self.success_json(&output)
    }

    fn failure(message: impl Into<String>) -> CallToolResult {
        CallToolResult::error(vec![ContentBlock::text(message.into())])
    }

    fn ensure_writable(&self) -> std::result::Result<(), String> {
        if self.read_only {
            Err(
                "Git MCP is running in read-only mode; local repository mutation is disabled"
                    .into(),
            )
        } else {
            Ok(())
        }
    }

    fn ensure_remote(&self) -> std::result::Result<(), String> {
        if self.allow_remote {
            Ok(())
        } else {
            Err(
                "remote Git operations are disabled; start with --allow-remote to enable them"
                    .into(),
            )
        }
    }

    fn resolve_repo(&self, repo: &str) -> std::result::Result<PathBuf, String> {
        let relative = clean_repo_path(repo)?;
        let requested = self.workspace_root.join(relative);
        let resolved = std::fs::canonicalize(&requested)
            .map_err(|error| format!("cannot resolve repository `{repo}`: {error}"))?;
        if !resolved.is_dir() {
            return Err(format!("repository target is not a directory: `{repo}`"));
        }
        if !resolved.starts_with(&self.workspace_root) {
            return Err(format!(
                "repository resolves outside the configured workspace root: `{repo}`"
            ));
        }

        let output = StdCommand::new("git")
            .arg("-C")
            .arg(&resolved)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "rev-parse",
                "--show-toplevel",
            ])
            .output()
            .map_err(|error| format!("failed to execute `git`: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "target is not a Git repository: `{repo}`{}",
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                }
            ));
        }

        let reported = String::from_utf8(output.stdout)
            .map_err(|_| "Git returned a non-UTF-8 repository path".to_owned())?;
        let reported = PathBuf::from(reported.trim());
        let repo_root = std::fs::canonicalize(&reported).map_err(|error| {
            format!(
                "cannot canonicalize Git repository root `{}`: {error}",
                reported.display()
            )
        })?;
        if !repo_root.starts_with(&self.workspace_root) {
            return Err(format!(
                "Git repository root resolves outside the configured workspace: `{repo}`"
            ));
        }
        if repo_root != resolved {
            return Err(format!(
                "repository selector must name the Git repository root, not a subdirectory: `{repo}`"
            ));
        }
        Ok(repo_root)
    }

    fn list_repositories(&self) -> std::result::Result<Vec<String>, String> {
        let mut repositories = Vec::new();
        self.discover_repositories(&self.workspace_root, &mut repositories)?;
        repositories.sort();
        repositories.dedup();
        Ok(repositories)
    }

    fn discover_repositories(
        &self,
        directory: &Path,
        repositories: &mut Vec<String>,
    ) -> std::result::Result<(), String> {
        if directory.join(".git").exists() {
            let relative = directory
                .strip_prefix(&self.workspace_root)
                .map_err(|_| "repository discovery escaped workspace root".to_owned())?;
            let selector = if relative.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                relative.to_string_lossy().into_owned()
            };
            if self.resolve_repo(&selector).is_ok() {
                repositories.push(selector);
                return Ok(());
            }
        }

        for entry in std::fs::read_dir(directory)
            .map_err(|error| format!("cannot read workspace directory: {error}"))?
        {
            let entry = entry.map_err(|error| format!("cannot read workspace entry: {error}"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect workspace entry: {error}"))?;
            if file_type.is_symlink() || !file_type.is_dir() || entry.file_name() == ".git" {
                continue;
            }
            self.discover_repositories(&entry.path(), repositories)?;
        }
        Ok(())
    }

    async fn run_git_raw<I, S>(
        &self,
        repo: &str,
        args: I,
        extra_env: &[(String, String)],
    ) -> std::result::Result<GitOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let repo_root = self.resolve_repo(repo)?;
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env("GIT_EDITOR", "true")
            .envs(extra_env.iter().map(|(key, value)| (key, value)))
            .output()
            .await
            .map_err(|error| format!("failed to execute git: {error}"))?;

        let mut truncated = false;
        Ok(GitOutput {
            exit_code: output.status.code(),
            stdout: bounded_utf8(output.stdout, self.max_output_bytes, &mut truncated),
            stderr: bounded_utf8(output.stderr, self.max_output_bytes, &mut truncated),
            truncated,
        })
    }

    async fn run_git<I, S>(&self, repo: &str, args: I) -> std::result::Result<GitOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run_git_raw(repo, args, &[]).await?;
        if output.exit_code == Some(0) {
            Ok(output)
        } else {
            Err(format_git_failure(&output))
        }
    }

    async fn run_git_env<I, S>(
        &self,
        repo: &str,
        args: I,
        extra_env: &[(String, String)],
    ) -> std::result::Result<GitOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run_git_raw(repo, args, extra_env).await?;
        if output.exit_code == Some(0) {
            Ok(output)
        } else {
            Err(format_git_failure(&output))
        }
    }

    async fn mutation_output(
        &self,
        operation: &'static str,
        repo: &str,
        output: GitOutput,
    ) -> std::result::Result<GitMutationOutput, String> {
        let branch = self.current_branch_required(repo).await?;
        let head = self.head_sha(repo).await?;
        let status = self.short_status(repo).await?;
        Ok(GitMutationOutput {
            operation,
            branch,
            head,
            status,
            stdout: output.stdout,
            stderr: output.stderr,
            truncated: output.truncated,
        })
    }

    async fn validate_tag_name(&self, repo: &str, name: &str) -> std::result::Result<(), String> {
        validate_token("tag name", name)?;
        let reference = format!("refs/tags/{name}");
        self.run_git(repo, ["check-ref-format", reference.as_str()])
            .await?;
        Ok(())
    }

    async fn validate_branch_name(
        &self,
        repo: &str,
        branch: &str,
    ) -> std::result::Result<(), String> {
        validate_token("branch", branch)?;
        self.run_git(repo, ["check-ref-format", "--branch", branch])
            .await?;
        Ok(())
    }

    async fn ensure_local_branch(
        &self,
        repo: &str,
        branch: &str,
    ) -> std::result::Result<(), String> {
        self.validate_branch_name(repo, branch).await?;
        let reference = format!("refs/heads/{branch}");
        self.run_git(
            repo,
            ["show-ref", "--verify", "--quiet", reference.as_str()],
        )
        .await
        .map(|_| ())
        .map_err(|_| format!("local branch does not exist: `{branch}`"))
    }

    async fn validate_commitish(
        &self,
        repo: &str,
        value: &str,
        kind: &str,
    ) -> std::result::Result<(), String> {
        validate_revision(value)?;
        let commitish = format!("{value}^{{commit}}");
        self.run_git(
            repo,
            [
                "rev-parse",
                "--verify",
                "--end-of-options",
                commitish.as_str(),
            ],
        )
        .await
        .map(|_| ())
        .map_err(|_| format!("{kind} does not resolve to a commit: `{value}`"))
    }

    async fn resolve_commit_sha(
        &self,
        repo: &str,
        value: &str,
    ) -> std::result::Result<String, String> {
        self.validate_commitish(repo, value, "commit").await?;
        let commitish = format!("{value}^{{commit}}");
        Ok(self
            .run_git(
                repo,
                [
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    commitish.as_str(),
                ],
            )
            .await?
            .stdout
            .trim()
            .to_owned())
    }

    async fn current_branch(&self, repo: &str) -> std::result::Result<Option<String>, String> {
        let output = self
            .run_git_raw(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"], &[])
            .await?;
        match output.exit_code {
            Some(0) => Ok(Some(output.stdout.trim().to_owned())),
            Some(1) => Ok(None),
            _ => Err(format_git_failure(&output)),
        }
    }

    async fn current_branch_required(&self, repo: &str) -> std::result::Result<String, String> {
        self.current_branch(repo)
            .await?
            .ok_or_else(|| "detached HEAD is not allowed for this operation".to_owned())
    }

    async fn head_sha(&self, repo: &str) -> std::result::Result<String, String> {
        Ok(self
            .run_git(repo, ["rev-parse", "--verify", "HEAD"])
            .await?
            .stdout
            .trim()
            .to_owned())
    }

    async fn tree_sha(&self, repo: &str, revision: &str) -> std::result::Result<String, String> {
        let tree = format!("{revision}^{{tree}}");
        Ok(self
            .run_git(
                repo,
                ["rev-parse", "--verify", "--end-of-options", tree.as_str()],
            )
            .await?
            .stdout
            .trim()
            .to_owned())
    }

    async fn short_status(&self, repo: &str) -> std::result::Result<String, String> {
        Ok(self
            .run_git(
                repo,
                ["status", "--short", "--branch", "--untracked-files=all"],
            )
            .await?
            .stdout)
    }

    async fn is_dirty(&self, repo: &str) -> std::result::Result<bool, String> {
        Ok(!self
            .run_git(repo, ["status", "--porcelain=v1", "--untracked-files=all"])
            .await?
            .stdout
            .is_empty())
    }

    async fn merge_base(&self, repo: &str, base: &str) -> std::result::Result<String, String> {
        self.validate_commitish(repo, base, "base").await?;
        Ok(self
            .run_git(repo, ["merge-base", "HEAD", base])
            .await?
            .stdout
            .trim()
            .to_owned())
    }

    async fn upstream(&self, repo: &str) -> std::result::Result<Option<String>, String> {
        let output = self
            .run_git_raw(
                repo,
                [
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                &[],
            )
            .await?;
        match output.exit_code {
            Some(0) => Ok(Some(output.stdout.trim().to_owned())),
            Some(128) | Some(1) => Ok(None),
            _ => Err(format_git_failure(&output)),
        }
    }

    async fn is_ancestor(
        &self,
        repo: &str,
        ancestor: &str,
        descendant: &str,
    ) -> std::result::Result<bool, String> {
        let output = self
            .run_git_raw(
                repo,
                ["merge-base", "--is-ancestor", ancestor, descendant],
                &[],
            )
            .await?;
        match output.exit_code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format_git_failure(&output)),
        }
    }

    async fn branch_history(
        &self,
        repo: &str,
        base: &str,
    ) -> std::result::Result<Vec<HistoryCommit>, String> {
        self.validate_commitish(repo, base, "base").await?;
        let range = format!("{base}..HEAD");
        let output = self
            .run_git(
                repo,
                [
                    "log",
                    "--reverse",
                    "--topo-order",
                    "--date=iso-strict",
                    "--pretty=format:%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%s",
                    range.as_str(),
                ],
            )
            .await?;
        if output.truncated {
            return Err("history output exceeded configured Git MCP output limit".into());
        }
        if output.stdout.is_empty() {
            return Ok(Vec::new());
        }
        let mut commits = Vec::new();
        for line in output.stdout.lines() {
            let fields: Vec<_> = line.split('\x1f').collect();
            if fields.len() != 6 {
                return Err("unexpected git log output while building history plan".into());
            }
            let parent_count = if fields[1].is_empty() {
                0
            } else {
                fields[1].split_whitespace().count()
            };
            commits.push(HistoryCommit {
                sha: fields[0].to_owned(),
                subject: fields[5].to_owned(),
                parents: parent_count,
                author: format!("{} <{}>", fields[2], fields[3]),
                timestamp: fields[4].to_owned(),
                conventional: is_conventional_commit_subject(fields[5]),
            });
            if commits.len() > MAX_HISTORY_COMMITS {
                return Err(format!(
                    "branch-local history exceeds safety limit of {MAX_HISTORY_COMMITS} commits"
                ));
            }
        }
        Ok(commits)
    }

    async fn history_plan(
        &self,
        repo: &str,
        base: &str,
    ) -> std::result::Result<HistoryPlan, String> {
        validate_revision(base)?;
        let branch = self.current_branch(repo).await?;
        let detached_head = branch.is_none();
        let branch_name = branch.clone().unwrap_or_else(|| "HEAD".to_owned());
        let merge_base = self.merge_base(repo, base).await?;
        let commits = self.branch_history(repo, base).await?;
        let dirty = self.is_dirty(repo).await?;
        let upstream = if detached_head {
            None
        } else {
            self.upstream(repo).await?
        };
        let mut published = false;
        if let Some(upstream_ref) = upstream.as_deref() {
            for commit in &commits {
                if self.is_ancestor(repo, &commit.sha, upstream_ref).await? {
                    published = true;
                    break;
                }
            }
        }

        let mut blocking_reasons = Vec::new();
        if detached_head {
            blocking_reasons.push("detached_head".to_owned());
        }
        if branch.as_deref() == Some("main") {
            blocking_reasons.push("protected_branch_main".to_owned());
        }
        if dirty {
            blocking_reasons.push("dirty_worktree_or_index".to_owned());
        }
        if commits.iter().any(|commit| commit.parents > 1) {
            blocking_reasons.push("merge_commit_present".to_owned());
        }
        if published {
            blocking_reasons.push("published_history".to_owned());
        }

        Ok(HistoryPlan {
            branch: branch_name,
            base: base.to_owned(),
            merge_base,
            detached_head,
            dirty,
            upstream,
            published,
            commit_count: commits.len(),
            commits,
            can_rewrite: blocking_reasons.is_empty(),
            blocking_reasons,
        })
    }

    async fn validate_merge_readiness(
        &self,
        args: &ValidateMergeReadinessArgs,
    ) -> std::result::Result<MergeReadiness, String> {
        let plan = self.history_plan(&args.repo, &args.base).await?;
        let max_allowed = args.max_commits.unwrap_or(DEFAULT_MAX_MERGE_COMMITS).max(1);
        let not_on_main = !plan.detached_head && plan.branch != "main";
        let clean_tree = !plan.dirty;
        let commit_limit = plan.commit_count <= max_allowed as usize;
        let invalid_commits: Vec<_> = plan
            .commits
            .iter()
            .filter(|commit| !commit.conventional)
            .collect();
        let conventional_commits = invalid_commits.is_empty();

        let mut violations = Vec::new();
        if !not_on_main {
            violations.push(ReadinessViolation {
                code: if plan.detached_head {
                    "DETACHED_HEAD"
                } else {
                    "PROTECTED_BRANCH"
                },
                message: if plan.detached_head {
                    "merge readiness requires a named work branch".to_owned()
                } else {
                    "merge readiness cannot be evaluated from protected branch `main`".to_owned()
                },
                commit: None,
                subject: None,
            });
        }
        if args.require_clean_tree && !clean_tree {
            violations.push(ReadinessViolation {
                code: "DIRTY_TREE",
                message: "working tree or index is not clean".to_owned(),
                commit: None,
                subject: None,
            });
        }
        if !commit_limit {
            violations.push(ReadinessViolation {
                code: "TOO_MANY_COMMITS",
                message: format!(
                    "{} branch commits; maximum is {max_allowed}",
                    plan.commit_count
                ),
                commit: None,
                subject: None,
            });
        }
        if args.require_conventional_commits {
            for commit in invalid_commits {
                violations.push(ReadinessViolation {
                    code: "NON_CONVENTIONAL_COMMIT",
                    message: "commit subject does not follow Conventional Commits".to_owned(),
                    commit: Some(commit.sha.clone()),
                    subject: Some(commit.subject.clone()),
                });
            }
        }

        Ok(MergeReadiness {
            ready: not_on_main
                && (!args.require_clean_tree || clean_tree)
                && commit_limit
                && (!args.require_conventional_commits || conventional_commits),
            branch: plan.branch,
            base: plan.base,
            commit_count: plan.commit_count,
            max_allowed,
            checks: ReadinessChecks {
                not_on_main,
                clean_tree,
                commit_limit,
                conventional_commits,
            },
            violations,
        })
    }

    async fn commit_snapshot(
        &self,
        repo: &str,
        commit: &str,
    ) -> std::result::Result<CommitSnapshot, String> {
        let sha = self.resolve_commit_sha(repo, commit).await?;
        let output = self
            .run_git(
                repo,
                [
                    "show",
                    "-s",
                    "--no-patch",
                    "--format=%T%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%B",
                    "--end-of-options",
                    sha.as_str(),
                ],
            )
            .await?;
        if output.truncated {
            return Err("commit metadata exceeded configured Git MCP output limit".into());
        }
        let mut fields = output.stdout.splitn(9, '\x1f');
        let tree = fields
            .next()
            .ok_or_else(|| "missing commit tree".to_owned())?
            .to_owned();
        let parents_raw = fields
            .next()
            .ok_or_else(|| "missing commit parents".to_owned())?;
        let author_name = fields
            .next()
            .ok_or_else(|| "missing author name".to_owned())?
            .to_owned();
        let author_email = fields
            .next()
            .ok_or_else(|| "missing author email".to_owned())?
            .to_owned();
        let author_date = fields
            .next()
            .ok_or_else(|| "missing author date".to_owned())?
            .to_owned();
        let committer_name = fields
            .next()
            .ok_or_else(|| "missing committer name".to_owned())?
            .to_owned();
        let committer_email = fields
            .next()
            .ok_or_else(|| "missing committer email".to_owned())?
            .to_owned();
        let committer_date = fields
            .next()
            .ok_or_else(|| "missing committer date".to_owned())?
            .to_owned();
        let message = fields
            .next()
            .ok_or_else(|| "missing commit message".to_owned())?
            .trim_end_matches('\n')
            .to_owned();
        Ok(CommitSnapshot {
            sha,
            tree,
            parents: if parents_raw.is_empty() {
                Vec::new()
            } else {
                parents_raw.split_whitespace().map(str::to_owned).collect()
            },
            author_name,
            author_email,
            author_date,
            committer_name,
            committer_email,
            committer_date,
            message,
        })
    }

    async fn create_commit_from_snapshot(
        &self,
        repo: &str,
        snapshot: &CommitSnapshot,
        tree: &str,
        parent: &str,
        message: &str,
    ) -> std::result::Result<String, String> {
        validate_commit_message(message)?;
        validate_object_id(tree)?;
        validate_object_id(parent)?;
        let env = vec![
            ("GIT_AUTHOR_NAME".to_owned(), snapshot.author_name.clone()),
            ("GIT_AUTHOR_EMAIL".to_owned(), snapshot.author_email.clone()),
            ("GIT_AUTHOR_DATE".to_owned(), snapshot.author_date.clone()),
            (
                "GIT_COMMITTER_NAME".to_owned(),
                snapshot.committer_name.clone(),
            ),
            (
                "GIT_COMMITTER_EMAIL".to_owned(),
                snapshot.committer_email.clone(),
            ),
            (
                "GIT_COMMITTER_DATE".to_owned(),
                snapshot.committer_date.clone(),
            ),
        ];
        let output = self
            .run_git_env(
                repo,
                ["commit-tree", tree, "-p", parent, "-m", message],
                &env,
            )
            .await?;
        let sha = output.stdout.trim().to_owned();
        validate_object_id(&sha)?;
        Ok(sha)
    }

    async fn update_current_branch_ref(
        &self,
        repo: &str,
        branch: &str,
        new_head: &str,
        old_head: &str,
    ) -> std::result::Result<(), String> {
        let reference = format!("refs/heads/{branch}");
        self.run_git(repo, ["update-ref", reference.as_str(), new_head, old_head])
            .await?;
        Ok(())
    }

    async fn ensure_suffix_rewrite_safe(
        &self,
        repo: &str,
        plan: &HistoryPlan,
        start_index: usize,
        policy: RewritePolicy,
    ) -> std::result::Result<(), String> {
        if plan.detached_head {
            return Err("history rewriting is not allowed in detached HEAD state".into());
        }
        if plan.branch == "main" {
            return Err("history rewriting is not allowed on protected branch `main`".into());
        }
        if plan.dirty {
            return Err("history rewriting requires a clean working tree and index".into());
        }
        if start_index >= plan.commits.len() {
            return Err("rewrite selection is outside branch-local history".into());
        }
        if plan.commits[start_index..]
            .iter()
            .any(|commit| commit.parents != 1)
        {
            return Err(
                "rewrite suffix contains a root or merge commit; merge-aware rewriting is not supported"
                    .into(),
            );
        }
        if matches!(policy, RewritePolicy::LocalOnly)
            && let Some(upstream) = plan.upstream.as_deref()
        {
            for commit in &plan.commits[start_index..] {
                if self.is_ancestor(repo, &commit.sha, upstream).await? {
                    return Err(format!(
                        "rewrite blocked: commit {} is already reachable from upstream `{upstream}`",
                        commit.sha
                    ));
                }
            }
        }
        Ok(())
    }

    async fn rewrite_failure(
        &self,
        operation: &'static str,
        repo: &str,
        original_head: &str,
        error: String,
    ) -> RewriteFailure {
        let current_head = self
            .head_sha(repo)
            .await
            .unwrap_or_else(|_| "unknown".to_owned());
        RewriteFailure {
            operation,
            status: if current_head == original_head {
                "failed"
            } else {
                "repository_in_progress_state"
            },
            restored: current_head == original_head,
            original_head: original_head.to_owned(),
            current_head,
            error,
        }
    }

    async fn amend_commit(
        &self,
        args: &AmendCommitArgs,
    ) -> std::result::Result<AmendCommitOutput, RewriteFailure> {
        if let Err(error) = self.ensure_writable() {
            return Err(RewriteFailure {
                operation: "amend_commit",
                status: "failed",
                restored: true,
                original_head: "unknown".to_owned(),
                current_head: "unknown".to_owned(),
                error,
            });
        }
        let original_head = match self.head_sha(&args.repo).await {
            Ok(head) => head,
            Err(error) => {
                return Err(RewriteFailure {
                    operation: "amend_commit",
                    status: "failed",
                    restored: true,
                    original_head: "unknown".to_owned(),
                    current_head: "unknown".to_owned(),
                    error,
                });
            }
        };
        let result = async {
            let branch = self.current_branch_required(&args.repo).await?;
            if branch == "main" {
                return Err("amend is not allowed on protected branch `main`".to_owned());
            }
            let snapshot = self.commit_snapshot(&args.repo, &original_head).await?;
            if snapshot.parents.len() != 1 {
                return Err("amend rejects root and merge commits".to_owned());
            }
            if let Some(message) = args.message.as_deref() {
                validate_commit_message(message)?;
            }
            if args.message.is_none() && !args.include_index {
                return Err("amend requires a new message or include_index=true".to_owned());
            }
            if let Some(upstream) = self.upstream(&args.repo).await?
                && self
                    .is_ancestor(&args.repo, &original_head, &upstream)
                    .await?
            {
                return Err(format!(
                    "amend blocked: HEAD is already reachable from upstream `{upstream}`"
                ));
            }
            let tree = if args.include_index {
                self.run_git(&args.repo, ["write-tree"])
                    .await?
                    .stdout
                    .trim()
                    .to_owned()
            } else {
                snapshot.tree.clone()
            };
            let message = args.message.as_deref().unwrap_or(&snapshot.message);
            let new_head = self
                .create_commit_from_snapshot(
                    &args.repo,
                    &snapshot,
                    &tree,
                    &snapshot.parents[0],
                    message,
                )
                .await?;
            if new_head == original_head {
                return Err("amend produced no commit change".to_owned());
            }
            self.update_current_branch_ref(&args.repo, &branch, &new_head, &original_head)
                .await?;
            Ok(AmendCommitOutput {
                operation: "amend_commit",
                branch,
                old_head: original_head.clone(),
                new_head,
                status: self.short_status(&args.repo).await?,
            })
        }
        .await;
        match result {
            Ok(output) => Ok(output),
            Err(error) => Err(self
                .rewrite_failure("amend_commit", &args.repo, &original_head, error)
                .await),
        }
    }

    async fn reword_commits(
        &self,
        args: &RewordCommitsArgs,
    ) -> std::result::Result<RewordCommitsOutput, RewriteFailure> {
        if let Err(error) = self.ensure_writable() {
            return Err(RewriteFailure {
                operation: "reword_commits",
                status: "failed",
                restored: true,
                original_head: "unknown".to_owned(),
                current_head: "unknown".to_owned(),
                error,
            });
        }
        let original_head = match self.head_sha(&args.repo).await {
            Ok(head) => head,
            Err(error) => {
                return Err(RewriteFailure {
                    operation: "reword_commits",
                    status: "failed",
                    restored: true,
                    original_head: "unknown".to_owned(),
                    current_head: "unknown".to_owned(),
                    error,
                });
            }
        };
        let result = async {
            if args.changes.is_empty() || args.changes.len() > 64 {
                return Err("reword requires 1..64 commit changes".to_owned());
            }
            let plan = self.history_plan(&args.repo, &args.base).await?;
            let mut requested = HashMap::new();
            for change in &args.changes {
                validate_commit_message(&change.message)?;
                let sha = self.resolve_commit_sha(&args.repo, &change.commit).await?;
                if requested.insert(sha, change.message.clone()).is_some() {
                    return Err("duplicate commit in reword request".to_owned());
                }
            }
            let positions: HashMap<_, _> = plan
                .commits
                .iter()
                .enumerate()
                .map(|(index, commit)| (commit.sha.clone(), index))
                .collect();
            let mut earliest = usize::MAX;
            for sha in requested.keys() {
                let index = positions.get(sha).ok_or_else(|| {
                    format!(
                        "commit {sha} is outside branch-local history relative to `{}`",
                        args.base
                    )
                })?;
                earliest = earliest.min(*index);
            }
            self.ensure_suffix_rewrite_safe(&args.repo, &plan, earliest, args.rewrite_policy)
                .await?;

            let original_tree = self.tree_sha(&args.repo, &original_head).await?;
            let first = self
                .commit_snapshot(&args.repo, &plan.commits[earliest].sha)
                .await?;
            let mut parent = first
                .parents
                .first()
                .cloned()
                .ok_or_else(|| "cannot rewrite a root commit".to_owned())?;
            let mut rewritten = Vec::new();
            let mut actually_changed = false;
            for commit in &plan.commits[earliest..] {
                let snapshot = self.commit_snapshot(&args.repo, &commit.sha).await?;
                if snapshot.parents.len() != 1 {
                    return Err("merge-aware reword is not supported".to_owned());
                }
                let new_message = requested
                    .get(&commit.sha)
                    .map(String::as_str)
                    .unwrap_or(&snapshot.message);
                let message_changed = requested
                    .get(&commit.sha)
                    .is_some_and(|message| message != &snapshot.message);
                actually_changed |= message_changed;
                let new_sha = self
                    .create_commit_from_snapshot(
                        &args.repo,
                        &snapshot,
                        &snapshot.tree,
                        &parent,
                        new_message,
                    )
                    .await?;
                rewritten.push(RewrittenCommit {
                    old: snapshot.sha.clone(),
                    new: new_sha.clone(),
                    old_subject: snapshot.subject(),
                    new_subject: new_message.lines().next().unwrap_or_default().to_owned(),
                    message_changed,
                });
                parent = new_sha;
            }
            if !actually_changed {
                return Err("reword request does not change any commit message".to_owned());
            }
            let new_head = parent;
            let new_tree = self.tree_sha(&args.repo, &new_head).await?;
            if new_tree != original_tree {
                return Err("tree-integrity check failed before updating branch ref".to_owned());
            }
            self.update_current_branch_ref(&args.repo, &plan.branch, &new_head, &original_head)
                .await?;
            Ok(RewordCommitsOutput {
                operation: "reword_commits",
                branch: plan.branch,
                old_head: original_head.clone(),
                new_head,
                rewritten,
                status: self.short_status(&args.repo).await?,
            })
        }
        .await;
        match result {
            Ok(output) => Ok(output),
            Err(error) => Err(self
                .rewrite_failure("reword_commits", &args.repo, &original_head, error)
                .await),
        }
    }

    async fn squash_commits(
        &self,
        args: &SquashCommitsArgs,
    ) -> std::result::Result<SquashCommitsOutput, RewriteFailure> {
        if let Err(error) = self.ensure_writable() {
            return Err(RewriteFailure {
                operation: "squash_commits",
                status: "failed",
                restored: true,
                original_head: "unknown".to_owned(),
                current_head: "unknown".to_owned(),
                error,
            });
        }
        let original_head = match self.head_sha(&args.repo).await {
            Ok(head) => head,
            Err(error) => {
                return Err(RewriteFailure {
                    operation: "squash_commits",
                    status: "failed",
                    restored: true,
                    original_head: "unknown".to_owned(),
                    current_head: "unknown".to_owned(),
                    error,
                });
            }
        };
        let result = async {
            if !(2..=128).contains(&args.commits.len()) {
                return Err("squash requires 2..128 commits".to_owned());
            }
            validate_commit_message(&args.message)?;
            let plan = self.history_plan(&args.repo, &args.base).await?;
            let positions: HashMap<_, _> = plan
                .commits
                .iter()
                .enumerate()
                .map(|(index, commit)| (commit.sha.clone(), index))
                .collect();
            let mut resolved = Vec::with_capacity(args.commits.len());
            let mut seen = BTreeSet::new();
            for commit in &args.commits {
                let sha = self.resolve_commit_sha(&args.repo, commit).await?;
                if !seen.insert(sha.clone()) {
                    return Err("duplicate commit in squash request".to_owned());
                }
                resolved.push(sha);
            }
            let indices: Vec<_> = resolved
                .iter()
                .map(|sha| {
                    positions.get(sha).copied().ok_or_else(|| {
                        format!(
                            "commit {sha} is outside branch-local history relative to `{}`",
                            args.base
                        )
                    })
                })
                .collect::<std::result::Result<_, _>>()?;
            let start = indices[0];
            for (offset, index) in indices.iter().enumerate() {
                if *index != start + offset {
                    return Err(
                        "squash commits must be one contiguous oldest-to-newest history range"
                            .to_owned(),
                    );
                }
            }
            let end = *indices.last().expect("validated non-empty indices");
            self.ensure_suffix_rewrite_safe(&args.repo, &plan, start, args.rewrite_policy)
                .await?;

            let first = self
                .commit_snapshot(&args.repo, &plan.commits[start].sha)
                .await?;
            let last = self
                .commit_snapshot(&args.repo, &plan.commits[end].sha)
                .await?;
            let parent = first
                .parents
                .first()
                .cloned()
                .ok_or_else(|| "cannot squash a root commit".to_owned())?;
            let original_tree = self.tree_sha(&args.repo, &original_head).await?;
            let new_commit = self
                .create_commit_from_snapshot(&args.repo, &first, &last.tree, &parent, &args.message)
                .await?;
            let mut new_head = new_commit.clone();
            for commit in &plan.commits[end + 1..] {
                let snapshot = self.commit_snapshot(&args.repo, &commit.sha).await?;
                if snapshot.parents.len() != 1 {
                    return Err("merge-aware squash replay is not supported".to_owned());
                }
                new_head = self
                    .create_commit_from_snapshot(
                        &args.repo,
                        &snapshot,
                        &snapshot.tree,
                        &new_head,
                        &snapshot.message,
                    )
                    .await?;
            }
            let new_tree = self.tree_sha(&args.repo, &new_head).await?;
            if new_tree != original_tree {
                return Err("tree-integrity check failed before updating branch ref".to_owned());
            }
            self.update_current_branch_ref(&args.repo, &plan.branch, &new_head, &original_head)
                .await?;
            Ok(SquashCommitsOutput {
                operation: "squash_commits",
                branch: plan.branch,
                old_head: original_head.clone(),
                new_head,
                squashed_count: resolved.len(),
                new_commit,
                status: self.short_status(&args.repo).await?,
            })
        }
        .await;
        match result {
            Ok(output) => Ok(output),
            Err(error) => Err(self
                .rewrite_failure("squash_commits", &args.repo, &original_head, error)
                .await),
        }
    }

    async fn switch_branch(
        &self,
        repo: &str,
        branch: &str,
    ) -> std::result::Result<GitMutationOutput, String> {
        self.ensure_writable()?;
        self.ensure_local_branch(repo, branch).await?;
        let output = self.run_git(repo, ["switch", "--", branch]).await?;
        self.mutation_output("switch", repo, output).await
    }

    async fn create_branch(
        &self,
        repo: &str,
        branch: &str,
        start_point: Option<&str>,
        switch: bool,
    ) -> std::result::Result<GitMutationOutput, String> {
        self.ensure_writable()?;
        self.validate_branch_name(repo, branch).await?;
        let reference = format!("refs/heads/{branch}");
        if self
            .run_git(
                repo,
                ["show-ref", "--verify", "--quiet", reference.as_str()],
            )
            .await
            .is_ok()
        {
            return Err(format!("local branch already exists: `{branch}`"));
        }
        if let Some(start_point) = start_point {
            self.validate_commitish(repo, start_point, "start point")
                .await?;
        }

        let output = if switch {
            let mut command = vec!["switch".to_owned(), "-c".to_owned(), branch.to_owned()];
            if let Some(start_point) = start_point {
                command.push(start_point.to_owned());
            }
            self.run_git(repo, command).await?
        } else {
            let mut command = vec!["branch".to_owned(), branch.to_owned()];
            if let Some(start_point) = start_point {
                command.push(start_point.to_owned());
            }
            self.run_git(repo, command).await?
        };
        self.mutation_output("create_branch", repo, output).await
    }

    async fn merge_branch(
        &self,
        repo: &str,
        source: &str,
        mode: MergeMode,
    ) -> std::result::Result<GitMutationOutput, String> {
        self.ensure_writable()?;
        self.validate_commitish(repo, source, "merge source")
            .await?;
        let output = match mode {
            MergeMode::NoFf => {
                self.run_git(repo, ["merge", "--no-ff", "--no-edit", "--", source])
                    .await?
            }
        };
        self.mutation_output("merge", repo, output).await
    }

    async fn delete_branch(
        &self,
        repo: &str,
        branch: &str,
    ) -> std::result::Result<GitMutationOutput, String> {
        self.ensure_writable()?;
        self.ensure_local_branch(repo, branch).await?;
        let output = self.run_git(repo, ["branch", "-d", "--", branch]).await?;
        self.mutation_output("delete_branch", repo, output).await
    }
}

#[tool_router(server_handler)]
impl GitServer {
    #[tool(
        description = "List Git repository roots discovered below the configured workspace boundary."
    )]
    async fn git_list_repositories(&self) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.list_repositories() {
            Ok(repositories) => self.success_json(&RepositoryList { repositories }),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Return HEAD/current branch and working-tree status for a selected workspace repository."
    )]
    async fn git_status(
        &self,
        Parameters(args): Parameters<RepoArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(
            match self
                .run_git(
                    &args.repo,
                    ["status", "--short", "--branch", "--untracked-files=all"],
                )
                .await
            {
                Ok(output) => self.success(output),
                Err(error) => Self::failure(error),
            },
        )
    }

    #[tool(
        description = "Inspect branch-local commits relative to a base branch and report rewrite safety without mutating repository state."
    )]
    async fn git_history_plan(
        &self,
        Parameters(args): Parameters<HistoryPlanArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.history_plan(&args.repo, &args.base).await {
            Ok(plan) => self.success_json(&plan),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Read-only merge-readiness checks for branch commit count, Conventional Commit subjects, protected branch state, and optional clean-tree policy."
    )]
    async fn git_validate_merge_readiness(
        &self,
        Parameters(args): Parameters<ValidateMergeReadinessArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.validate_merge_readiness(&args).await {
            Ok(readiness) => self.success_json(&readiness),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Safely amend HEAD on a non-main, unpublished branch by replacing its message and/or including the current index. No force push is performed."
    )]
    async fn git_amend_commit(
        &self,
        Parameters(args): Parameters<AmendCommitArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.amend_commit(&args).await {
            Ok(output) => self.success_json(&output),
            Err(error) => self.error_json(&error),
        })
    }

    #[tool(
        description = "Rewrite messages for branch-local commits using an atomic semantic replay. Final HEAD tree must remain identical."
    )]
    async fn git_reword_commits(
        &self,
        Parameters(args): Parameters<RewordCommitsArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.reword_commits(&args).await {
            Ok(output) => self.success_json(&output),
            Err(error) => self.error_json(&error),
        })
    }

    #[tool(
        description = "Squash one contiguous oldest-to-newest range of branch-local commits into one commit while preserving the final HEAD tree."
    )]
    async fn git_squash_commits(
        &self,
        Parameters(args): Parameters<SquashCommitsArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.squash_commits(&args).await {
            Ok(output) => self.success_json(&output),
            Err(error) => self.error_json(&error),
        })
    }

    #[tool(
        description = "Show unstaged or staged Git diff, optionally limited to one repository-relative path."
    )]
    async fn git_diff(
        &self,
        Parameters(args): Parameters<DiffArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mut command = vec![
            "diff".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-textconv".to_owned(),
        ];
        if args.staged {
            command.push("--cached".into());
        }
        command.push("--".into());
        if let Some(path) = args.path {
            match clean_relative_path(&path) {
                Ok(path) => command.push(path.to_string_lossy().into_owned()),
                Err(error) => return Ok(Self::failure(error)),
            }
        }
        Ok(match self.run_git(&args.repo, command).await {
            Ok(output) => self.success(output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Show recent commit history with commit hash, author, timestamp, and subject."
    )]
    async fn git_log(
        &self,
        Parameters(args): Parameters<LogArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let count = args.max_count.unwrap_or(20).clamp(1, 200).to_string();
        Ok(
            match self
                .run_git(
                    &args.repo,
                    [
                        "log",
                        "--date=iso-strict",
                        "--pretty=format:%H%x09%an%x09%ad%x09%s",
                        "-n",
                        count.as_str(),
                    ],
                )
                .await
            {
                Ok(output) => self.success(output),
                Err(error) => Self::failure(error),
            },
        )
    }

    #[tool(
        description = "Show one revision/commit/tag/branch with metadata and either patch or diffstat."
    )]
    async fn git_show(
        &self,
        Parameters(args): Parameters<ShowArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = validate_revision(&args.revision) {
            return Ok(Self::failure(error));
        }
        let mode = if args.stat_only { "--stat" } else { "--patch" };
        Ok(
            match self
                .run_git(
                    &args.repo,
                    [
                        "show",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--format=fuller",
                        mode,
                        args.revision.as_str(),
                        "--",
                    ],
                )
                .await
            {
                Ok(output) => self.success(output),
                Err(error) => Self::failure(error),
            },
        )
    }

    #[tool(description = "List local branches with current branch marker and full commit hashes.")]
    async fn git_branches(
        &self,
        Parameters(args): Parameters<RepoArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(
            match self
                .run_git(&args.repo, ["branch", "--list", "--verbose", "--no-abbrev"])
                .await
            {
                Ok(output) => self.success(output),
                Err(error) => Self::failure(error),
            },
        )
    }

    #[tool(
        description = "Switch to an existing local branch without discarding working-tree changes."
    )]
    async fn git_switch(
        &self,
        Parameters(args): Parameters<SwitchArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.switch_branch(&args.repo, &args.branch).await {
            Ok(output) => self.success_json(&output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Create a local branch from HEAD or a validated start point, optionally switching to it. Existing branches are never reset."
    )]
    async fn git_create_branch(
        &self,
        Parameters(args): Parameters<CreateBranchArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(
            match self
                .create_branch(
                    &args.repo,
                    &args.branch,
                    args.start_point.as_deref(),
                    args.switch,
                )
                .await
            {
                Ok(output) => self.success_json(&output),
                Err(error) => Self::failure(error),
            },
        )
    }

    #[tool(
        description = "Merge a validated source into the current branch using no-fast-forward semantics. A merge commit is always created when a merge is needed."
    )]
    async fn git_merge(
        &self,
        Parameters(args): Parameters<MergeArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(
            match self.merge_branch(&args.repo, &args.source, args.mode).await {
                Ok(output) => self.success_json(&output),
                Err(error) => Self::failure(error),
            },
        )
    }

    #[tool(
        description = "Safely delete a merged local branch using `git branch -d`. Force deletion is not exposed."
    )]
    async fn git_delete_branch(
        &self,
        Parameters(args): Parameters<DeleteBranchArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.delete_branch(&args.repo, &args.branch).await {
            Ok(output) => self.success_json(&output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(description = "Stage repository-relative paths.")]
    async fn git_stage(
        &self,
        Parameters(args): Parameters<PathsArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_writable() {
            return Ok(Self::failure(error));
        }
        let paths = match clean_paths(&args.paths) {
            Ok(paths) => paths,
            Err(error) => return Ok(Self::failure(error)),
        };
        let mut command = vec!["add".to_owned(), "--".to_owned()];
        command.extend(paths);
        Ok(match self.run_git(&args.repo, command).await {
            Ok(output) => self.success(output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Remove repository-relative paths from the staging area while preserving working-tree content."
    )]
    async fn git_unstage(
        &self,
        Parameters(args): Parameters<PathsArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_writable() {
            return Ok(Self::failure(error));
        }
        let paths = match clean_paths(&args.paths) {
            Ok(paths) => paths,
            Err(error) => return Ok(Self::failure(error)),
        };
        let mut command = vec!["restore".to_owned(), "--staged".to_owned(), "--".to_owned()];
        command.extend(paths);
        Ok(match self.run_git(&args.repo, command).await {
            Ok(output) => self.success(output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Discard working-tree changes for repository-relative paths. This is destructive."
    )]
    async fn git_restore(
        &self,
        Parameters(args): Parameters<PathsArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_writable() {
            return Ok(Self::failure(error));
        }
        let paths = match clean_paths(&args.paths) {
            Ok(paths) => paths,
            Err(error) => return Ok(Self::failure(error)),
        };
        let mut command = vec![
            "restore".to_owned(),
            "--worktree".to_owned(),
            "--".to_owned(),
        ];
        command.extend(paths);
        Ok(match self.run_git(&args.repo, command).await {
            Ok(output) => self.success(output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Create a Git commit from the current index. Repository hooks are disabled."
    )]
    async fn git_commit(
        &self,
        Parameters(args): Parameters<CommitArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_writable() {
            return Ok(Self::failure(error));
        }
        if let Err(error) = validate_commit_message(&args.message) {
            return Ok(Self::failure(error));
        }
        Ok(
            match self
                .run_git(&args.repo, ["commit", "-m", args.message.as_str()])
                .await
            {
                Ok(output) => self.success(output),
                Err(error) => Self::failure(error),
            },
        )
    }

    #[tool(
        description = "Create a lightweight or annotated Git tag. Tag names are validated by Git."
    )]
    async fn git_tag(
        &self,
        Parameters(args): Parameters<TagArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_writable() {
            return Ok(Self::failure(error));
        }
        if let Err(error) = self.validate_tag_name(&args.repo, &args.name).await {
            return Ok(Self::failure(error));
        }
        let result = if let Some(message) = args.message {
            if let Err(error) = validate_commit_message(&message) {
                return Ok(Self::failure(error));
            }
            self.run_git(
                &args.repo,
                ["tag", "-a", args.name.as_str(), "-m", message.as_str()],
            )
            .await
        } else {
            self.run_git(&args.repo, ["tag", args.name.as_str()]).await
        };
        Ok(match result {
            Ok(output) => self.success(output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Fetch and prune refs from a configured remote. Disabled in read-only mode and unless --allow-remote is enabled."
    )]
    async fn git_fetch(
        &self,
        Parameters(args): Parameters<RemoteArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_writable().and_then(|_| self.ensure_remote()) {
            return Ok(Self::failure(error));
        }
        let remote = args.remote.unwrap_or_else(|| "origin".into());
        if let Err(error) = validate_token("remote", &remote) {
            return Ok(Self::failure(error));
        }
        Ok(
            match self
                .run_git(&args.repo, ["fetch", "--prune", "--", remote.as_str()])
                .await
            {
                Ok(output) => self.success(output),
                Err(error) => Self::failure(error),
            },
        )
    }

    #[tool(
        description = "Fast-forward-only pull. Disabled unless local writes and remote operations are enabled."
    )]
    async fn git_pull(
        &self,
        Parameters(args): Parameters<PullArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_writable().and_then(|_| self.ensure_remote()) {
            return Ok(Self::failure(error));
        }
        let remote = args.remote.unwrap_or_else(|| "origin".into());
        if let Err(error) = validate_token("remote", &remote) {
            return Ok(Self::failure(error));
        }
        let mut command = vec![
            "pull".to_owned(),
            "--ff-only".to_owned(),
            "--".to_owned(),
            remote,
        ];
        if let Some(branch) = args.branch {
            if let Err(error) = validate_token("branch", &branch) {
                return Ok(Self::failure(error));
            }
            command.push(branch);
        }
        Ok(match self.run_git(&args.repo, command).await {
            Ok(output) => self.success(output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Push a ref to a remote without force. Disabled unless remote operations are enabled."
    )]
    async fn git_push(
        &self,
        Parameters(args): Parameters<PushArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_remote() {
            return Ok(Self::failure(error));
        }
        let remote = args.remote.unwrap_or_else(|| "origin".into());
        if let Err(error) = validate_token("remote", &remote) {
            return Ok(Self::failure(error));
        }
        let mut command = vec!["push".to_owned(), "--".to_owned(), remote];
        if let Some(refspec) = args.refspec {
            if let Err(error) = validate_refspec(&refspec) {
                return Ok(Self::failure(error));
            }
            command.push(refspec);
        }
        Ok(match self.run_git(&args.repo, command).await {
            Ok(output) => self.success(output),
            Err(error) => Self::failure(error),
        })
    }
}

fn clean_paths(paths: &[String]) -> std::result::Result<Vec<String>, String> {
    if paths.is_empty() {
        return Err("at least one path is required".into());
    }
    if paths.len() > 1024 {
        return Err("too many paths".into());
    }
    paths
        .iter()
        .map(|path| clean_relative_path(path).map(|p| p.to_string_lossy().into_owned()))
        .collect()
}

fn clean_repo_path(raw: &str) -> std::result::Result<PathBuf, String> {
    if raw.is_empty()
        || raw.len() > 4096
        || raw.as_bytes().contains(&0)
        || raw.chars().any(char::is_control)
    {
        return Err("repo must be a non-empty, bounded path without control characters".into());
    }
    clean_path(raw, true)
}

fn clean_relative_path(raw: &str) -> std::result::Result<PathBuf, String> {
    if raw.is_empty()
        || raw.len() > 4096
        || raw.as_bytes().contains(&0)
        || raw.chars().any(char::is_control)
    {
        return Err("path must be a non-empty, bounded path without control characters".into());
    }
    clean_path(raw, false)
}

fn clean_path(raw: &str, allow_dot: bool) -> std::result::Result<PathBuf, String> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err("absolute paths are not allowed".into());
    }
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => cleaned.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err("parent traversal (`..`) is not allowed".into()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute paths are not allowed".into());
            }
        }
    }
    if cleaned.as_os_str().is_empty() {
        if allow_dot && raw == "." {
            return Ok(PathBuf::from("."));
        }
        return Err("path must resolve below the configured root".into());
    }
    Ok(cleaned)
}

fn validate_revision(value: &str) -> std::result::Result<(), String> {
    validate_token("revision", value)
}

fn validate_token(kind: &str, value: &str) -> std::result::Result<(), String> {
    if value.is_empty()
        || value.len() > 1024
        || value.starts_with('-')
        || value.as_bytes().contains(&0)
        || value.chars().any(char::is_control)
    {
        return Err(format!("invalid {kind}"));
    }
    Ok(())
}

fn validate_object_id(value: &str) -> std::result::Result<(), String> {
    if (value.len() == 40 || value.len() == 64)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err("invalid object id returned by Git".to_owned())
    }
}

fn validate_commit_message(message: &str) -> std::result::Result<(), String> {
    if message.trim().is_empty()
        || message.len() > MAX_COMMIT_MESSAGE_BYTES
        || message.as_bytes().contains(&0)
        || message.contains('\r')
    {
        return Err(format!(
            "commit message must be 1..{MAX_COMMIT_MESSAGE_BYTES} bytes and must not contain NUL or CR"
        ));
    }
    Ok(())
}

fn is_conventional_commit_subject(subject: &str) -> bool {
    const TYPES: &[&str] = &[
        "feat", "fix", "docs", "test", "refactor", "perf", "build", "ci", "chore", "style",
        "revert",
    ];
    for kind in TYPES {
        let Some(mut rest) = subject.strip_prefix(kind) else {
            continue;
        };
        if let Some(after_scope) = rest.strip_prefix('(') {
            let Some(close) = after_scope.find(')') else {
                return false;
            };
            if close == 0 {
                return false;
            }
            let scope = &after_scope[..close];
            if scope.chars().any(char::is_control) || scope.contains('(') || scope.contains(')') {
                return false;
            }
            rest = &after_scope[close + 1..];
        }
        if let Some(after_bang) = rest.strip_prefix('!') {
            rest = after_bang;
        }
        if let Some(description) = rest.strip_prefix(": ") {
            return !description.trim().is_empty();
        }
    }
    false
}

fn validate_refspec(value: &str) -> std::result::Result<(), String> {
    validate_token("refspec", value)?;
    if value.starts_with('+') || value.contains("--force") {
        return Err("force push refspecs are not allowed".into());
    }
    Ok(())
}

fn bounded_utf8(bytes: Vec<u8>, limit: usize, truncated: &mut bool) -> String {
    if bytes.len() <= limit {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    *truncated = true;
    let mut output = String::from_utf8_lossy(&bytes[..limit]).into_owned();
    output.push_str("\n[output truncated by MCP Git]\n");
    output
}

fn format_git_failure(output: &GitOutput) -> String {
    let mut fields = BTreeMap::new();
    fields.insert(
        "exit_code",
        output
            .exit_code
            .map_or_else(|| "signal".into(), |value| value.to_string()),
    );
    if !output.stderr.trim().is_empty() {
        fields.insert("stderr", output.stderr.trim().to_owned());
    }
    if !output.stdout.trim().is_empty() {
        fields.insert("stdout", output.stdout.trim().to_owned());
    }
    serde_json::to_string_pretty(&fields).unwrap_or_else(|_| "git command failed".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rust_mcp_git=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let server = GitServer::new(&cli)?;
    tracing::info!(
        workspace_root = %server.workspace_root.display(),
        read_only = server.read_only,
        allow_remote = server.allow_remote,
        "starting multi-repository Git MCP server"
    );
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        process::Output,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    struct TestWorkspace {
        root: PathBuf,
        repo: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("rust-mcp-git-test-{}-{id}", std::process::id()));
            let repo = root.join("repo");
            fs::create_dir_all(&repo).unwrap();
            git_ok(&repo, ["init", "-q", "-b", "main"]);
            git_ok(&repo, ["config", "user.email", "mcp-git@example.invalid"]);
            git_ok(&repo, ["config", "user.name", "MCP Git Test"]);
            fs::write(repo.join("tracked.txt"), "base\n").unwrap();
            git_ok(&repo, ["add", "--", "tracked.txt"]);
            git_ok(&repo, ["commit", "-q", "-m", "chore: initial"]);
            Self { root, repo }
        }

        fn server(&self, read_only: bool) -> GitServer {
            GitServer {
                workspace_root: fs::canonicalize(&self.root).unwrap(),
                read_only,
                allow_remote: false,
                max_output_bytes: 1_048_576,
            }
        }

        fn commit_file(&self, path: &str, content: &str, message: &str) {
            fs::write(self.repo.join(path), content).unwrap();
            git_ok(&self.repo, ["add", "--", path]);
            git_ok(&self.repo, ["commit", "-q", "-m", message]);
        }

        fn head(&self) -> String {
            git_string(&self.repo, ["rev-parse", "HEAD"])
        }

        fn tree(&self) -> String {
            git_string(&self.repo, ["rev-parse", "HEAD^{tree}"])
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> Output {
        StdCommand::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap()
    }

    fn git_ok<const N: usize>(repo: &Path, args: [&str; N]) -> Output {
        let output = git_output(repo, args);
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git_string<const N: usize>(repo: &Path, args: [&str; N]) -> String {
        String::from_utf8(git_ok(repo, args).stdout)
            .unwrap()
            .trim()
            .to_owned()
    }

    #[test]
    fn validates_paths_tokens_and_conventional_subjects() {
        assert_eq!(
            clean_relative_path("studio/src/main.rs").unwrap(),
            PathBuf::from("studio/src/main.rs")
        );
        assert_eq!(clean_repo_path(".").unwrap(), PathBuf::from("."));
        assert!(clean_relative_path("../secret").is_err());
        assert!(clean_relative_path("/etc/passwd").is_err());
        assert!(validate_revision("--help").is_err());
        assert!(validate_token("branch", "main\n--help").is_err());
        assert!(validate_refspec("+HEAD:main").is_err());
        assert!(is_conventional_commit_subject("feat: add history plan"));
        assert!(is_conventional_commit_subject(
            "fix(git)!: tighten rewrite safety"
        ));
        assert!(!is_conventional_commit_subject("Add history plan"));
        assert!(!is_conventional_commit_subject("feat(): invalid"));
    }

    #[test]
    fn rejects_subdirectory_as_repo_selector() {
        let workspace = TestWorkspace::new();
        fs::create_dir_all(workspace.repo.join("subdir")).unwrap();
        assert!(workspace.server(false).resolve_repo("repo/subdir").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let workspace = TestWorkspace::new();
        let outside = std::env::temp_dir().join(format!(
            "rust-mcp-git-outside-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&outside).unwrap();
        git_ok(&outside, ["init", "-q", "-b", "main"]);
        symlink(&outside, workspace.root.join("escape")).unwrap();
        assert!(workspace.server(false).resolve_repo("escape").is_err());
        fs::remove_dir_all(outside).unwrap();
    }

    #[tokio::test]
    async fn history_plan_reports_branch_local_commits_and_dirty_tree() {
        let workspace = TestWorkspace::new();
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/history"]);
        workspace.commit_file("one.txt", "one\n", "feat: one");
        workspace.commit_file("two.txt", "two\n", "Add two");
        let server = workspace.server(false);
        let plan = server.history_plan("repo", "main").await.unwrap();
        assert_eq!(plan.branch, "feature/history");
        assert_eq!(plan.commit_count, 2);
        assert!(!plan.published);
        assert!(plan.can_rewrite);
        assert!(plan.commits[0].conventional);
        assert!(!plan.commits[1].conventional);

        fs::write(workspace.repo.join("dirty.txt"), "dirty\n").unwrap();
        let dirty = server.history_plan("repo", "main").await.unwrap();
        assert!(dirty.dirty);
        assert!(!dirty.can_rewrite);
    }

    #[tokio::test]
    async fn merge_readiness_enforces_limit_conventional_and_clean_tree() {
        let workspace = TestWorkspace::new();
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/readiness"]);
        for index in 0..5 {
            workspace.commit_file(
                &format!("{index}.txt"),
                &format!("{index}\n"),
                &format!("feat: commit {index}"),
            );
        }
        let server = workspace.server(false);
        let ready = server
            .validate_merge_readiness(&ValidateMergeReadinessArgs {
                repo: "repo".to_owned(),
                base: "main".to_owned(),
                max_commits: Some(5),
                require_conventional_commits: true,
                require_clean_tree: true,
            })
            .await
            .unwrap();
        assert!(ready.ready);

        workspace.commit_file("six.txt", "six\n", "not conventional");
        let blocked = server
            .validate_merge_readiness(&ValidateMergeReadinessArgs {
                repo: "repo".to_owned(),
                base: "main".to_owned(),
                max_commits: Some(5),
                require_conventional_commits: true,
                require_clean_tree: true,
            })
            .await
            .unwrap();
        assert!(!blocked.ready);
        assert!(!blocked.checks.commit_limit);
        assert!(!blocked.checks.conventional_commits);
    }

    #[tokio::test]
    async fn amend_message_and_index_are_atomic_and_reject_main() {
        let workspace = TestWorkspace::new();
        let server = workspace.server(false);
        let main_head = workspace.head();
        let blocked = server
            .amend_commit(&AmendCommitArgs {
                repo: "repo".to_owned(),
                message: Some("chore: rewritten".to_owned()),
                include_index: false,
            })
            .await;
        assert!(blocked.is_err());
        assert_eq!(workspace.head(), main_head);

        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/amend"]);
        workspace.commit_file("one.txt", "one\n", "feat: one");
        let first_head = workspace.head();
        let message_only = server
            .amend_commit(&AmendCommitArgs {
                repo: "repo".to_owned(),
                message: Some("feat: reword one".to_owned()),
                include_index: false,
            })
            .await
            .unwrap();
        assert_ne!(message_only.new_head, first_head);
        assert_eq!(
            git_string(&workspace.repo, ["log", "-1", "--pretty=%s"]),
            "feat: reword one"
        );

        fs::write(workspace.repo.join("two.txt"), "two\n").unwrap();
        git_ok(&workspace.repo, ["add", "--", "two.txt"]);
        server
            .amend_commit(&AmendCommitArgs {
                repo: "repo".to_owned(),
                message: None,
                include_index: true,
            })
            .await
            .unwrap();
        assert_eq!(git_string(&workspace.repo, ["status", "--porcelain"]), "");
        assert_eq!(git_string(&workspace.repo, ["show", "HEAD:two.txt"]), "two");
    }

    #[tokio::test]
    async fn reword_multiple_commits_preserves_final_tree_and_maps_suffix() {
        let workspace = TestWorkspace::new();
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/reword"]);
        workspace.commit_file("one.txt", "one\n", "Add one");
        let one = workspace.head();
        workspace.commit_file("two.txt", "two\n", "feat: two");
        workspace.commit_file("three.txt", "three\n", "Add three");
        let three = workspace.head();
        let tree_before = workspace.tree();
        let server = workspace.server(false);
        let output = server
            .reword_commits(&RewordCommitsArgs {
                repo: "repo".to_owned(),
                base: "main".to_owned(),
                changes: vec![
                    RewordCommit {
                        commit: one,
                        message: "feat: add one".to_owned(),
                    },
                    RewordCommit {
                        commit: three,
                        message: "test: add three".to_owned(),
                    },
                ],
                rewrite_policy: RewritePolicy::LocalOnly,
            })
            .await
            .unwrap();
        assert_eq!(output.rewritten.len(), 3);
        assert_eq!(workspace.tree(), tree_before);
        let subjects = git_string(
            &workspace.repo,
            ["log", "--reverse", "--pretty=%s", "main..HEAD"],
        );
        assert_eq!(subjects, "feat: add one\nfeat: two\ntest: add three");
    }

    #[tokio::test]
    async fn squash_contiguous_range_preserves_final_tree_and_rejects_non_contiguous() {
        let workspace = TestWorkspace::new();
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/squash"]);
        workspace.commit_file("one.txt", "one\n", "feat: one");
        let one = workspace.head();
        workspace.commit_file("two.txt", "two\n", "feat: two");
        let two = workspace.head();
        workspace.commit_file("three.txt", "three\n", "feat: three");
        let three = workspace.head();
        let tree_before = workspace.tree();
        let server = workspace.server(false);
        let rejected = server
            .squash_commits(&SquashCommitsArgs {
                repo: "repo".to_owned(),
                base: "main".to_owned(),
                commits: vec![one.clone(), three.clone()],
                message: "feat: invalid squash".to_owned(),
                rewrite_policy: RewritePolicy::LocalOnly,
            })
            .await;
        assert!(rejected.is_err());
        assert_eq!(workspace.head(), three);

        let output = server
            .squash_commits(&SquashCommitsArgs {
                repo: "repo".to_owned(),
                base: "main".to_owned(),
                commits: vec![one, two],
                message: "feat: combine one and two".to_owned(),
                rewrite_policy: RewritePolicy::LocalOnly,
            })
            .await
            .unwrap();
        assert_eq!(output.squashed_count, 2);
        assert_eq!(workspace.tree(), tree_before);
        assert_eq!(
            git_string(&workspace.repo, ["rev-list", "--count", "main..HEAD"]),
            "2"
        );
        assert_eq!(
            git_string(
                &workspace.repo,
                ["log", "--reverse", "--pretty=%s", "main..HEAD"]
            ),
            "feat: combine one and two\nfeat: three"
        );
    }

    #[tokio::test]
    async fn local_only_allows_unpublished_suffix_but_blocks_published_commit() {
        let workspace = TestWorkspace::new();
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/published"]);
        workspace.commit_file("one.txt", "one\n", "Add one");
        let published = workspace.head();
        git_ok(&workspace.repo, ["branch", "upstream-snapshot", "HEAD"]);
        git_ok(
            &workspace.repo,
            ["config", "branch.feature/published.remote", "."],
        );
        git_ok(
            &workspace.repo,
            [
                "config",
                "branch.feature/published.merge",
                "refs/heads/upstream-snapshot",
            ],
        );
        workspace.commit_file("two.txt", "two\n", "Add two");
        let local = workspace.head();
        let server = workspace.server(false);

        let allowed = server
            .reword_commits(&RewordCommitsArgs {
                repo: "repo".to_owned(),
                base: "main".to_owned(),
                changes: vec![RewordCommit {
                    commit: local,
                    message: "feat: local two".to_owned(),
                }],
                rewrite_policy: RewritePolicy::LocalOnly,
            })
            .await;
        assert!(allowed.is_ok());

        let blocked = server
            .reword_commits(&RewordCommitsArgs {
                repo: "repo".to_owned(),
                base: "main".to_owned(),
                changes: vec![RewordCommit {
                    commit: published,
                    message: "feat: published one".to_owned(),
                }],
                rewrite_policy: RewritePolicy::LocalOnly,
            })
            .await;
        assert!(blocked.is_err());
    }

    #[tokio::test]
    async fn switch_create_merge_and_delete_branch_regressions() {
        let workspace = TestWorkspace::new();
        let server = workspace.server(false);
        let original_head = workspace.head();
        let created = server
            .create_branch("repo", "feature/no-ff", None, true)
            .await
            .unwrap();
        assert_eq!(created.branch, "feature/no-ff");
        assert_eq!(created.head, original_head);
        workspace.commit_file("feature.txt", "feature\n", "feat: feature");
        let feature_head = workspace.head();
        server.switch_branch("repo", "main").await.unwrap();
        let result = server
            .merge_branch("repo", "feature/no-ff", MergeMode::NoFf)
            .await
            .unwrap();
        assert_eq!(result.branch, "main");
        assert_ne!(result.head, feature_head);
        let parents = git_string(
            &workspace.repo,
            ["rev-list", "--parents", "-n", "1", "HEAD"],
        );
        assert_eq!(parents.split_whitespace().count(), 3);
        server.delete_branch("repo", "feature/no-ff").await.unwrap();
    }

    #[tokio::test]
    async fn read_only_blocks_history_mutation() {
        let workspace = TestWorkspace::new();
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/readonly"]);
        workspace.commit_file("one.txt", "one\n", "feat: one");
        let server = workspace.server(true);
        assert!(
            server
                .amend_commit(&AmendCommitArgs {
                    repo: "repo".to_owned(),
                    message: Some("feat: blocked".to_owned()),
                    include_index: false,
                })
                .await
                .is_err()
        );
    }
}
