use crate::decode;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use t3_core::{CoreError, CoreResult};
use t3_maki::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_DEPTH, DEFAULT_MAX_HITS, DEFAULT_MAX_LINES, code_diff,
    code_diff_in_worktree, code_diff_scoped, code_grep, code_index, code_slice,
};

#[derive(Deserialize)]
struct IndexArgs {
    path: PathBuf,
    max_depth: Option<usize>,
    max_lines: Option<usize>,
}

#[derive(Deserialize)]
struct SliceArgs {
    path: PathBuf,
    symbol: Option<String>,
    start: Option<usize>,
    end: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: PathBuf,
    glob: Option<String>,
    max_hits: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct DiffArgs {
    cwd: PathBuf,
    rev: Option<String>,
    path: Option<String>,
    max_bytes: Option<usize>,
}

pub(crate) async fn call(
    name: String,
    args: Value,
    workspace: Option<PathBuf>,
    project: Option<PathBuf>,
) -> CoreResult<Value> {
    tokio::task::spawn_blocking(move || {
        execute(&name, args, workspace.as_deref(), project.as_deref())
    })
    .await
    .map_err(|error| CoreError::Internal(error.to_string()))?
}

fn execute(
    name: &str,
    args: Value,
    workspace: Option<&Path>,
    project: Option<&Path>,
) -> CoreResult<Value> {
    let workspace = workspace
        .map(Path::canonicalize)
        .transpose()
        .map_err(|error| {
            CoreError::InvalidArgument(format!("cannot resolve caller workspace: {error}"))
        })?;
    let result = match name {
        "code_index" => {
            let args: IndexArgs = decode(args)?;
            let path = scoped_path(args.path, workspace.as_deref())?;
            code_index(
                &path,
                args.max_depth.unwrap_or(DEFAULT_MAX_DEPTH),
                args.max_lines.unwrap_or(DEFAULT_MAX_LINES),
            )
        }
        "code_slice" => {
            let args: SliceArgs = decode(args)?;
            let path = scoped_path(args.path, workspace.as_deref())?;
            code_slice(
                &path,
                args.symbol.as_deref(),
                args.start,
                args.end,
                args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
            )
        }
        "code_grep" => {
            let args: GrepArgs = decode(args)?;
            let path = scoped_path(args.path, workspace.as_deref())?;
            code_grep(
                &args.pattern,
                &path,
                args.glob.as_deref(),
                args.max_hits.unwrap_or(DEFAULT_MAX_HITS),
                args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
            )
        }
        "code_diff" => {
            let args: DiffArgs = decode(args)?;
            let cwd = scoped_path(args.cwd, workspace.as_deref())?;
            let rev = args.rev.as_deref();
            let path = args.path.as_deref();
            let max_bytes = args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
            match (workspace, project) {
                (Some(root), Some(project)) => {
                    code_diff_in_worktree(&cwd, rev, path, max_bytes, &root, project)
                }
                (Some(root), None) => code_diff_scoped(&cwd, rev, path, max_bytes, &root),
                (None, _) => code_diff(&cwd, rev, path, max_bytes),
            }
        }
        _ => return Err(CoreError::InvalidArgument(format!("unknown tool: {name}"))),
    };
    result.map_err(CoreError::InvalidArgument)
}

fn scoped_path(path: PathBuf, workspace: Option<&Path>) -> CoreResult<PathBuf> {
    let Some(workspace) = workspace else {
        return Ok(path);
    };
    let path = workspace.join(path).canonicalize().map_err(|error| {
        CoreError::InvalidArgument(format!("cannot resolve workspace path: {error}"))
    })?;
    if !path.starts_with(workspace) {
        return Err(CoreError::InvalidArgument(
            "path is outside the caller's workspace".into(),
        ));
    }
    Ok(path)
}
