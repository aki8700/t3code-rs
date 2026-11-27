use std::{
    collections::HashMap,
    ffi::OsStr,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Output, Stdio},
};

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use t3_core::{CoreError, CoreResult, DiffFile, DiffSummary, classify_diff_risk};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

const PATCH_LIMIT: usize = 200 * 1024;
const MAX_TOTAL_PATCH_BYTES: usize = 2 * 1024 * 1024;
const PATCH_TRUNCATED_MARKER: &str = "\n... [truncated]";
const BUDGET_TRUNCATED_MARKER: &str = "[truncated: total diff patch budget exceeded]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged {
        sha: String,
        summary: String,
    },
    Conflict {
        conflict_files: Vec<String>,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct GitManager {
    worktrees_root: PathBuf,
}

impl GitManager {
    pub fn new(worktrees_root: PathBuf) -> Self {
        Self { worktrees_root }
    }

    pub async fn is_git_repo(dir: &Path) -> bool {
        git_output(dir, &["rev-parse", "--is-inside-work-tree"])
            .await
            .is_ok_and(|output| output == b"true\n")
    }

    pub async fn current_branch(dir: &Path) -> CoreResult<Option<String>> {
        let output = git_output(dir, &["branch", "--show-current"]).await?;
        let name = output.strip_suffix(b"\n").unwrap_or(&output);
        Ok((!name.is_empty()).then(|| String::from_utf8_lossy(name).into_owned()))
    }

    pub async fn list_branches(dir: &Path) -> CoreResult<Vec<String>> {
        let output = git_output(dir, &["branch", "--format=%(refname:short)"]).await?;
        Ok(String::from_utf8_lossy(&output)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// Write the current worktree state (including non-ignored untracked files) to a tree.
    /// The temporary index keeps the repository's real index untouched.
    pub async fn snapshot_tree(dir: &Path) -> CoreResult<Option<String>> {
        if !Self::is_git_repo(dir).await {
            return Ok(None);
        }
        let root_output = git_output(dir, &["rev-parse", "--show-toplevel"]).await?;
        let root = git_path(root_output.strip_suffix(b"\n").unwrap_or(&root_output));
        let filters = disabled_filters(&root).await?;
        let index = std::env::temp_dir().join(format!("t3-git-index-{}", Uuid::new_v4()));
        let result = async {
            let head = git(&root)
                .args(["rev-parse", "--verify", "--quiet", "HEAD"])
                .output()
                .await
                .map_err(git_error)?;
            let mut command = git(&root);
            command.env("GIT_INDEX_FILE", &index);
            if head.status.success() {
                command.args(["read-tree", "HEAD"]);
            } else if head.status.code() == Some(1) {
                command.args(["read-tree", "--empty"]);
            } else {
                return Err(command_error(&head));
            }
            checked_output(command, false).await?;

            let mut add = git_with_filters(&root, &filters);
            add.env("GIT_INDEX_FILE", &index)
                .args(["--literal-pathspecs", "add", "-A", "--", "."]);
            checked_output(add, false).await?;

            let mut write = git(&root);
            write.env("GIT_INDEX_FILE", &index).arg("write-tree");
            let tree = checked_output(write, false).await?;
            Ok(String::from_utf8_lossy(&tree).trim().to_owned())
        }
        .await;
        let _ = tokio::fs::remove_file(&index).await;
        let _ = tokio::fs::remove_file(index.with_extension("lock")).await;
        result.map(Some)
    }

    pub async fn restore_tree(dir: &Path, tree_sha: &str) -> CoreResult<()> {
        let root_output = git_output(dir, &["rev-parse", "--show-toplevel"]).await?;
        let root = git_path(root_output.strip_suffix(b"\n").unwrap_or(&root_output));
        let verified = git_output(
            &root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{tree_sha}^{{tree}}"),
            ],
        )
        .await?;
        let tree = String::from_utf8_lossy(&verified).trim().to_owned();
        let filters = disabled_filters(&root).await?;
        let index = std::env::temp_dir().join(format!("t3-git-index-{}", Uuid::new_v4()));
        let result = async {
            // Give clean an index containing the snapshot, so untracked files that belong
            // to the snapshot survive while files created afterwards are removed.
            let mut read = git(&root);
            read.env("GIT_INDEX_FILE", &index)
                .args(["read-tree", &tree]);
            checked_output(read, false).await?;
            let mut clean = git_with_filters(&root, &filters);
            clean.env("GIT_INDEX_FILE", &index).args(["clean", "-fd"]);
            checked_output(clean, false).await?;
            let mut restore = git_with_filters(&root, &filters);
            restore.args(["read-tree", "--reset", "-u", &tree]);
            checked_output(restore, false).await?;
            Ok(())
        }
        .await;
        let _ = tokio::fs::remove_file(&index).await;
        let _ = tokio::fs::remove_file(index.with_extension("lock")).await;
        result
    }

    pub async fn stage_file(dir: &Path, rel_path: &str) -> CoreResult<()> {
        validate_relative_path(rel_path)?;
        let mut command = git(dir);
        command.args(["--literal-pathspecs", "add", "-A", "--", rel_path]);
        checked_output(command, false).await?;
        Ok(())
    }

    pub async fn discard_file(dir: &Path, rel_path: &str) -> CoreResult<()> {
        validate_relative_path(rel_path)?;
        let root_output = git_output(dir, &["rev-parse", "--show-toplevel"]).await?;
        let root = git_path(root_output.strip_suffix(b"\n").unwrap_or(&root_output));
        let in_head = git(&root)
            .args([
                "--literal-pathspecs",
                "ls-tree",
                "-r",
                "-z",
                "--name-only",
                "HEAD",
                "--",
                rel_path,
            ])
            .output()
            .await
            .map_err(git_error)?;
        let tracked = in_head.status.success()
            && nul_fields(&in_head.stdout).any(|path| path == rel_path.as_bytes());
        if tracked {
            let mut command = git(&root);
            command.args(["--literal-pathspecs", "checkout", "HEAD", "--", rel_path]);
            checked_output(command, false).await?;
        } else {
            let path = root.join(rel_path);
            ensure_inside_repo(&root, &path)?;
            match tokio::fs::symlink_metadata(&path).await {
                Ok(metadata) if metadata.is_dir() => {
                    tokio::fs::remove_dir_all(path).await.map_err(git_error)?;
                }
                Ok(_) => tokio::fs::remove_file(path).await.map_err(git_error)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(git_error(error)),
            }
        }
        Ok(())
    }

    pub async fn commit(dir: &Path, message: &str) -> CoreResult<String> {
        if message.trim().is_empty() {
            return Err(git_error("commit message must not be empty"));
        }
        let staged = git(dir)
            .args(["diff", "--cached", "--quiet", "--exit-code"])
            .output()
            .await
            .map_err(git_error)?;
        if staged.status.success() {
            let mut add = git(dir);
            add.args(["--literal-pathspecs", "add", "-A", "--", "."]);
            checked_output(add, false).await?;
        } else if staged.status.code() != Some(1) {
            return Err(command_error(&staged));
        }
        let mut command = git(dir);
        command.args(["commit", "-m", message]);
        checked_output(command, false).await?;
        let sha = git_output(dir, &["rev-parse", "HEAD"]).await?;
        Ok(String::from_utf8_lossy(&sha).trim().to_owned())
    }

    pub async fn has_merge_in_progress(workspace: &Path) -> CoreResult<bool> {
        let output = git(workspace)
            .args(["rev-parse", "--verify", "-q", "MERGE_HEAD"])
            .output()
            .await
            .map_err(git_error)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(command_error(&output)),
        }
    }

    pub async fn has_uncommitted_changes(workspace: &Path) -> CoreResult<bool> {
        let filters = disabled_filters(workspace).await?;
        let mut command = git_with_filters(workspace, &filters);
        command.args(["status", "--porcelain"]);
        let output = checked_output(command, false).await?;
        Ok(!output.trim_ascii().is_empty())
    }

    pub async fn merge_branch(repo_root: &Path, branch: &str) -> CoreResult<String> {
        validate_branch(repo_root, branch).await?;
        if Self::has_merge_in_progress(repo_root).await? {
            return Err(git_error("a merge is already in progress"));
        }
        let mut command = git(repo_root);
        command.args(["merge", "--no-edit", "--", branch]);
        if let Err(error) = checked_output(command, false).await {
            if Self::has_merge_in_progress(repo_root)
                .await
                .unwrap_or(false)
            {
                let mut abort = git(repo_root);
                abort.args(["merge", "--abort"]);
                let _ = checked_output(abort, false).await;
            }
            return Err(error);
        }
        let sha = git_output(repo_root, &["rev-parse", "HEAD"]).await?;
        Ok(String::from_utf8_lossy(&sha).trim().to_owned())
    }

    pub async fn merge_branch_checked(
        repo_root: &Path,
        branch: &str,
        commit_message: Option<&str>,
    ) -> CoreResult<MergeOutcome> {
        validate_branch(repo_root, branch).await?;
        if Self::has_merge_in_progress(repo_root).await? {
            return Err(git_error("a merge is already in progress"));
        }
        let mut command = git(repo_root);
        command.args(["merge", "--no-edit"]);
        if let Some(message) = commit_message
            .map(str::trim)
            .filter(|message| !message.is_empty())
        {
            command.args(["--no-ff", "-m", message]);
        }
        command.args(["--", branch]);
        let output = command.output().await.map_err(git_error)?;
        if output.status.success() {
            let sha = git_output(repo_root, &["rev-parse", "HEAD"]).await?;
            let sha = String::from_utf8_lossy(&sha).trim().to_owned();
            let summary = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            return Ok(MergeOutcome::Merged { sha, summary });
        }
        if Self::has_merge_in_progress(repo_root)
            .await
            .unwrap_or(false)
        {
            let conflicts =
                git_output(repo_root, &["diff", "--name-only", "--diff-filter=U", "-z"])
                    .await
                    .unwrap_or_default();
            let conflict_files = nul_fields(&conflicts)
                .filter(|field| !field.is_empty())
                .map(|field| String::from_utf8_lossy(field).into_owned())
                .collect::<Vec<_>>();
            let mut abort = git(repo_root);
            abort.args(["merge", "--abort"]);
            let _ = checked_output(abort, false).await;
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let message = match (stdout.is_empty(), stderr.is_empty()) {
                (false, false) => format!("{stdout}\n{stderr}"),
                (false, true) => stdout,
                (true, false) => stderr,
                (true, true) => "merge conflict".to_owned(),
            };
            return Ok(MergeOutcome::Conflict {
                conflict_files,
                message,
            });
        }
        Err(command_error(&output))
    }

    pub async fn delete_branch(repo_root: &Path, branch: &str) -> CoreResult<()> {
        validate_branch(repo_root, branch).await?;
        let mut command = git(repo_root);
        command.args(["branch", "-D", "--", branch]);
        checked_output(command, false).await?;
        Ok(())
    }

    pub async fn prune_worktrees(&self, repo_root: &Path) -> CoreResult<()> {
        let mut command = git(repo_root);
        command.args(["worktree", "prune"]);
        checked_output(command, false).await?;
        Ok(())
    }

    pub async fn create_worktree(
        &self,
        repo_root: &Path,
        branch_name: &str,
        base_ref: Option<&str>,
    ) -> CoreResult<(PathBuf, String)> {
        let root = std::path::absolute(&self.worktrees_root).map_err(git_error)?;
        let mut dir = tokio::fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        dir.mode(0o700);
        dir.create(&root).await.map_err(git_error)?;
        let stem = ref_safe_name(branch_name);
        let suffix = Uuid::new_v4().simple().to_string();
        let name = format!("{stem}-{}", &suffix[..12]);
        let branch = format!("t3rs/{name}");
        let path = root.join(name);
        let filters = disabled_filters(repo_root).await?;
        let mut command = git_with_filters(repo_root, &filters);
        command
            .args(["worktree", "add", "-b", &branch, "--"])
            .arg(&path);
        if let Some(base_ref) = base_ref {
            let commit = git_output(
                repo_root,
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{base_ref}^{{commit}}"),
                ],
            )
            .await?;
            command.arg(String::from_utf8_lossy(&commit).trim());
        }
        checked_output(command, false).await?;
        // The worktree is usable without the local files, so a copy failure is not fatal.
        if let Err(error) = copy_worktree_includes(repo_root, &path).await {
            tracing::warn!(%error, "Failed to apply .worktreeinclude");
        }
        tracing::debug!("Created Git worktree");
        Ok((path, branch))
    }

    /// Remove a worktree without losing its uncommitted work. Tracked edits and untracked,
    /// non-ignored files are first committed (on top of the worktree's HEAD, so the branch's
    /// commits stay reachable even if the branch is deleted later) and pinned in the main
    /// repository as `refs/t3/snapshots/<branch>-<UTC time>`; that ref is returned. A clean
    /// worktree gets no ref. When the snapshot cannot be written the worktree is kept and the
    /// error returned, because `--force` would otherwise destroy the changes.
    ///
    /// To restore: `git for-each-ref refs/t3/snapshots` lists them, `git show --stat <ref>`
    /// shows one, and `git worktree add -b <new-branch> <dir> <ref>^` followed by
    /// `git -C <dir> restore --source=<ref> --worktree -- .` brings the files back as
    /// uncommitted changes. `git update-ref -d <ref>` drops a snapshot.
    pub async fn remove_worktree(
        &self,
        repo_root: &Path,
        worktree_path: &Path,
    ) -> CoreResult<Option<String>> {
        let snapshot = snapshot_worktree(repo_root, worktree_path).await?;
        if let Some(snapshot) = &snapshot {
            tracing::info!(%snapshot, path = %worktree_path.display(), "Saved worktree changes before removal");
        }
        let mut command = git(repo_root);
        command
            .args(["worktree", "remove", "--force", "--"])
            .arg(worktree_path);
        checked_output(command, false).await?;
        tracing::debug!("Removed Git worktree");
        Ok(snapshot)
    }

    pub async fn diff_summary(dir: &Path, ignore_whitespace: bool) -> CoreResult<DiffSummary> {
        Self::diff_summary_with_budget(dir, ignore_whitespace, MAX_TOTAL_PATCH_BYTES).await
    }

    async fn diff_summary_with_budget(
        dir: &Path,
        ignore_whitespace: bool,
        max_total_patch_bytes: usize,
    ) -> CoreResult<DiffSummary> {
        let root = git_output(dir, &["rev-parse", "--show-toplevel"]).await?;
        let root = git_path(root.strip_suffix(b"\n").unwrap_or(&root));
        let branch = Self::current_branch(&root).await?;
        let filters = disabled_filters(&root).await?;
        let mut status = git_with_filters(&root, &filters);
        status.args(["status", "--porcelain=v1", "-z", "--untracked-files=all"]);
        let status = checked_output(status, false).await?;
        let entries = status_entries(&status)?;
        let base = diff_base(&root, &entries).await?;

        let mut remaining_patch_bytes = max_total_patch_bytes;
        let mut files = Vec::with_capacity(entries.len());
        for entry in entries {
            let path = git_path(entry.path);
            let diff = FileDiff {
                root: &root,
                filters: &filters,
                path: &path,
                base: base.as_deref(),
                rename_from: entry.rename_from.map(git_path),
                untracked: entry.status == b"??",
                ignore_whitespace,
            };
            let (additions, deletions, patch) =
                if remaining_patch_bytes < PATCH_TRUNCATED_MARKER.len() {
                    let numstat_raw = diff.output("--numstat").await?;
                    if numstat_raw.is_empty() && ignore_whitespace && !diff.untracked {
                        continue;
                    }
                    let (additions, deletions) = numstat(&numstat_raw)?;
                    (additions, deletions, BUDGET_TRUNCATED_MARKER.to_owned())
                } else {
                    let file_limit = PATCH_LIMIT.min(remaining_patch_bytes);
                    let (raw_patch, truncated) = diff.bounded_output("--patch", file_limit).await?;
                    if raw_patch.is_empty() && ignore_whitespace && !diff.untracked {
                        continue;
                    }
                    let (additions, deletions) = numstat(&diff.output("--numstat").await?)?;
                    let patch = format_patch(&raw_patch, file_limit, truncated);
                    let consumed = if truncated { file_limit } else { patch.len() };
                    remaining_patch_bytes = remaining_patch_bytes.saturating_sub(consumed);
                    (additions, deletions, patch)
                };
            files.push(DiffFile {
                path: path.to_string_lossy().into_owned(),
                status: String::from_utf8_lossy(entry.status).into_owned(),
                additions,
                deletions,
                patch,
            });
        }

        let risk = Some(classify_diff_risk(&files));
        Ok(DiffSummary {
            workspace_path: dir.to_owned(),
            branch,
            clean: files.is_empty(),
            total_additions: files.iter().map(|file| file.additions).sum(),
            total_deletions: files.iter().map(|file| file.deletions).sum(),
            files,
            risk,
        })
    }

    pub async fn branch_diff_summary(
        workspace: &Path,
        base_ref: &str,
        head_ref: &str,
        ignore_whitespace: bool,
    ) -> CoreResult<DiffSummary> {
        Self::branch_diff_summary_with_budget(
            workspace,
            base_ref,
            head_ref,
            ignore_whitespace,
            MAX_TOTAL_PATCH_BYTES,
        )
        .await
    }

    async fn branch_diff_summary_with_budget(
        workspace: &Path,
        base_ref: &str,
        head_ref: &str,
        ignore_whitespace: bool,
        max_total_patch_bytes: usize,
    ) -> CoreResult<DiffSummary> {
        let root = git_output(workspace, &["rev-parse", "--show-toplevel"]).await?;
        let root = git_path(root.strip_suffix(b"\n").unwrap_or(&root));
        let base_sha = resolve_commit(&root, base_ref).await?;
        let head_sha = resolve_commit(&root, head_ref).await?;
        let range = format!("{base_sha}...{head_sha}");
        let filters = disabled_filters(&root).await?;

        let mut name_status_cmd = git_with_filters(&root, &filters);
        name_status_cmd.args([
            "--literal-pathspecs",
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
        ]);
        if ignore_whitespace {
            name_status_cmd.arg("-w");
        }
        name_status_cmd.args([&range, "--"]);
        let name_status_raw = checked_output(name_status_cmd, false).await?;
        let entries = parse_name_status(&name_status_raw)?;

        let mut numstat_cmd = git_with_filters(&root, &filters);
        numstat_cmd.args([
            "--literal-pathspecs",
            "diff",
            "--numstat",
            "-z",
            "--find-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
        ]);
        if ignore_whitespace {
            numstat_cmd.arg("-w");
        }
        numstat_cmd.args([&range, "--"]);
        let numstat_raw = checked_output(numstat_cmd, false).await?;
        let stats = parse_numstat_map(&numstat_raw)?;

        let mut remaining_patch_bytes = max_total_patch_bytes;
        let mut files = Vec::with_capacity(entries.len());
        for entry in entries {
            let (additions, deletions) = stats.get(&entry.path).copied().unwrap_or((0, 0));
            let path = git_path(&entry.path);
            let patch = if remaining_patch_bytes < PATCH_TRUNCATED_MARKER.len() {
                if ignore_whitespace && additions == 0 && deletions == 0 {
                    continue;
                }
                BUDGET_TRUNCATED_MARKER.to_owned()
            } else {
                let mut patch_cmd = git_with_filters(&root, &filters);
                patch_cmd.args([
                    "--literal-pathspecs",
                    "diff",
                    "--patch",
                    "--find-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                ]);
                if ignore_whitespace {
                    patch_cmd.arg("-w");
                }
                patch_cmd.arg(&range).arg("--").arg(&path);
                if let Some(source) = &entry.rename_from {
                    patch_cmd.arg(git_path(source));
                }
                let file_limit = PATCH_LIMIT.min(remaining_patch_bytes);
                let (raw_patch, truncated) =
                    checked_output_bounded(patch_cmd, false, file_limit).await?;
                if raw_patch.is_empty() && ignore_whitespace {
                    continue;
                }
                let patch = format_patch(&raw_patch, file_limit, truncated);
                let consumed = if truncated { file_limit } else { patch.len() };
                remaining_patch_bytes = remaining_patch_bytes.saturating_sub(consumed);
                patch
            };
            files.push(DiffFile {
                path: path.to_string_lossy().into_owned(),
                status: entry.status,
                additions,
                deletions,
                patch,
            });
        }

        let risk = Some(classify_diff_risk(&files));
        Ok(DiffSummary {
            workspace_path: workspace.to_owned(),
            branch: Some(head_ref.to_owned()),
            clean: files.is_empty(),
            total_additions: files.iter().map(|file| file.additions).sum(),
            total_deletions: files.iter().map(|file| file.deletions).sum(),
            files,
            risk,
        })
    }
}

struct StatusEntry<'a> {
    status: &'a [u8],
    path: &'a [u8],
    rename_from: Option<&'a [u8]>,
}

async fn diff_base(root: &Path, entries: &[StatusEntry<'_>]) -> CoreResult<Option<String>> {
    let head = git(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .await
        .map_err(git_error)?;
    match head.status.code() {
        Some(0) => Ok(Some("HEAD".to_owned())),
        // An empty tree includes files staged before the first commit.
        Some(1)
            if entries
                .iter()
                .any(|entry| !matches!(entry.status[0], b' ' | b'?')) =>
        {
            let empty_tree = git_output(root, &["hash-object", "-t", "tree", "--stdin"]).await?;
            Ok(Some(String::from_utf8_lossy(&empty_tree).trim().to_owned()))
        }
        Some(1) => Ok(None),
        _ => Err(command_error(&head)),
    }
}

fn nul_fields(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split_inclusive(|byte| *byte == 0)
        .map(|field| field.strip_suffix(b"\0").unwrap_or(field))
}

fn status_entries(status: &[u8]) -> CoreResult<Vec<StatusEntry<'_>>> {
    let mut fields = nul_fields(status);
    let mut entries = Vec::new();
    while let Some(entry) = fields.next() {
        if entry.len() < 4 || entry[2] != b' ' {
            return Err(git_error("invalid Git porcelain status entry"));
        }
        let status = &entry[..2];
        let rename_from = if status.iter().any(|byte| matches!(byte, b'R' | b'C')) {
            let source = fields
                .next()
                .filter(|source| !source.is_empty())
                .ok_or_else(|| git_error("missing source path in Git porcelain status"))?;
            // A copied destination is new relative to the base tree; its source has its own diff.
            status.contains(&b'R').then_some(source)
        } else {
            None
        };
        entries.push(StatusEntry {
            status,
            path: &entry[3..],
            rename_from,
        });
    }
    Ok(entries)
}

struct FileDiff<'a> {
    root: &'a Path,
    filters: &'a [String],
    path: &'a Path,
    base: Option<&'a str>,
    rename_from: Option<PathBuf>,
    untracked: bool,
    ignore_whitespace: bool,
}

impl FileDiff<'_> {
    fn command(&self, format: &str) -> Command {
        let mut command = git_with_filters(self.root, self.filters);
        command.args([
            "--literal-pathspecs",
            "diff",
            format,
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
        ]);
        if self.untracked {
            // Git counts text lines and represents binary files and symlinks itself.
            // Prefix relative paths so Git treats a file named "-" as a file.
            command
                .args(["--no-index", "--", "/dev/null"])
                .arg(Path::new(".").join(self.path));
        } else {
            if self.ignore_whitespace {
                command.arg("-w");
            }
            command.arg("--find-renames");
            if let Some(base) = self.base {
                command.arg(base);
            }
            command.arg("--").arg(self.path);
            if let Some(source) = &self.rename_from {
                command.arg(source);
            }
        }
        command
    }

    async fn output(&self, format: &str) -> CoreResult<Vec<u8>> {
        checked_output(self.command(format), self.untracked).await
    }

    async fn bounded_output(&self, format: &str, max_bytes: usize) -> CoreResult<(Vec<u8>, bool)> {
        checked_output_bounded(self.command(format), self.untracked, max_bytes).await
    }
}

