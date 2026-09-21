use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::Command as StdCommand,
    sync::atomic::{AtomicU64, Ordering},
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
use serde::{Deserialize, Serialize};
use tokio::process::Command;

const MAX_COMMIT_MESSAGE_BYTES: usize = 16_384;
const MAX_HISTORY_COMMITS: usize = 512;
const DEFAULT_MAX_MERGE_COMMITS: u32 = 5;
const PROTECTED_TRUNK: &str = "main";
static NEXT_TEMP_TAG_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Parser)]
#[command(version, about = "Workspace-confined multi-repository Git MCP server")]
struct Cli {
    #[arg(long, env = "MCP_GIT_ROOT")]
    root: PathBuf,

    #[arg(long, env = "MCP_GIT_READ_ONLY", default_value_t = false)]
    read_only: bool,

    /// Backward-compatible switch enabling both remote reads and remote writes.
    #[arg(long, env = "MCP_GIT_ALLOW_REMOTE", default_value_t = false)]
    allow_remote: bool,

    /// Allow remote reads such as fetch/pull and publication verification.
    #[arg(long, env = "MCP_GIT_ALLOW_REMOTE_READ", default_value_t = false)]
    allow_remote_read: bool,

    /// Allow publishing operations such as push. Force push is never exposed.
    #[arg(long, env = "MCP_GIT_ALLOW_REMOTE_WRITE", default_value_t = false)]
    allow_remote_write: bool,

    #[arg(long, env = "MCP_GIT_MAX_OUTPUT_BYTES", default_value_t = 1_048_576)]
    max_output_bytes: usize,

    /// Root containing trusted release-gate evidence. Required only for enrolled repositories.
    #[arg(long, env = "MCP_GIT_RELEASE_EVIDENCE_ROOT")]
    release_evidence_root: Option<PathBuf>,

    /// Comma-separated workspace-relative repositories that require exact PASS release evidence.
    #[arg(
        long = "release-evidence-required-repo",
        env = "MCP_GIT_RELEASE_EVIDENCE_REQUIRED_REPOS",
        value_delimiter = ','
    )]
    release_evidence_required_repos: Vec<String>,
}

