use crate::{GENERATED_GLOBS, MAX_OUTPUT_BYTES, utf8_prefix};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

/// Summarize tracked Git changes, optionally against a named revision.
pub fn code_diff(
    cwd: &Path,
    rev: Option<&str>,
    path: Option<&str>,
    max_bytes: usize,
) -> Result<Value, String> {
    diff(cwd, rev, path, max_bytes, None)
}

/// Restrict Git discovery and path selection to the caller's workspace.
pub fn code_diff_scoped(
    cwd: &Path,
    rev: Option<&str>,
    path: Option<&str>,
    max_bytes: usize,
    workspace: &Path,
) -> Result<Value, String> {
    diff(cwd, rev, path, max_bytes, Some(workspace))
}

fn diff(
    cwd: &Path,
    rev: Option<&str>,
    path: Option<&str>,
    max_bytes: usize,
    workspace: Option<&Path>,
) -> Result<Value, String> {
    let cwd = cwd.canonicalize().map_err(|error| error.to_string())?;
    let workspace = workspace
        .map(Path::canonicalize)
        .transpose()
        .map_err(|error| error.to_string())?;
    let path = literal_path(&cwd, path, workspace.as_deref())?;
    if let Some(workspace) = &workspace {
        let (root, truncated) =
            git_text(command(&cwd).args(["rev-parse", "--show-toplevel"]), 32_768)?;
        if truncated {
            return Err("Git repository path exceeds the limit".into());
        }
        let root = Path::new(root.strip_suffix('\n').unwrap_or(&root))
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !root.starts_with(workspace) {
            return Err("Git repository is outside the caller's workspace".into());
        }
    }
    let tree = if let Some(rev) = rev {
        let (tree, truncated) = git_text(
            command(&cwd).args([
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{rev}^{{tree}}"),
            ]),
            128,
        )?;
        let tree = tree.trim();
        if truncated
            || !matches!(tree.len(), 40 | 64)
            || !tree.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("invalid Git revision".into());
        }
        Some(tree.to_owned())
    } else {
        None
    };

    let filters = disabled_filters(&cwd)?;
    let make_diff = |format: &[&str], include_generated: bool| {
        let mut git = command(&cwd);
        for filter in &filters {
            git.args(["-c", filter]);
        }
        git.args([
            "diff",
            "--relative",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "-O/dev/null",
            "--submodule=short",
            "--ignore-submodules=dirty",
        ])
        .args(format)
        .args(tree.as_deref())
        .arg("--")
        .arg(&path);
        if !include_generated {
            for glob in GENERATED_GLOBS {
                git.arg(format!(":(exclude,glob){glob}"));
            }
        }
        git
    };
    let (shortstat, _) = git_text(&mut make_diff(&["--shortstat"], false), 1024)?;
    let (name_status, names_truncated) =
        git_text(&mut make_diff(&["--name-status"], false), 32_768)?;
    let (hunks, hunks_truncated) = git_text(
        &mut make_diff(
            &[
                "-U2",
                "--inter-hunk-context=0",
                "--output-indicator-new=+",
                "--output-indicator-old=-",
                "--output-indicator-context= ",
            ],
            false,
        ),
        max_bytes.min(MAX_OUTPUT_BYTES),
    )?;

    let count_paths = |include_generated| {
        let mut count = 0;
        run(
            &mut make_diff(&["--name-only", "-z"], include_generated),
            |chunk| {
                count += chunk.iter().filter(|&&byte| byte == 0).count();
            },
        )?;
        Ok::<_, String>(count)
    };
    let skipped_generated = count_paths(true)?.saturating_sub(count_paths(false)?);

    Ok(json!({
        "shortstat": shortstat.trim(),
        "name_status": name_status.trim_end_matches('\n'),
        "hunks": hunks,
        "skipped_generated": skipped_generated,
        "truncated": names_truncated || hunks_truncated,
    }))
}

fn literal_path(
    cwd: &Path,
    path: Option<&str>,
    workspace: Option<&Path>,
) -> Result<String, String> {
    if workspace.is_some_and(|workspace| !cwd.starts_with(workspace)) {
        return Err("Git working directory is outside the caller's workspace".into());
    }
    let path = Path::new(path.unwrap_or("."));
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("Git path is outside the working directory".into());
    }
    let resolved = if workspace.is_some() || path.is_absolute() {
        Some(canonicalize_diff_path(&cwd.join(path))?)
    } else {
        None
    };
    if let (Some(workspace), Some(resolved)) = (workspace, &resolved)
        && !resolved.starts_with(workspace)
    {
        return Err("Git path is outside the caller's workspace".into());
    }
    let path = if path.is_absolute() {
        path.strip_prefix(cwd)
            .or_else(|_| resolved.as_deref().unwrap_or(path).strip_prefix(cwd))
            .map_err(|_| "Git path is outside the working directory")?
    } else {
        path
    };
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let path = path.to_str().ok_or("Git path is not valid UTF-8")?;
    Ok(format!(":(literal){path}"))
}

fn canonicalize_diff_path(path: &Path) -> Result<PathBuf, String> {
    // A deleted file may still be in the diff. Resolve its closest existing ancestor.
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let parent = ancestor.canonicalize().map_err(|error| error.to_string())?;
                let suffix = path
                    .strip_prefix(ancestor)
                    .map_err(|error| error.to_string())?;
                return Ok(parent.join(suffix));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("could not resolve Git path".into())
}

fn command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(cwd)
        .args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.attributesFile=/dev/null",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    command
}

fn disabled_filters(cwd: &Path) -> Result<Vec<String>, String> {
    // A worktree diff can invoke configured clean filters even with textconv disabled.
    let (keys, truncated) = git_text(
        command(cwd).args(["config", "--null", "--name-only", "--list"]),
        65_536,
    )?;
    if truncated {
        return Err("Git config keys exceed the limit".into());
    }
    let mut overrides = keys
        .split_terminator('\0')
        .filter_map(|key| {
            let (filter, setting) = key.rsplit_once('.')?;
            if !filter.starts_with("filter.") {
                return None;
            }
            match setting {
                "clean" | "process" => Some(format!("{key}=")),
                "required" => Some(format!("{key}=false")),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    overrides.sort();
    overrides.dedup();
    Ok(overrides)
}

fn git_text(command: &mut Command, limit: usize) -> Result<(String, bool), String> {
    let mut bytes = Vec::new();
    let read_limit = limit + 4;
    let mut truncated = false;
    run(command, |chunk| {
        let count = chunk.len().min(read_limit - bytes.len());
        bytes.extend_from_slice(&chunk[..count]);
        truncated |= count < chunk.len();
    })?;
    let text = String::from_utf8_lossy(&bytes);
    truncated |= text.len() > limit;
    Ok((utf8_prefix(&text, limit).to_owned(), truncated))
}

fn run(command: &mut Command, mut consume: impl FnMut(&[u8])) -> Result<(), String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run Git: {error}"))?;
    let mut stdout = child.stdout.take().expect("Git stdout is piped");
    let mut buf = [0; 8192];
    let mut result = Ok(());
    loop {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(count) => consume(&buf[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                result = Err(error.to_string());
                let _ = child.kill();
                break;
            }
        }
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    result?;
    if !status.success() {
        return Err(format!("Git command failed ({status})"));
    }
    Ok(())
}