struct BranchDiffEntry {
    status: String,
    path: Vec<u8>,
    rename_from: Option<Vec<u8>>,
}

fn parse_name_status(bytes: &[u8]) -> CoreResult<Vec<BranchDiffEntry>> {
    let mut fields = nul_fields(bytes);
    let mut entries = Vec::new();
    while let Some(status_bytes) = fields.next() {
        if status_bytes.is_empty() {
            continue;
        }
        let status = String::from_utf8_lossy(status_bytes).into_owned();
        if matches!(status_bytes[0], b'R' | b'C') {
            let old_path = fields
                .next()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| git_error("missing source path in Git diff name-status"))?;
            let new_path = fields
                .next()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| git_error("missing destination path in Git diff name-status"))?;
            entries.push(BranchDiffEntry {
                status,
                path: new_path.to_vec(),
                rename_from: Some(old_path.to_vec()),
            });
        } else {
            let path = fields
                .next()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| git_error("missing path in Git diff name-status"))?;
            entries.push(BranchDiffEntry {
                status,
                path: path.to_vec(),
                rename_from: None,
            });
        }
    }
    Ok(entries)
}

fn parse_numstat_map(bytes: &[u8]) -> CoreResult<HashMap<Vec<u8>, (usize, usize)>> {
    let mut fields = nul_fields(bytes);
    let mut stats = HashMap::new();
    while let Some(entry) = fields.next() {
        if entry.is_empty() {
            continue;
        }
        let mut columns = entry.splitn(3, |byte| *byte == b'\t');
        let (Some(added), Some(deleted), Some(path)) =
            (columns.next(), columns.next(), columns.next())
        else {
            return Err(git_error("invalid Git numstat entry"));
        };
        let additions = line_count(added)?;
        let deletions = line_count(deleted)?;
        let key = if path.is_empty() {
            let _old = fields
                .next()
                .ok_or_else(|| git_error("missing source path in Git numstat rename"))?;
            let new = fields
                .next()
                .ok_or_else(|| git_error("missing destination path in Git numstat rename"))?;
            new.to_vec()
        } else {
            path.to_vec()
        };
        stats.insert(key, (additions, deletions));
    }
    Ok(stats)
}

