use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Output, Stdio},
};

use t3_core::{CoreError, CoreResult, DiffFile, DiffSummary};
use tokio::process::Command;
use uuid::Uuid;

const PATCH_LIMIT: usize = 200 * 1024;

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
        let safe: String = branch_name
            .chars()
            .take(64)
            .map(|ch| match ch {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
                _ => '-',
            })
            .collect();
        let stem = safe.trim_matches('-');
        let stem = if stem.is_empty() { "worktree" } else { stem };
        let suffix = Uuid::new_v4().simple().to_string();
        let name = format!("{stem}-{}", &suffix[..12]);
        let branch = format!("t3rs/{name}");
        let path = root.join(name);
        let mut command = git(repo_root);
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
        tracing::debug!("Created Git worktree");
        Ok((path, branch))
    }

    pub async fn remove_worktree(&self, repo_root: &Path, worktree_path: &Path) -> CoreResult<()> {
        let mut command = git(repo_root);
        command
            .args(["worktree", "remove", "--force", "--"])
            .arg(worktree_path);
        checked_output(command, false).await?;
        tracing::debug!("Removed Git worktree");
        Ok(())
    }

    pub async fn diff_summary(dir: &Path, ignore_whitespace: bool) -> CoreResult<DiffSummary> {
        let root = git_output(dir, &["rev-parse", "--show-toplevel"]).await?;
        let root = git_path(root.strip_suffix(b"\n").unwrap_or(&root));
        let branch = Self::current_branch(&root).await?;
        let status = git_output(
            &root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
        .await?;
        let entries = status_entries(&status)?;
        let base = diff_base(&root, &entries).await?;

        let mut files = Vec::with_capacity(entries.len());
        for entry in entries {
            let path = git_path(entry.path);
            let diff = FileDiff {
                root: &root,
                path: &path,
                base: base.as_deref(),
                rename_from: entry.rename_from.map(git_path),
                untracked: entry.status == b"??",
                ignore_whitespace,
            };
            let patch = diff.output("--patch").await?;
            if patch.is_empty() && ignore_whitespace && !diff.untracked {
                continue;
            }

            let (additions, deletions) = numstat(&diff.output("--numstat").await?)?;
            let patch = String::from_utf8_lossy(&patch);
            files.push(DiffFile {
                path: path.to_string_lossy().into_owned(),
                status: String::from_utf8_lossy(entry.status).into_owned(),
                additions,
                deletions,
                patch: patch[..patch.floor_char_boundary(PATCH_LIMIT)].to_owned(),
            });
        }

        Ok(DiffSummary {
            workspace_path: dir.to_owned(),
            branch,
            clean: files.is_empty(),
            total_additions: files.iter().map(|file| file.additions).sum(),
            total_deletions: files.iter().map(|file| file.deletions).sum(),
            files,
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
    path: &'a Path,
    base: Option<&'a str>,
    rename_from: Option<PathBuf>,
    untracked: bool,
    ignore_whitespace: bool,
}

impl FileDiff<'_> {
    async fn output(&self, format: &str) -> CoreResult<Vec<u8>> {
        let mut command = git(self.root);
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
        checked_output(command, self.untracked).await
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

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .kill_on_drop(true);
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

#[cfg(test)]
mod tests;