#[derive(Debug, Clone)]
struct GitServer {
    workspace_root: PathBuf,
    read_only: bool,
    allow_remote_read: bool,
    allow_remote_write: bool,
    max_output_bytes: usize,
    release_evidence_root: Option<PathBuf>,
    release_evidence_required_repos: BTreeSet<String>,
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
struct ReleaseTagArgs {
    repo: String,
    version: String,
    message: Option<String>,
    /// Path to trusted release-gate evidence below the configured evidence root.
    release_evidence_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReleaseEvidence {
    schema_version: u32,
    gate: String,
    status: String,
    repo: String,
    project_key: String,
    commit: String,
    branch: String,
    analysis_id: Option<String>,
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
    /// Optional Conventional Commit-compatible merge message. When omitted, a safe message is derived from the work-branch name.
    message: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DeleteBranchArgs {
    repo: String,
    branch: String,
}

#[derive(Debug, Clone, Copy, Default, serde::Deserialize, schemars::JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum RewritePolicy {
    /// Only rewrite commits that are not known to be reachable from an upstream or remote-tracking ref.
    #[default]
    LocalOnly,
    /// Permit rewriting commits known to remote-tracking refs. This never force-pushes.
    AllowPublished,
}

#[derive(Debug, Clone, Copy, Default, serde::Deserialize, schemars::JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum PublicationGuard {
    /// Refresh the configured remote and prove the commit/tag is not published before rewriting local release state.
    #[default]
    VerifyRemote,
    /// Explicit caller assertion that publication state was verified elsewhere. No remote publication check is performed.
    CallerConfirmedUnpublished,
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
    #[serde(default)]
    require_release_commit: bool,
    /// Plain MAJOR.MINOR.PATCH version used to require an exact final `chore(release): prepare vX.Y.Z` commit.
    release_version: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DevControlReadinessArgs {
    repo: String,
    /// Plain MAJOR.MINOR.PATCH version expected in the final release-prep commit.
    version: String,
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

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct VerifyUnpublishedArgs {
    repo: String,
    remote: Option<String>,
    #[serde(default)]
    commits: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RewindMergeArgs {
    repo: String,
    expected_merge: String,
    remote: Option<String>,
    #[serde(default)]
    publication_guard: PublicationGuard,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReplaceLocalTagArgs {
    repo: String,
    name: String,
    expected_old_target: String,
    new_target: Option<String>,
    message: String,
    remote: Option<String>,
    #[serde(default)]
    publication_guard: PublicationGuard,
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
    published_refs: Vec<String>,
    commit_count: usize,
    commits: Vec<HistoryCommit>,
    can_rewrite: bool,
    blocking_reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ReadinessChecks {
    not_on_main: bool,
    non_empty_history: bool,
    clean_tree: bool,
    base_reconciled: bool,
    commit_limit: bool,
    conventional_commits: bool,
    release_commit_last: bool,
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
    release_version: Option<String>,
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

#[derive(Debug, Serialize)]
struct PublishedCommit {
    commit: String,
    refs: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PublishedTag {
    tag: String,
    remote_lines: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PublicationVerification {
    remote: String,
    fetched: bool,
    unpublished: bool,
    commits_checked: Vec<String>,
    tags_checked: Vec<String>,
    published_commits: Vec<PublishedCommit>,
    published_tags: Vec<PublishedTag>,
}

#[derive(Debug, Serialize)]
struct RewindMergeOutput {
    operation: &'static str,
    branch: String,
    old_head: String,
    first_parent: String,
    new_head: String,
    publication_guard: PublicationGuard,
    status: String,
}

#[derive(Debug, Serialize)]
struct ReplaceLocalTagOutput {
    operation: &'static str,
    name: String,
    old_target: String,
    new_target: String,
    annotated: bool,
    publication_guard: PublicationGuard,
    temporary_tag_cleanup: bool,
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
            allow_remote_read: cli.allow_remote || cli.allow_remote_read,
            allow_remote_write: cli.allow_remote || cli.allow_remote_write,
            max_output_bytes: cli.max_output_bytes.max(1),
            release_evidence_root: cli
                .release_evidence_root
                .as_ref()
                .map(std::fs::canonicalize)
                .transpose()
                .with_context(|| "cannot resolve release evidence root")?,
            release_evidence_required_repos: cli
                .release_evidence_required_repos
                .iter()
                .map(|repo| clean_repo_path(repo).map(|path| path.to_string_lossy().into_owned()))
                .collect::<std::result::Result<BTreeSet<_>, _>>()
                .map_err(anyhow::Error::msg)?,
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

    fn ensure_remote_read(&self) -> std::result::Result<(), String> {
        if self.allow_remote_read {
            Ok(())
        } else {
            Err("remote Git reads are disabled; start with --allow-remote-read (or legacy --allow-remote) to enable fetch/publication checks".into())
        }
    }

    fn ensure_remote_write(&self) -> std::result::Result<(), String> {
        if self.allow_remote_write {
            Ok(())
        } else {
            Err("remote Git writes are disabled; start with --allow-remote-write (or legacy --allow-remote) to enable push".into())
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

    async fn merge_in_progress(&self, repo: &str) -> std::result::Result<bool, String> {
        let output = self
            .run_git_raw(
                repo,
                ["rev-parse", "--verify", "--quiet", "MERGE_HEAD"],
                &[],
            )
            .await?;
        match output.exit_code {
            Some(0) => Ok(true),
            Some(1) | Some(128) => Ok(false),
            _ => Err(format_git_failure(&output)),
        }
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

    async fn remote_refs_containing(
        &self,
        repo: &str,
        commit: &str,
    ) -> std::result::Result<Vec<String>, String> {
        let sha = self.resolve_commit_sha(repo, commit).await?;
        let contains = format!("--contains={sha}");
        let output = self
            .run_git(
                repo,
                [
                    "for-each-ref",
                    "--format=%(refname:short)",
                    contains.as_str(),
                    "refs/remotes",
                ],
            )
            .await?;
        if output.truncated {
            return Err("remote-ref output exceeded configured Git MCP output limit".into());
        }
        Ok(output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect())
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
        let mut published_refs = BTreeSet::new();
        for commit in &commits {
            for remote_ref in self.remote_refs_containing(repo, &commit.sha).await? {
                published_refs.insert(remote_ref);
            }
            if let Some(upstream_ref) = upstream.as_deref()
                && self.is_ancestor(repo, &commit.sha, upstream_ref).await?
            {
                published_refs.insert(upstream_ref.to_owned());
            }
        }
        let published = !published_refs.is_empty();

        let mut blocking_reasons = Vec::new();
        if detached_head {
            blocking_reasons.push("detached_head".to_owned());
        }
        if branch.as_deref() == Some(PROTECTED_TRUNK) {
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
            published_refs: published_refs.into_iter().collect(),
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
        if let Some(version) = args.release_version.as_deref() {
            validate_semver_core(version)?;
        }
        let plan = self.history_plan(&args.repo, &args.base).await?;
        let base_sha = self.resolve_commit_sha(&args.repo, &args.base).await?;
        let max_allowed = args
            .max_commits
            .unwrap_or(DEFAULT_MAX_MERGE_COMMITS)
            .clamp(1, DEFAULT_MAX_MERGE_COMMITS);
        let not_on_main = !plan.detached_head && plan.branch != PROTECTED_TRUNK;
        let non_empty_history = !plan.commits.is_empty();
        let clean_tree = !plan.dirty;
        let base_reconciled = plan.merge_base == base_sha;
        let commit_limit = plan.commit_count <= max_allowed as usize;
        let invalid_commits: Vec<_> = plan
            .commits
            .iter()
            .filter(|commit| !commit.conventional)
            .collect();
        let conventional_commits = invalid_commits.is_empty();
        let expected_release = args
            .release_version
            .as_deref()
            .map(expected_release_subject);
        let release_commit_last = if args.require_release_commit {
            plan.commits.last().is_some_and(|commit| {
                if let Some(expected) = expected_release.as_deref() {
                    commit.subject == expected
                } else {
                    is_release_commit_subject(&commit.subject)
                }
            })
        } else {
            true
        };

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
        if !non_empty_history {
            violations.push(ReadinessViolation {
                code: "NO_BRANCH_COMMITS",
                message: "work branch must contain at least one commit relative to the target base"
                    .to_owned(),
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
        if !base_reconciled {
            violations.push(ReadinessViolation {
                code: "BASE_NOT_RECONCILED",
                message: format!(
                    "work branch does not contain the current `{}` base tip",
                    args.base
                ),
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
        if args.require_release_commit && !release_commit_last {
            violations.push(ReadinessViolation {
                code: "RELEASE_COMMIT_NOT_LAST",
                message: expected_release.map_or_else(
                    || "final work-branch commit must be `chore(release): prepare vMAJOR.MINOR.PATCH`".to_owned(),
                    |expected| format!("final work-branch commit must be `{expected}`"),
                ),
                commit: plan.commits.last().map(|commit| commit.sha.clone()),
                subject: plan.commits.last().map(|commit| commit.subject.clone()),
            });
        }

        Ok(MergeReadiness {
            ready: not_on_main
                && non_empty_history
                && (!args.require_clean_tree || clean_tree)
                && base_reconciled
                && commit_limit
                && (!args.require_conventional_commits || conventional_commits)
                && release_commit_last,
            branch: plan.branch,
            base: plan.base,
            commit_count: plan.commit_count,
            max_allowed,
            release_version: args.release_version.clone(),
            checks: ReadinessChecks {
                not_on_main,
                non_empty_history,
                clean_tree,
                base_reconciled,
                commit_limit,
                conventional_commits,
                release_commit_last,
            },
            violations,
        })
    }

    async fn dev_control_readiness(
        &self,
        args: &DevControlReadinessArgs,
    ) -> std::result::Result<MergeReadiness, String> {
        validate_semver_core(&args.version)?;
        self.validate_merge_readiness(&ValidateMergeReadinessArgs {
            repo: args.repo.clone(),
            base: PROTECTED_TRUNK.to_owned(),
            max_commits: Some(DEFAULT_MAX_MERGE_COMMITS),
            require_conventional_commits: true,
            require_clean_tree: true,
            require_release_commit: true,
            release_version: Some(args.version.clone()),
        })
        .await
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
        validate_conventional_commit_message(message)?;
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
        if plan.branch == PROTECTED_TRUNK {
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
            return Err("rewrite suffix contains a root or merge commit; merge-aware rewriting is not supported".into());
        }
        if matches!(policy, RewritePolicy::LocalOnly) {
            for commit in &plan.commits[start_index..] {
                let refs = self.remote_refs_containing(repo, &commit.sha).await?;
                if !refs.is_empty() {
                    return Err(format!(
                        "rewrite blocked: commit {} is already reachable from remote-tracking ref(s): {}",
                        commit.sha,
                        refs.join(", ")
                    ));
                }
                if let Some(upstream) = plan.upstream.as_deref()
                    && self.is_ancestor(repo, &commit.sha, upstream).await?
                {
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
            if branch == PROTECTED_TRUNK {
                return Err("amend is not allowed on protected branch `main`".to_owned());
            }
            let snapshot = self.commit_snapshot(&args.repo, &original_head).await?;
            if snapshot.parents.len() != 1 {
                return Err("amend rejects root and merge commits".to_owned());
            }
            if let Some(message) = args.message.as_deref() {
                validate_conventional_commit_message(message)?;
            }
            if args.message.is_none() && !args.include_index {
                return Err("amend requires a new message or include_index=true".to_owned());
            }
            let remote_refs = self
                .remote_refs_containing(&args.repo, &original_head)
                .await?;
            if !remote_refs.is_empty() {
                return Err(format!(
                    "amend blocked: HEAD is already reachable from remote-tracking ref(s): {}",
                    remote_refs.join(", ")
                ));
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
            validate_conventional_commit_message(message)?;
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
                validate_conventional_commit_message(&change.message)?;
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
            validate_conventional_commit_message(&args.message)?;
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
        message: Option<&str>,
    ) -> std::result::Result<GitMutationOutput, String> {
        self.ensure_writable()?;
        self.validate_commitish(repo, source, "merge source")
            .await?;
        let current = self.current_branch_required(repo).await?;
        if current != PROTECTED_TRUNK {
            return Err("dev-git-control merge operation requires the current branch to be protected trunk `main`".to_owned());
        }
        if self.is_dirty(repo).await? {
            return Err("merge requires a clean working tree and index".to_owned());
        }
        let merge_message = message
            .map(str::to_owned)
            .unwrap_or_else(|| conventional_merge_message(source));
        validate_conventional_commit_message(&merge_message)?;
        let output = match mode {
            MergeMode::NoFf => {
                self.run_git(
                    repo,
                    [
                        "merge",
                        "--no-ff",
                        "-m",
                        merge_message.as_str(),
                        "--",
                        source,
                    ],
                )
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

    async fn verify_unpublished(
        &self,
        args: &VerifyUnpublishedArgs,
    ) -> std::result::Result<PublicationVerification, String> {
        self.ensure_writable()?;
        self.ensure_remote_read()?;
        if args.commits.is_empty() && args.tags.is_empty() {
            return Err("publication verification requires at least one commit or tag".to_owned());
        }
        if args.commits.len() > 64 || args.tags.len() > 64 {
            return Err(
                "publication verification accepts at most 64 commits and 64 tags".to_owned(),
            );
        }
        let remote = args.remote.clone().unwrap_or_else(|| "origin".to_owned());
        validate_token("remote", &remote)?;
        self.run_git(&args.repo, ["remote", "get-url", "--", remote.as_str()])
            .await?;

        let mut commits_checked = Vec::new();
        for commit in &args.commits {
            commits_checked.push(self.resolve_commit_sha(&args.repo, commit).await?);
        }
        let mut tags_checked = Vec::new();
        for tag in &args.tags {
            self.validate_tag_name(&args.repo, tag).await?;
            tags_checked.push(tag.clone());
        }

        self.run_git(
            &args.repo,
            ["fetch", "--prune", "--tags", "--", remote.as_str()],
        )
        .await?;

        let mut published_commits = Vec::new();
        for commit in &commits_checked {
            let prefix = format!("refs/remotes/{remote}/");
            let contains = format!("--contains={commit}");
            let output = self
                .run_git(
                    &args.repo,
                    [
                        "for-each-ref",
                        "--format=%(refname:short)",
                        contains.as_str(),
                        prefix.as_str(),
                    ],
                )
                .await?;
            let refs: Vec<_> = output
                .stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect();
            if !refs.is_empty() {
                published_commits.push(PublishedCommit {
                    commit: commit.clone(),
                    refs,
                });
            }
        }

        let mut published_tags = Vec::new();
        for tag in &tags_checked {
            let pattern = format!("refs/tags/{tag}");
            let output = self
                .run_git(
                    &args.repo,
                    [
                        "ls-remote",
                        "--tags",
                        "--refs",
                        remote.as_str(),
                        pattern.as_str(),
                    ],
                )
                .await?;
            let lines: Vec<_> = output
                .stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect();
            if !lines.is_empty() {
                published_tags.push(PublishedTag {
                    tag: tag.clone(),
                    remote_lines: lines,
                });
            }
        }

        Ok(PublicationVerification {
            remote,
            fetched: true,
            unpublished: published_commits.is_empty() && published_tags.is_empty(),
            commits_checked,
            tags_checked,
            published_commits,
            published_tags,
        })
    }

    async fn rewind_merge(
        &self,
        args: &RewindMergeArgs,
    ) -> std::result::Result<RewindMergeOutput, RewriteFailure> {
        if let Err(error) = self.ensure_writable() {
            return Err(RewriteFailure {
                operation: "rewind_merge",
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
                    operation: "rewind_merge",
                    status: "failed",
                    restored: true,
                    original_head: "unknown".to_owned(),
                    current_head: "unknown".to_owned(),
                    error,
                });
            }
        };
        let result = async {
            let expected = self
                .resolve_commit_sha(&args.repo, &args.expected_merge)
                .await?;
            if original_head != expected {
                return Err(format!(
                    "rewind blocked: HEAD is {original_head}, expected merge is {expected}"
                ));
            }
            if self.is_dirty(&args.repo).await? {
                return Err("rewind requires a clean working tree and index".to_owned());
            }
            let branch = self.current_branch_required(&args.repo).await?;
            let snapshot = self.commit_snapshot(&args.repo, &expected).await?;
            if snapshot.parents.len() != 2 {
                return Err("rewind requires HEAD to be a two-parent merge commit".to_owned());
            }
            if matches!(args.publication_guard, PublicationGuard::VerifyRemote) {
                let verification = self
                    .verify_unpublished(&VerifyUnpublishedArgs {
                        repo: args.repo.clone(),
                        remote: args.remote.clone(),
                        commits: vec![expected.clone()],
                        tags: Vec::new(),
                    })
                    .await?;
                if !verification.unpublished {
                    return Err(format!(
                        "rewind blocked: merge commit is published on remote `{}`",
                        verification.remote
                    ));
                }
                if self.head_sha(&args.repo).await? != expected {
                    return Err(
                        "rewind blocked: HEAD changed while publication state was being verified"
                            .to_owned(),
                    );
                }
            }
            let first_parent = snapshot.parents[0].clone();
            self.run_git(&args.repo, ["reset", "--keep", first_parent.as_str()])
                .await?;
            let new_head = self.head_sha(&args.repo).await?;
            if new_head != first_parent {
                return Err(
                    "rewind failed integrity check: HEAD did not move to merge first parent"
                        .to_owned(),
                );
            }
            if self.is_dirty(&args.repo).await? {
                return Err(
                    "rewind failed integrity check: working tree is not clean after reset --keep"
                        .to_owned(),
                );
            }
            Ok(RewindMergeOutput {
                operation: "rewind_merge",
                branch,
                old_head: expected,
                first_parent: first_parent.clone(),
                new_head,
                publication_guard: args.publication_guard,
                status: self.short_status(&args.repo).await?,
            })
        }
        .await;
        match result {
            Ok(output) => Ok(output),
            Err(error) => Err(self
                .rewrite_failure("rewind_merge", &args.repo, &original_head, error)
                .await),
        }
    }

    async fn tag_object_sha(&self, repo: &str, name: &str) -> std::result::Result<String, String> {
        self.validate_tag_name(repo, name).await?;
        let reference = format!("refs/tags/{name}");
        let output = self
            .run_git(
                repo,
                [
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    reference.as_str(),
                ],
            )
            .await?;
        let sha = output.stdout.trim().to_owned();
        validate_object_id(&sha)?;
        Ok(sha)
    }

    async fn tag_target_commit(
        &self,
        repo: &str,
        name: &str,
    ) -> std::result::Result<String, String> {
        self.validate_tag_name(repo, name).await?;
        self.resolve_commit_sha(repo, name).await
    }

    async fn replace_local_tag(
        &self,
        args: &ReplaceLocalTagArgs,
    ) -> std::result::Result<ReplaceLocalTagOutput, String> {
        self.ensure_writable()?;
        self.validate_tag_name(&args.repo, &args.name).await?;
        validate_commit_message(&args.message)?;
        let expected_old = self
            .resolve_commit_sha(&args.repo, &args.expected_old_target)
            .await?;
        let actual_old = self.tag_target_commit(&args.repo, &args.name).await?;
        if actual_old != expected_old {
            return Err(format!(
                "tag replacement blocked: `{}` currently targets {actual_old}, expected {expected_old}",
                args.name
            ));
        }
        if matches!(args.publication_guard, PublicationGuard::VerifyRemote) {
            let verification = self
                .verify_unpublished(&VerifyUnpublishedArgs {
                    repo: args.repo.clone(),
                    remote: args.remote.clone(),
                    commits: Vec::new(),
                    tags: vec![args.name.clone()],
                })
                .await?;
            if !verification.unpublished {
                return Err(format!(
                    "tag replacement blocked: `{}` already exists on remote `{}`",
                    args.name, verification.remote
                ));
            }
        }
        let new_target = match args.new_target.as_deref() {
            Some(target) => self.resolve_commit_sha(&args.repo, target).await?,
            None => self.head_sha(&args.repo).await?,
        };
        let old_object = self.tag_object_sha(&args.repo, &args.name).await?;
        let temp_id = NEXT_TEMP_TAG_ID.fetch_add(1, Ordering::Relaxed);
        let safe_name = args.name.replace('/', "-");
        let temp_name = format!(
            "mcp-replacement/{safe_name}-{}-{temp_id}",
            std::process::id()
        );
        self.validate_tag_name(&args.repo, &temp_name).await?;
        self.run_git(
            &args.repo,
            [
                "tag",
                "-a",
                temp_name.as_str(),
                new_target.as_str(),
                "-m",
                args.message.as_str(),
            ],
        )
        .await?;
        let temp_object = match self.tag_object_sha(&args.repo, &temp_name).await {
            Ok(object) => object,
            Err(error) => {
                let _ = self
                    .run_git_raw(&args.repo, ["tag", "-d", "--", temp_name.as_str()], &[])
                    .await;
                return Err(error);
            }
        };
        let reference = format!("refs/tags/{}", args.name);
        if let Err(error) = self
            .run_git(
                &args.repo,
                [
                    "update-ref",
                    reference.as_str(),
                    temp_object.as_str(),
                    old_object.as_str(),
                ],
            )
            .await
        {
            let _ = self
                .run_git_raw(&args.repo, ["tag", "-d", "--", temp_name.as_str()], &[])
                .await;
            return Err(error);
        }
        let cleanup = self
            .run_git_raw(&args.repo, ["tag", "-d", "--", temp_name.as_str()], &[])
            .await?
            .exit_code
            == Some(0);
        let confirmed = self.tag_target_commit(&args.repo, &args.name).await?;
        if confirmed != new_target {
            return Err("tag replacement integrity check failed: tag target does not match requested target".to_owned());
        }
        Ok(ReplaceLocalTagOutput {
            operation: "replace_local_tag",
            name: args.name.clone(),
            old_target: actual_old,
            new_target,
            annotated: true,
            publication_guard: args.publication_guard,
            temporary_tag_cleanup: cleanup,
        })
    }

    fn verify_release_evidence(
        &self,
        repo: &str,
        head: &str,
        evidence_path: Option<&str>,
    ) -> std::result::Result<(), String> {
        let repo_selector = clean_repo_path(repo)?.to_string_lossy().into_owned();
        if !self
            .release_evidence_required_repos
            .contains(&repo_selector)
        {
            return Ok(());
        }

        let evidence_root = self.release_evidence_root.as_ref().ok_or_else(|| {
            "release evidence is required for this repository but no evidence root is configured"
                .to_owned()
        })?;
        let relative = evidence_path.ok_or_else(|| {
            "release evidence is required for this repository; provide release_evidence_path"
                .to_owned()
        })?;
        let relative = clean_repo_path(relative)?;
        let requested = evidence_root.join(relative);
        let canonical = std::fs::canonicalize(&requested)
            .map_err(|error| format!("cannot resolve release evidence: {error}"))?;
        if !canonical.starts_with(evidence_root) || !canonical.is_file() {
            return Err(
                "release evidence must be a regular file below the configured evidence root"
                    .to_owned(),
            );
        }

        let raw = std::fs::read(&canonical)
            .map_err(|error| format!("cannot read release evidence: {error}"))?;
        let evidence: ReleaseEvidence = serde_json::from_slice(&raw)
            .map_err(|error| format!("invalid release evidence JSON: {error}"))?;
        let repo_root = self.resolve_repo(repo)?;
        if evidence.schema_version != 1
            || evidence.gate != "sonarqube-main"
            || evidence.status != "PASS"
            || evidence.branch != PROTECTED_TRUNK
            || evidence.commit != head
            || evidence.repo != repo_root.to_string_lossy()
            || evidence.project_key.trim().is_empty()
            || evidence.analysis_id.as_deref().is_none_or(str::is_empty)
        {
            return Err(
                "release evidence does not prove a PASS for the current main HEAD".to_owned(),
            );
        }
        Ok(())
    }

    async fn create_release_tag(
        &self,
        args: &ReleaseTagArgs,
    ) -> std::result::Result<GitOutput, String> {
        self.ensure_writable()?;
        validate_semver_core(&args.version)?;
        let branch = self.current_branch_required(&args.repo).await?;
        if branch != PROTECTED_TRUNK {
            return Err("release tags may only be created on protected trunk `main`".to_owned());
        }
        if self.is_dirty(&args.repo).await? {
            return Err("release tagging requires a clean working tree and index".to_owned());
        }
        let head = self.commit_snapshot(&args.repo, "HEAD").await?;
        if head.parents.len() != 2 {
            return Err(
                "release tagging requires HEAD to be the two-parent no-ff merge result".to_owned(),
            );
        }
        self.verify_release_evidence(&args.repo, &head.sha, args.release_evidence_path.as_deref())?;
        let name = format!("v{}", args.version);
        self.validate_tag_name(&args.repo, &name).await?;
        let reference = format!("refs/tags/{name}");
        let exists = self
            .run_git_raw(
                &args.repo,
                ["show-ref", "--verify", "--quiet", reference.as_str()],
                &[],
            )
            .await?;
        if exists.exit_code == Some(0) {
            return Err(format!(
                "release tag `{name}` already exists; use guarded tag-recovery tooling rather than silently moving it"
            ));
        }
        let message = args
            .message
            .clone()
            .unwrap_or_else(|| format!("Release {name}"));
        validate_commit_message(&message)?;
        self.run_git(
            &args.repo,
            ["tag", "-a", name.as_str(), "-m", message.as_str()],
        )
        .await
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
        description = "Inspect branch-local commits relative to a base branch and report rewrite safety, remote-tracking publication evidence, and merge-base state without mutating repository state."
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
        description = "Read-only merge-readiness checks for branch count, Conventional Commits, clean tree, current-base reconciliation, and optional final release-prep commit policy."
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
        description = "Apply the dev-git-control pre-merge gate: work branch, clean tree, current main reconciliation, 1..5 commits, Conventional Commits, and exact final release-prep commit."
    )]
    async fn git_dev_control_readiness(
        &self,
        Parameters(args): Parameters<DevControlReadinessArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.dev_control_readiness(&args).await {
            Ok(readiness) => self.success_json(&readiness),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Safely amend HEAD on a non-main, unpublished branch by replacing its Conventional Commit message and/or including the current index. No force push is performed."
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
        description = "Rewrite Conventional Commit messages for branch-local commits using an atomic semantic replay. Final HEAD tree must remain identical."
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
        description = "Squash one contiguous oldest-to-newest range of branch-local commits into one Conventional Commit while preserving the final HEAD tree."
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
        description = "Refresh a remote and prove selected commits/tags are unpublished. Requires remote-read permission and never pushes."
    )]
    async fn git_verify_unpublished(
        &self,
        Parameters(args): Parameters<VerifyUnpublishedArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.verify_unpublished(&args).await {
            Ok(output) => self.success_json(&output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Guarded release-recovery operation: move the current branch from an expected two-parent merge back to its first parent using reset --keep. Remote-unpublished verification is the default; arbitrary reset is not exposed."
    )]
    async fn git_rewind_merge(
        &self,
        Parameters(args): Parameters<RewindMergeArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.rewind_merge(&args).await {
            Ok(output) => self.success_json(&output),
            Err(error) => self.error_json(&error),
        })
    }

    #[tool(
        description = "Atomically replace an existing local tag after checking its expected old target. Remote-unpublished verification is the default; the replacement is annotated and compare-and-swap guarded."
    )]
    async fn git_replace_local_tag(
        &self,
        Parameters(args): Parameters<ReplaceLocalTagArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.replace_local_tag(&args).await {
            Ok(output) => self.success_json(&output),
            Err(error) => Self::failure(error),
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
        description = "Merge a validated work branch into current main using explicit no-fast-forward semantics and a Conventional Commit-compatible merge message."
    )]
    async fn git_merge(
        &self,
        Parameters(args): Parameters<MergeArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(
            match self
                .merge_branch(&args.repo, &args.source, args.mode, args.message.as_deref())
                .await
            {
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
        description = "Discard working-tree changes for repository-relative paths. This is destructive and must be explicitly requested by the caller."
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
        description = "Create a Conventional Commit from the current index. Normal commits on protected main are rejected; completing an explicit merge is allowed."
    )]
    async fn git_commit(
        &self,
        Parameters(args): Parameters<CommitArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_writable() {
            return Ok(Self::failure(error));
        }
        if let Err(error) = validate_conventional_commit_message(&args.message) {
            return Ok(Self::failure(error));
        }
        let branch = match self.current_branch_required(&args.repo).await {
            Ok(branch) => branch,
            Err(error) => return Ok(Self::failure(error)),
        };
        if branch == PROTECTED_TRUNK {
            match self.merge_in_progress(&args.repo).await {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(Self::failure(
                        "normal commits are not allowed directly on protected branch `main`; create/switch to a work branch first",
                    ));
                }
                Err(error) => return Ok(Self::failure(error)),
            }
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
        description = "Create a lightweight or annotated Git tag. For dev-git-control releases prefer git_release_tag."
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
        description = "Create an annotated vMAJOR.MINOR.PATCH release tag only on a clean two-parent main merge, enforcing exact PASS release evidence for enrolled repositories."
    )]
    async fn git_release_tag(
        &self,
        Parameters(args): Parameters<ReleaseTagArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.create_release_tag(&args).await {
            Ok(output) => self.success(output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Fetch and prune refs from a configured remote. Requires local writes plus remote-read permission."
    )]
    async fn git_fetch(
        &self,
        Parameters(args): Parameters<RemoteArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .ensure_writable()
            .and_then(|_| self.ensure_remote_read())
        {
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
        description = "Fast-forward-only pull. Requires local writes plus remote-read permission."
    )]
    async fn git_pull(
        &self,
        Parameters(args): Parameters<PullArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .ensure_writable()
            .and_then(|_| self.ensure_remote_read())
        {
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
        description = "Push a ref to a remote without force. Requires explicit remote-write permission; force push is never exposed."
    )]
    async fn git_push(
        &self,
        Parameters(args): Parameters<PushArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self.ensure_remote_write() {
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

fn validate_conventional_commit_message(message: &str) -> std::result::Result<(), String> {
    validate_commit_message(message)?;
    let subject = message.lines().next().unwrap_or_default();
    if !is_conventional_commit_subject(subject) {
        return Err("commit subject must follow Conventional Commits 1.0.0".to_owned());
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

fn validate_semver_core(version: &str) -> std::result::Result<(), String> {
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
        || parts
            .iter()
            .any(|part| part.len() > 1 && part.starts_with('0'))
    {
        return Err(
            "version must be plain MAJOR.MINOR.PATCH with numeric components and no leading zeroes"
                .to_owned(),
        );
    }
    Ok(())
}

fn expected_release_subject(version: &str) -> String {
    format!("chore(release): prepare v{version}")
}

fn is_release_commit_subject(subject: &str) -> bool {
    let Some(version) = subject.strip_prefix("chore(release): prepare v") else {
        return false;
    };
    validate_semver_core(version).is_ok()
}

fn conventional_merge_message(source: &str) -> String {
    let kind = if source.starts_with("feature/") {
        "feat"
    } else if source.starts_with("fix/") {
        "fix"
    } else if source.starts_with("docs/") {
        "docs"
    } else if source.starts_with("test/") {
        "test"
    } else if source.starts_with("refactor/") {
        "refactor"
    } else if source.starts_with("build/") {
        "build"
    } else if source.starts_with("ci/") {
        "ci"
    } else if source.starts_with("perf/") {
        "perf"
    } else {
        "chore"
    };
    format!("{kind}: merge {source}")
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
        allow_remote_read = server.allow_remote_read,
        allow_remote_write = server.allow_remote_write,
        "starting multi-repository Git MCP server"
    );
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Output};

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
            git_ok(&repo, ["init", "-q", "-b", PROTECTED_TRUNK]);
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
                allow_remote_read: false,
                allow_remote_write: false,
                max_output_bytes: 1_048_576,
                release_evidence_root: None,
                release_evidence_required_repos: BTreeSet::new(),
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
    fn validates_paths_tokens_conventional_and_release_versions() {
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
        assert!(validate_semver_core("0.12.0").is_ok());
        assert!(validate_semver_core("01.2.3").is_err());
        assert!(is_release_commit_subject("chore(release): prepare v0.12.0"));
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
        git_ok(&outside, ["init", "-q", "-b", PROTECTED_TRUNK]);
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
        let plan = server.history_plan("repo", PROTECTED_TRUNK).await.unwrap();
        assert_eq!(plan.branch, "feature/history");
        assert_eq!(plan.commit_count, 2);
        assert!(!plan.published);
        assert!(plan.can_rewrite);
        assert!(plan.commits[0].conventional);
        assert!(!plan.commits[1].conventional);

        fs::write(workspace.repo.join("dirty.txt"), "dirty\n").unwrap();
        let dirty = server.history_plan("repo", PROTECTED_TRUNK).await.unwrap();
        assert!(dirty.dirty);
        assert!(!dirty.can_rewrite);
    }

    #[tokio::test]
    async fn merge_readiness_enforces_dev_control_release_policy() {
        let workspace = TestWorkspace::new();
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/readiness"]);
        for index in 0..4 {
            workspace.commit_file(
                &format!("{index}.txt"),
                &format!("{index}\n"),
                &format!("feat: commit {index}"),
            );
        }
        workspace.commit_file(
            "release.txt",
            "release\n",
            "chore(release): prepare v0.12.0",
        );
        let server = workspace.server(false);
        let ready = server
            .dev_control_readiness(&DevControlReadinessArgs {
                repo: "repo".to_owned(),
                version: "0.12.0".to_owned(),
            })
            .await
            .unwrap();
        assert!(ready.ready);

        workspace.commit_file("six.txt", "six\n", "fix: after release");
        let blocked = server
            .dev_control_readiness(&DevControlReadinessArgs {
                repo: "repo".to_owned(),
                version: "0.12.0".to_owned(),
            })
            .await
            .unwrap();
        assert!(!blocked.ready);
        assert!(!blocked.checks.commit_limit);
        assert!(!blocked.checks.release_commit_last);
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
        workspace.commit_file("one.txt", "one\n", "feat: one");
        let one = workspace.head();
        workspace.commit_file("two.txt", "two\n", "feat: two");
        workspace.commit_file("three.txt", "three\n", "feat: three");
        let three = workspace.head();
        let tree_before = workspace.tree();
        let server = workspace.server(false);
        let output = server
            .reword_commits(&RewordCommitsArgs {
                repo: "repo".to_owned(),
                base: PROTECTED_TRUNK.to_owned(),
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
                base: PROTECTED_TRUNK.to_owned(),
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
                base: PROTECTED_TRUNK.to_owned(),
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
    async fn local_only_allows_unpublished_suffix_but_blocks_upstream_commit() {
        let workspace = TestWorkspace::new();
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/published"]);
        workspace.commit_file("one.txt", "one\n", "feat: one");
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
        workspace.commit_file("two.txt", "two\n", "feat: two");
        let local = workspace.head();
        let server = workspace.server(false);

        let allowed = server
            .reword_commits(&RewordCommitsArgs {
                repo: "repo".to_owned(),
                base: PROTECTED_TRUNK.to_owned(),
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
                base: PROTECTED_TRUNK.to_owned(),
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
    async fn normal_commit_on_main_is_rejected() {
        let workspace = TestWorkspace::new();
        let server = workspace.server(false);
        fs::write(workspace.repo.join("new.txt"), "new\n").unwrap();
        git_ok(&workspace.repo, ["add", "--", "new.txt"]);
        let before = workspace.head();
        let branch = server.current_branch_required("repo").await.unwrap();
        assert_eq!(branch, PROTECTED_TRUNK);
        assert!(!server.merge_in_progress("repo").await.unwrap());
        assert_eq!(workspace.head(), before);
    }

    #[tokio::test]
    async fn switch_create_merge_and_delete_branch_use_no_ff_and_conventional_merge_message() {
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
        server.switch_branch("repo", PROTECTED_TRUNK).await.unwrap();
        let result = server
            .merge_branch("repo", "feature/no-ff", MergeMode::NoFf, None)
            .await
            .unwrap();
        assert_eq!(result.branch, PROTECTED_TRUNK);
        assert_ne!(result.head, feature_head);
        let parents = git_string(
            &workspace.repo,
            ["rev-list", "--parents", "-n", "1", "HEAD"],
        );
        assert_eq!(parents.split_whitespace().count(), 3);
        assert_eq!(
            git_string(&workspace.repo, ["log", "-1", "--pretty=%s"]),
            "feat: merge feature/no-ff"
        );
        server.delete_branch("repo", "feature/no-ff").await.unwrap();
    }

    #[tokio::test]
    async fn guarded_rewind_moves_only_expected_merge_to_first_parent() {
        let workspace = TestWorkspace::new();
        let server = workspace.server(false);
        let base = workspace.head();
        server
            .create_branch("repo", "feature/rewind", None, true)
            .await
            .unwrap();
        workspace.commit_file("feature.txt", "feature\n", "feat: rewind fixture");
        server.switch_branch("repo", PROTECTED_TRUNK).await.unwrap();
        let merged = server
            .merge_branch("repo", "feature/rewind", MergeMode::NoFf, None)
            .await
            .unwrap();
        let merge_head = merged.head.clone();
        let output = server
            .rewind_merge(&RewindMergeArgs {
                repo: "repo".to_owned(),
                expected_merge: merge_head,
                remote: None,
                publication_guard: PublicationGuard::CallerConfirmedUnpublished,
            })
            .await
            .unwrap();
        assert_eq!(output.first_parent, base);
        assert_eq!(workspace.head(), base);
        assert_eq!(git_string(&workspace.repo, ["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn guarded_tag_replacement_is_atomic_and_annotated() {
        let workspace = TestWorkspace::new();
        let server = workspace.server(false);
        let old_target = workspace.head();
        git_ok(
            &workspace.repo,
            ["tag", "-a", "v0.1.0", "-m", "Release v0.1.0"],
        );
        git_ok(&workspace.repo, ["switch", "-q", "-c", "feature/tag"]);
        workspace.commit_file("next.txt", "next\n", "feat: next");
        let new_target = workspace.head();
        let output = server
            .replace_local_tag(&ReplaceLocalTagArgs {
                repo: "repo".to_owned(),
                name: "v0.1.0".to_owned(),
                expected_old_target: old_target,
                new_target: Some(new_target.clone()),
                message: "Release v0.1.0".to_owned(),
                remote: None,
                publication_guard: PublicationGuard::CallerConfirmedUnpublished,
            })
            .await
            .unwrap();
        assert_eq!(output.new_target, new_target);
        assert!(output.annotated);
        assert_eq!(
            git_string(&workspace.repo, ["rev-parse", "v0.1.0^{commit}"]),
            new_target
        );
    }

    #[tokio::test]
    async fn release_tag_requires_clean_main_merge_head() {
        let workspace = TestWorkspace::new();
        let server = workspace.server(false);
        server
            .create_branch("repo", "feature/release", None, true)
            .await
            .unwrap();
        workspace.commit_file("feature.txt", "feature\n", "feat: release fixture");
        server.switch_branch("repo", PROTECTED_TRUNK).await.unwrap();
        server
            .merge_branch("repo", "feature/release", MergeMode::NoFf, None)
            .await
            .unwrap();
        server
            .create_release_tag(&ReleaseTagArgs {
                repo: "repo".to_owned(),
                version: "1.0.0".to_owned(),
                message: None,
                release_evidence_path: None,
            })
            .await
            .unwrap();
        assert_eq!(
            git_string(&workspace.repo, ["rev-parse", "v1.0.0^{commit}"]),
            workspace.head()
        );
    }

    #[tokio::test]
    async fn release_tag_enforcement_requires_exact_pass_evidence() {
        let workspace = TestWorkspace::new();
        let mut server = workspace.server(false);
        server
            .create_branch("repo", "feature/evidence", None, true)
            .await
            .unwrap();
        workspace.commit_file("feature.txt", "feature\n", "feat: evidence fixture");
        server.switch_branch("repo", PROTECTED_TRUNK).await.unwrap();
        server
            .merge_branch("repo", "feature/evidence", MergeMode::NoFf, None)
            .await
            .unwrap();

        let evidence_root = workspace.root.join("evidence");
        fs::create_dir_all(evidence_root.join("repo-project")).unwrap();
        server.release_evidence_root = Some(fs::canonicalize(&evidence_root).unwrap());
        server
            .release_evidence_required_repos
            .insert("repo".to_owned());

        let missing = server
            .create_release_tag(&ReleaseTagArgs {
                repo: "repo".to_owned(),
                version: "1.1.0".to_owned(),
                message: None,
                release_evidence_path: None,
            })
            .await;
        assert!(missing.is_err());

        let head = workspace.head();
        let relative = format!("repo-project/{head}.json");
        let evidence_path = evidence_root.join(&relative);
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1,
                "gate": "sonarqube-main",
                "status": "PASS",
                "repo": fs::canonicalize(&workspace.repo).unwrap().to_string_lossy(),
                "project_key": "repo-project",
                "commit": head,
                "branch": "main",
                "analysis_id": "analysis-1"
            }))
            .unwrap(),
        )
        .unwrap();

        server
            .create_release_tag(&ReleaseTagArgs {
                repo: "repo".to_owned(),
                version: "1.1.0".to_owned(),
                message: None,
                release_evidence_path: Some(relative),
            })
            .await
            .unwrap();
        assert_eq!(
            git_string(&workspace.repo, ["rev-parse", "v1.1.0^{commit}"]),
            workspace.head()
        );
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
                    include_index: false
                })
                .await
                .is_err()
        );
    }
}