async fn resolve_commit(repo_root: &Path, rev: &str) -> CoreResult<String> {
    if rev.trim().is_empty() {
        return Err(git_error("git ref must not be empty"));
    }
    let commit = git_output(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .await?;
    Ok(String::from_utf8_lossy(&commit).trim().to_owned())
}

fn trim_incomplete_utf8_tail(bytes: &[u8]) -> &[u8] {
    let len = bytes.len();
    let start = len.saturating_sub(3);
    for i in (start..len).rev() {
        let b = bytes[i];
        if (b & 0xC0) != 0x80 {
            let expected = if b < 0x80 {
                1
            } else if (b & 0xE0) == 0xC0 {
                2
            } else if (b & 0xF0) == 0xE0 {
                3
            } else if (b & 0xF8) == 0xF0 {
                4
            } else {
                return bytes;
            };
            return if len - i < expected {
                &bytes[..i]
            } else {
                bytes
            };
        }
    }
    bytes
}

fn format_patch(bytes: &[u8], limit: usize, truncated: bool) -> String {
    let clean = if truncated {
        trim_incomplete_utf8_tail(bytes)
    } else {
        bytes
    };
    let text = String::from_utf8_lossy(clean);
    if !truncated && text.len() <= limit {
        return text.into_owned();
    }
    if limit >= PATCH_TRUNCATED_MARKER.len() {
        let cap = (limit - PATCH_TRUNCATED_MARKER.len()).min(text.len());
        let keep = text.floor_char_boundary(cap);
        let mut out = String::with_capacity(keep + PATCH_TRUNCATED_MARKER.len());
        out.push_str(&text[..keep]);
        out.push_str(PATCH_TRUNCATED_MARKER);
        out
    } else {
        text[..text.floor_char_boundary(limit.min(text.len()))].to_owned()
    }
}

fn numstat(bytes: &[u8]) -> CoreResult<(usize, usize)> {
    let mut fields = nul_fields(bytes);
    let mut additions = 0;
    let mut deletions = 0;
    while let Some(entry) = fields.next() {
        let mut columns = entry.splitn(3, |byte| *byte == b'\t');
        let (Some(added), Some(deleted), Some(path)) =
            (columns.next(), columns.next(), columns.next())
        else {
            return Err(git_error("invalid Git numstat entry"));
        };
        additions += line_count(added)?;
        deletions += line_count(deleted)?;
        // With -z, a rename has an empty path column followed by two NUL-separated paths.
        if path.is_empty() && (fields.next().is_none() || fields.next().is_none()) {
            return Err(git_error("missing paths in Git numstat rename"));
        }
    }
    Ok((additions, deletions))
}

fn line_count(field: &[u8]) -> CoreResult<usize> {
    if field == b"-" {
        return Ok(0);
    }
    std::str::from_utf8(field)
        .map_err(git_error)?
        .parse()
        .map_err(git_error)
}

#[cfg(unix)]
fn git_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn git_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
}

/// Commit the worktree's uncommitted state and pin it under `refs/t3/snapshots/`.
/// `None` when the path is gone, is not a Git worktree, or has nothing uncommitted.
async fn snapshot_worktree(repo_root: &Path, worktree: &Path) -> CoreResult<Option<String>> {
    if !tokio::fs::try_exists(worktree).await.map_err(git_error)? {
        return Ok(None);
    }
    let Some(tree) = GitManager::snapshot_tree(worktree).await? else {
        return Ok(None);
    };
    let rev = |spec: &'static str| async move {
        let output = git(worktree)
            .args(["rev-parse", "--verify", "--quiet", spec])
            .output()
            .await
            .map_err(git_error)?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            )),
            Some(1) => Ok(None),
            _ => Err(command_error(&output)),
        }
    };
    if rev("HEAD^{tree}").await?.as_deref() == Some(tree.as_str()) {
        return Ok(None);
    }
    let head = rev("HEAD^{commit}").await?;
    let branch = GitManager::current_branch(worktree).await?;
    let name = branch.clone().unwrap_or_else(|| {
        worktree
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let refname = format!(
        "refs/t3/snapshots/{}-{}",
        ref_safe_name(&name),
        chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ")
    );
    let message = format!(
        "t3 snapshot of {}\n\nbranch: {}\nhead: {}\n",
        worktree.display(),
        branch.as_deref().unwrap_or("(detached)"),
        head.as_deref().unwrap_or("(none)"),
    );
    let mut commit = git(repo_root);
    for (key, value) in [("NAME", "t3 snapshot"), ("EMAIL", "snapshot@t3.invalid")] {
        commit.env(format!("GIT_AUTHOR_{key}"), value);
        commit.env(format!("GIT_COMMITTER_{key}"), value);
    }
    commit.args(["-c", "commit.gpgSign=false", "commit-tree", "-m", &message]);
    if let Some(head) = &head {
        commit.args(["-p", head]);
    }
    commit.arg(&tree);
    let sha = checked_output(commit, false).await?;
    let sha = String::from_utf8_lossy(&sha).trim().to_owned();
    // The empty old value makes update-ref refuse to overwrite an existing snapshot.
    git_output(repo_root, &["update-ref", &refname, &sha, ""]).await?;
    Ok(Some(refname))
}

/// Up to 64 characters of `name` with everything outside `[A-Za-z0-9_-]` turned into `-`.
fn ref_safe_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .take(64)
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect();
    let stem = safe.trim_matches('-');
    if stem.is_empty() { "worktree" } else { stem }.to_owned()
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "filter.lfs.smudge=",
            "-c",
            "filter.lfs.process=",
            "-c",
            "filter.lfs.clean=",
            "-c",
            "filter.lfs.required=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "protocol.file.allow=user",
        ])
        .kill_on_drop(true);
    command
}

async fn disabled_filters(dir: &Path) -> CoreResult<Vec<String>> {
    let keys = git_output(dir, &["config", "--null", "--name-only", "--list"]).await?;
    let mut overrides = Vec::new();
    // Attributes can select any configured driver, including repository-specific drivers.
    for key in nul_fields(&keys) {
        if !key.starts_with(b"filter.") {
            continue;
        }
        let key = std::str::from_utf8(key).map_err(git_error)?;
        let Some((_, setting)) = key.rsplit_once('.') else {
            continue;
        };
        match setting {
            "clean" | "smudge" | "process" => overrides.push(format!("{key}=")),
            "required" => overrides.push(format!("{key}=false")),
            _ => {}
        }
    }
    overrides.sort_unstable();
    overrides.dedup();
    Ok(overrides)
}

fn git_with_filters(dir: &Path, overrides: &[String]) -> Command {
    let mut command = git(dir);
    for key in overrides {
        command.args(["-c", key]);
    }
    command
}

async fn git_output(dir: &Path, args: &[impl AsRef<OsStr>]) -> CoreResult<Vec<u8>> {
    let mut command = git(dir);
    command.args(args);
    checked_output(command, false).await
}

async fn checked_output(mut command: Command, allow_differences: bool) -> CoreResult<Vec<u8>> {
    let output = command.output().await.map_err(git_error)?;
    if output.status.success()
        || (allow_differences && output.status.code() == Some(1) && output.stderr.is_empty())
    {
        Ok(output.stdout)
    } else {
        Err(command_error(&output))
    }
}

async fn checked_output_bounded(
    mut command: Command,
    allow_differences: bool,
    max_bytes: usize,
) -> CoreResult<(Vec<u8>, bool)> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(git_error)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| git_error("failed to capture git stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| git_error("failed to capture git stderr"))?;

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.take(64 * 1024).read_to_end(&mut buf).await;
        buf
    });

    let read_limit = max_bytes.saturating_add(1);
    let mut stdout_buf = Vec::with_capacity(read_limit.min(64 * 1024));
    stdout
        .take(read_limit as u64)
        .read_to_end(&mut stdout_buf)
        .await
        .map_err(git_error)?;

    let truncated = stdout_buf.len() > max_bytes;
    if truncated {
        stdout_buf.truncate(max_bytes);
        let _ = child.kill().await;
        let _ = child.wait().await;
        stderr_task.abort();
        return Ok((stdout_buf, true));
    }

    let status = child.wait().await.map_err(git_error)?;
    let stderr_buf = stderr_task.await.unwrap_or_default();
    if status.success() || (allow_differences && status.code() == Some(1) && stderr_buf.is_empty())
    {
        Ok((stdout_buf, false))
    } else {
        Err(command_error(&Output {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        }))
    }
}

fn command_error(output: &Output) -> CoreError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.trim().is_empty() {
        git_error(output.status)
    } else {
        git_error(stderr.trim())
    }
}

fn git_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::Git(error.to_string())
}

fn validate_relative_path(path: &str) -> CoreResult<()> {
    let path = Path::new(path);
    let mut has_normal = false;
    let invalid = path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| match component {
            std::path::Component::Normal(seg) => {
                has_normal = true;
                seg.to_str().is_some_and(|s| s.eq_ignore_ascii_case(".git"))
            }
            _ => true,
        })
        || !has_normal;
    if invalid {
        return Err(git_error(
            "path must be a non-empty repository-relative path without '.', '..', or '.git'",
        ));
    }
    Ok(())
}

fn ensure_inside_repo(root: &Path, path: &Path) -> CoreResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| git_error("invalid repository-relative path"))?;
    if let Ok(parent) = parent.canonicalize()
        && !parent.starts_with(root)
    {
        return Err(git_error("path resolves outside repository"));
    }
    Ok(())
}

/// Copies gitignored files selected by `.worktreeinclude` into a new worktree.
/// Git decides what is ignored (hence never tracked); the `ignore` crate matches the include
/// file because the `**/` rule depends on which pattern matched.
/// <https://code.claude.com/docs/en/worktrees#copy-gitignored-files-into-worktrees>
async fn copy_worktree_includes(repo_root: &Path, worktree: &Path) -> CoreResult<()> {
    let root = git_output(repo_root, &["rev-parse", "--show-toplevel"]).await?;
    let root = git_path(root.strip_suffix(b"\n").unwrap_or(&root));
    let patterns = match tokio::fs::read_to_string(root.join(".worktreeinclude")).await {
        Ok(patterns) => patterns,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(git_error(error)),
    };
    let lines = || patterns.trim_start_matches('\u{feff}').lines();
    let all = include_matcher(&root, lines())?;
    // Fallback for files that an unreached `**/` pattern shadows in the full matcher.
    let rooted = include_matcher(&root, lines().filter(|line| !line.starts_with("**/")))?;
    let list = [
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
    ];
    let ignored = git_output(&root, &list).await?;
    let root = tokio::fs::canonicalize(&root).await.map_err(git_error)?;
    let mut cache = HashMap::new();
    for relative in nul_fields(&ignored).map(git_path) {
        if !is_included(&root, &all, &relative, &mut cache).await?
            && !is_included(&root, &rooted, &relative, &mut cache).await?
        {
            continue;
        }
        if let Err(error) = copy_include(&root, worktree, &relative).await {
            tracing::warn!(%error, path = %relative.display(), "Failed to copy included file");
        }
    }
    Ok(())
}

fn include_matcher<'a>(root: &Path, lines: impl Iterator<Item = &'a str>) -> CoreResult<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    for line in lines {
        // Like Git, skip a malformed pattern without discarding the rest.
        if let Err(error) = builder.add_line(None, line) {
            tracing::warn!(%error, "Invalid .worktreeinclude pattern");
        }
    }
    builder.build().map_err(git_error)
}

async fn is_included(
    root: &Path,
    matcher: &Gitignore,
    relative: &Path,
    cache: &mut HashMap<PathBuf, bool>,
) -> CoreResult<bool> {
    let mut paths: Vec<&Path> = relative.ancestors().collect();
    paths.pop(); // the empty root
    paths.reverse();
    // Top down, as in Git: a matched directory includes everything below it.
    for (depth, path) in paths.iter().enumerate() {
        let Match::Ignore(glob) = matcher.matched(path, depth + 1 < paths.len()) else {
            continue;
        };
        let Some(rest) = glob.original().strip_prefix("**/") else {
            return Ok(true);
        };
        // `**/` enters a wholly ignored directory above the match only if it names it.
        for dir in &paths[..depth] {
            if !cache.contains_key(*dir) {
                cache.insert(dir.to_path_buf(), is_ignored(root, dir).await?);
            }
            if cache[*dir] {
                let name = rest.split('/').next();
                return Ok(dir.iter().any(|part| part.to_str() == name));
            }
        }
        return Ok(true);
    }
    Ok(false)
}

async fn is_ignored(dir: &Path, relative: &Path) -> CoreResult<bool> {
    let mut command = git(dir);
    // The `./` prefix keeps a leading `:` from being read as pathspec magic.
    command
        .args(["check-ignore", "-q", "--"])
        .arg(Path::new(".").join(relative));
    let output = command.output().await.map_err(git_error)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(command_error(&output)),
    }
}

async fn copy_include(root: &Path, worktree: &Path, relative: &Path) -> CoreResult<()> {
    // Resolving symlinks first rejects anything that leads outside the repo or into `.git`.
    let source = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(git_error)?;
    let in_git = |path: &Path| path.iter().any(|part| part.eq_ignore_ascii_case(".git"));
    let inside = source.strip_prefix(root);
    if !inside.is_ok_and(|inside| !in_git(inside)) || in_git(relative) || !source.is_file() {
        return Ok(());
    }
    // Never overwrite checked-out content or write through a checked-out symlink. A file the
    // worktree's revision does not ignore is skipped too, so it cannot be committed by accident.
    let target = worktree.join(relative);
    if target.symlink_metadata().is_ok()
        || relative
            .ancestors()
            .skip(1)
            .any(|dir| worktree.join(dir).is_symlink())
        || !is_ignored(worktree, relative).await?
    {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(git_error)?;
    }
    // `copy` also carries over the permission bits.
    tokio::fs::copy(source, target).await.map_err(git_error)?;
    Ok(())
}

async fn validate_branch(repo_root: &Path, branch: &str) -> CoreResult<()> {
    if branch.is_empty() {
        return Err(git_error("branch name must not be empty"));
    }
    let mut command = git(repo_root);
    command.args(["check-ref-format", "--branch", branch]);
    checked_output(command, false).await?;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod primitive_tests {
    use super::*;
    use std::fs;

    struct Repo(PathBuf);

    impl Repo {
        async fn new() -> Self {
            let path = std::env::temp_dir().join(format!("t3-git-primitives-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            git_output(&path, &["init", "--initial-branch=main"])
                .await
                .unwrap();
            git_output(&path, &["config", "user.name", "Git tests"])
                .await
                .unwrap();
            git_output(
                &path,
                &["config", "user.email", "git-tests@example.invalid"],
            )
            .await
            .unwrap();
            fs::write(path.join("tracked.txt"), "base\n").unwrap();
            git_output(&path, &["add", "--", "tracked.txt"])
                .await
                .unwrap();
            git_output(&path, &["commit", "-m", "initial"])
                .await
                .unwrap();
            Self(path)
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn snapshot_restores_tracked_and_untracked_files_without_changing_index() {
        let repo = Repo::new().await;
        fs::write(repo.0.join("tracked.txt"), "snapshot\n").unwrap();
        fs::write(repo.0.join("untracked.txt"), "keep\n").unwrap();
        let staged_before = git_output(&repo.0, &["diff", "--cached", "--raw"])
            .await
            .unwrap();
        let tree = GitManager::snapshot_tree(&repo.0).await.unwrap().unwrap();
        let staged_after = git_output(&repo.0, &["diff", "--cached", "--raw"])
            .await
            .unwrap();
        assert_eq!(staged_before, staged_after);

        fs::write(repo.0.join("tracked.txt"), "later\n").unwrap();
        fs::write(repo.0.join("new.txt"), "remove\n").unwrap();
        GitManager::restore_tree(&repo.0, &tree).await.unwrap();
        assert_eq!(
            fs::read_to_string(repo.0.join("tracked.txt")).unwrap(),
            "snapshot\n"
        );
        assert_eq!(
            fs::read_to_string(repo.0.join("untracked.txt")).unwrap(),
            "keep\n"
        );
        assert!(!repo.0.join("new.txt").exists());
    }

    #[tokio::test]
    async fn file_operations_reject_parent_paths_and_commit_staged_changes() {
        let repo = Repo::new().await;
        assert!(GitManager::stage_file(&repo.0, "../outside").await.is_err());
        assert!(GitManager::discard_file(&repo.0, "/outside").await.is_err());
        fs::write(repo.0.join("tracked.txt"), "updated\n").unwrap();
        GitManager::stage_file(&repo.0, "tracked.txt")
            .await
            .unwrap();
        let sha = GitManager::commit(&repo.0, "update tracked file")
            .await
            .unwrap();
        assert_eq!(
            git_output(&repo.0, &["rev-parse", "HEAD"])
                .await
                .unwrap()
                .trim_ascii(),
            sha.as_bytes()
        );
        assert!(GitManager::commit(&repo.0, " ").await.is_err());
    }

    #[tokio::test]
    async fn merges_and_deletes_a_validated_branch() {
        let repo = Repo::new().await;
        git_output(&repo.0, &["branch", "feature"]).await.unwrap();
        git_output(&repo.0, &["checkout", "feature"]).await.unwrap();
        fs::write(repo.0.join("feature.txt"), "feature\n").unwrap();
        git_output(&repo.0, &["add", "--", "feature.txt"])
            .await
            .unwrap();
        git_output(&repo.0, &["commit", "-m", "feature"])
            .await
            .unwrap();
        git_output(&repo.0, &["checkout", "main"]).await.unwrap();

        let sha = GitManager::merge_branch(&repo.0, "feature").await.unwrap();
        assert_eq!(
            git_output(&repo.0, &["rev-parse", "HEAD"])
                .await
                .unwrap()
                .trim_ascii(),
            sha.as_bytes()
        );
        GitManager::delete_branch(&repo.0, "feature").await.unwrap();
        assert!(
            !GitManager::list_branches(&repo.0)
                .await
                .unwrap()
                .iter()
                .any(|branch| branch == "feature")
        );
    }

    #[tokio::test]
    async fn diff_summary_enforces_patch_and_total_budget_truncation() {
        let repo = Repo::new().await;
        let payload = "0123456789abcdef0123456789abcdef\n".repeat(7_000); // ~231 KiB > PATCH_LIMIT
        for i in 0..12 {
            fs::write(repo.0.join(format!("big_{i:02}.txt")), &payload).unwrap();
        }
        let diff = GitManager::diff_summary(&repo.0, false).await.unwrap();
        assert_eq!(diff.files.len(), 12);
        assert_eq!(diff.total_additions, 12 * 7_000);
        // First 10 files each consume PATCH_LIMIT (200 KiB) -> 2000 KiB; 11th consumes 48 KiB;
        // 12th hits exhausted budget without invoking extra patch diffs.
        for file in &diff.files[..11] {
            assert!(file.patch.len() <= PATCH_LIMIT, "{}", file.path);
            assert!(
                file.patch.ends_with(PATCH_TRUNCATED_MARKER),
                "{} did not end with truncation marker",
                file.path
            );
        }
        assert_eq!(diff.files[11].patch, BUDGET_TRUNCATED_MARKER);
        let non_marker_bytes: usize = diff.files[..11].iter().map(|f| f.patch.len()).sum();
        assert!(non_marker_bytes <= MAX_TOTAL_PATCH_BYTES);
    }

    #[tokio::test]
    async fn has_merge_in_progress_detects_active_merge_and_protects_user_merge() {
        let repo = Repo::new().await;
        assert!(!GitManager::has_merge_in_progress(&repo.0).await.unwrap());

        git_output(&repo.0, &["checkout", "-b", "conflict-branch"])
            .await
            .unwrap();
        fs::write(repo.0.join("tracked.txt"), "branch side\n").unwrap();
        git_output(&repo.0, &["commit", "-am", "branch edit"])
            .await
            .unwrap();

        git_output(&repo.0, &["checkout", "main"]).await.unwrap();
        fs::write(repo.0.join("tracked.txt"), "main side\n").unwrap();
        git_output(&repo.0, &["commit", "-am", "main edit"])
            .await
            .unwrap();

        // Start a conflicting merge manually (simulating a user merge in progress).
        let merge = git(&repo.0)
            .args(["merge", "--no-edit", "conflict-branch"])
            .output()
            .await
            .unwrap();
        assert!(!merge.status.success());
        assert!(GitManager::has_merge_in_progress(&repo.0).await.unwrap());

        // merge_branch_checked MUST reject and MUST NOT abort the user's merge.
        let err = GitManager::merge_branch_checked(&repo.0, "conflict-branch", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("merge is already in progress"));
        assert!(GitManager::has_merge_in_progress(&repo.0).await.unwrap());

        // Abort the manual merge and confirm MERGE_HEAD clears.
        git_output(&repo.0, &["merge", "--abort"]).await.unwrap();
        assert!(!GitManager::has_merge_in_progress(&repo.0).await.unwrap());

        // Now merge_branch_checked reports Conflict with conflict_files and aborts its own merge cleanly.
        let outcome = GitManager::merge_branch_checked(&repo.0, "conflict-branch", None)
            .await
            .unwrap();
        match outcome {
            MergeOutcome::Conflict { conflict_files, .. } => {
                assert_eq!(conflict_files, vec!["tracked.txt".to_owned()]);
            }
            MergeOutcome::Merged { .. } => panic!("expected merge conflict"),
        }
        assert!(!GitManager::has_merge_in_progress(&repo.0).await.unwrap());
    }

    #[tokio::test]
    async fn has_uncommitted_changes_tracks_working_tree_state() {
        let repo = Repo::new().await;
        assert!(!GitManager::has_uncommitted_changes(&repo.0).await.unwrap());

        fs::write(repo.0.join("untracked.txt"), "new\n").unwrap();
        assert!(GitManager::has_uncommitted_changes(&repo.0).await.unwrap());

        git_output(&repo.0, &["add", "--", "untracked.txt"])
            .await
            .unwrap();
        assert!(GitManager::has_uncommitted_changes(&repo.0).await.unwrap());

        git_output(&repo.0, &["commit", "-m", "add untracked"])
            .await
            .unwrap();
        assert!(!GitManager::has_uncommitted_changes(&repo.0).await.unwrap());

        fs::write(repo.0.join("tracked.txt"), "modified\n").unwrap();
        assert!(GitManager::has_uncommitted_changes(&repo.0).await.unwrap());
    }

    #[tokio::test]
    async fn branch_diff_summary_reports_three_dot_diff_and_enforces_budget() {
        let repo = Repo::new().await;
        fs::write(repo.0.join("space.txt"), "one two\n").unwrap();
        fs::write(repo.0.join("rename_me.txt"), "same line\n".repeat(20)).unwrap();
        git_output(&repo.0, &["add", "--", "."]).await.unwrap();
        git_output(&repo.0, &["commit", "-m", "setup"])
            .await
            .unwrap();

        // Create feature branch with added, renamed, and whitespace-only changes.
        git_output(&repo.0, &["checkout", "-b", "t3rs/feat"])
            .await
            .unwrap();
        fs::write(repo.0.join("space.txt"), "one   two\n").unwrap();
        fs::write(repo.0.join("added.txt"), "hello\nworld\n").unwrap();
        git_output(&repo.0, &["mv", "rename_me.txt", "renamed.txt"])
            .await
            .unwrap();
        git_output(&repo.0, &["add", "--", "."]).await.unwrap();
        git_output(&repo.0, &["commit", "-m", "feature changes"])
            .await
            .unwrap();

        // Advance main with an unrelated commit that three-dot diff must NOT include.
        git_output(&repo.0, &["checkout", "main"]).await.unwrap();
        fs::write(repo.0.join("main_only.txt"), "on main\n").unwrap();
        git_output(&repo.0, &["add", "--", "main_only.txt"])
            .await
            .unwrap();
        git_output(&repo.0, &["commit", "-m", "main commit"])
            .await
            .unwrap();

        let full = GitManager::branch_diff_summary(&repo.0, "main", "t3rs/feat", false)
            .await
            .unwrap();
        assert_eq!(full.branch.as_deref(), Some("t3rs/feat"));
        assert!(!full.clean);
        assert!(!full.files.iter().any(|f| f.path == "main_only.txt"));
        assert!(full.files.iter().any(|f| f.path == "space.txt"));
        let added = full.files.iter().find(|f| f.path == "added.txt").unwrap();
        assert_eq!(
            (added.status.as_str(), added.additions, added.deletions),
            ("A", 2, 0)
        );
        assert!(added.patch.contains("+hello\n+world"));
        let renamed = full.files.iter().find(|f| f.path == "renamed.txt").unwrap();
        assert!(renamed.status.starts_with('R'));
        assert!(renamed.patch.contains("rename from rename_me.txt"));

        let ignore_ws = GitManager::branch_diff_summary(&repo.0, "main", "t3rs/feat", true)
            .await
            .unwrap();
        assert!(!ignore_ws.files.iter().any(|f| f.path == "space.txt"));
        assert!(ignore_ws.files.iter().any(|f| f.path == "added.txt"));

        // Invalid ref is rejected.
        assert!(
            GitManager::branch_diff_summary(&repo.0, "--bad-ref", "t3rs/feat", false)
                .await
                .is_err()
        );

        // Budget enforcement on branch_diff_summary truncates once budget is exhausted.
        let tiny_budget =
            GitManager::branch_diff_summary_with_budget(&repo.0, "main", "t3rs/feat", false, 40)
                .await
                .unwrap();
        assert!(
            tiny_budget
                .files
                .iter()
                .any(|f| f.patch.ends_with(PATCH_TRUNCATED_MARKER))
        );
        assert!(
            tiny_budget
                .files
                .iter()
                .any(|f| f.patch == BUDGET_TRUNCATED_MARKER)
        );
    }
}
