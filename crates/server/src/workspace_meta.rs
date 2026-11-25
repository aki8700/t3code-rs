use serde::Serialize;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const MAX_METADATA_BYTES: u64 = 16 * 1024;
const MAX_FILES: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceCommand {
    pub name: String,
    pub desc: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceMeta {
    pub commands: Vec<WorkspaceCommand>,
    pub files: Vec<String>,
}

pub(crate) async fn scan_workspace(root: &Path) -> WorkspaceMeta {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut commands = Vec::new();
    collect_commands(&root, Path::new(".claude/commands"), false, &mut commands);
    collect_commands(&root, Path::new(".agents/skills"), true, &mut commands);
    commands.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.source.cmp(&right.source))
    });
    WorkspaceMeta {
        commands,
        files: git_files(&root).unwrap_or_else(|| shallow_files(&root)),
    }
}

fn collect_commands(
    root: &Path,
    relative: &Path,
    skills: bool,
    output: &mut Vec<WorkspaceCommand>,
) {
    let base = root.join(relative);
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if skills {
            if !metadata.is_dir() && !metadata.file_type().is_symlink() {
                continue;
            }
            let file = path.join("SKILL.md");
            if let Some(command) = read_command(
                root,
                &file,
                relative.join(entry.file_name()).join("SKILL.md"),
                true,
            ) {
                output.push(command);
            }
        } else if path.extension().is_some_and(|extension| extension == "md")
            && let Some(command) =
                read_command(root, &path, relative.join(entry.file_name()), false)
        {
            output.push(command);
        }
    }
}

fn read_command(
    root: &Path,
    path: &Path,
    relative: PathBuf,
    skill: bool,
) -> Option<WorkspaceCommand> {
    let canonical_root = fs::canonicalize(root).ok()?;
    let canonical_file = fs::canonicalize(path).ok()?;
    if !canonical_file.starts_with(&canonical_root) || !canonical_file.is_file() {
        return None;
    }
    let mut file = File::open(canonical_file).ok()?.take(MAX_METADATA_BYTES);
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    let (frontmatter, body) = parse_frontmatter(&contents);
    let description = frontmatter
        .and_then(|yaml| {
            yaml.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "description").then(|| unquote(value.trim()).to_owned())
            })
        })
        .filter(|value| !value.is_empty())
        .or_else(|| first_content_line(body))?;
    let name = if skill {
        relative.parent()?.file_name()?.to_str()?.to_owned()
    } else {
        relative.file_stem()?.to_str()?.to_owned()
    };
    Some(WorkspaceCommand {
        name,
        desc: description,
        source: relative.to_string_lossy().into_owned(),
    })
}

fn parse_frontmatter(contents: &str) -> (Option<&str>, &str) {
    let Some(after_open) = contents
        .strip_prefix("---\n")
        .or_else(|| contents.strip_prefix("---\r\n"))
    else {
        return (None, contents);
    };
    let mut offset = 0;
    for segment in after_open.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        if line.trim() == "---" {
            return (
                Some(&after_open[..offset]),
                &after_open[offset + segment.len()..],
            );
        }
        offset += segment.len();
    }
    (None, contents)
}

fn first_content_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "---")
        .map(|line| line.trim_start_matches('#').trim().to_owned())
        .filter(|line| !line.is_empty())
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn git_files(root: &Path) -> Option<Vec<String>> {
    let mut child = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);
    let mut files = Vec::new();
    let mut bytes = Vec::new();
    while files.len() < MAX_FILES {
        bytes.clear();
        let count = reader.read_until(0, &mut bytes).ok()?;
        if count == 0 {
            break;
        }
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        let path = std::str::from_utf8(&bytes).ok()?;
        if safe_relative(path) {
            files.push(path.to_owned());
        }
    }
    let capped = files.len() == MAX_FILES;
    if capped {
        let _ = child.kill();
    }
    let status = child.wait().ok()?;
    if !capped && !status.success() {
        return None;
    }
    Some(files)
}

fn safe_relative(value: &str) -> bool {
    let path = Path::new(value);
    !path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

fn shallow_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if files.len() >= MAX_FILES {
                return files;
            }
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            let Ok(canonical) = fs::canonicalize(&path) else {
                continue;
            };
            if !canonical.starts_with(&canonical_root) {
                continue;
            }
            if metadata.is_dir() && depth < 3 && entry.file_name() != ".git" {
                pending.push((path, depth + 1));
            } else if metadata.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                files.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[tokio::test]
    async fn scans_frontmatter_and_skips_symlinks_outside_workspace() {
        let root = std::env::temp_dir().join(format!("t3-meta-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join(".claude/commands")).unwrap();
        fs::create_dir_all(root.join(".agents/skills/demo")).unwrap();
        fs::write(
            root.join(".claude/commands/check.md"),
            "---\ndescription: Run checks\n---\n# Check\n",
        )
        .unwrap();
        fs::write(
            root.join(".agents/skills/demo/SKILL.md"),
            "# Demo skill\nDetails\n",
        )
        .unwrap();
        let outside = root.with_extension("outside.md");
        fs::write(&outside, "---\ndescription: outside\n---\n").unwrap();
        symlink(&outside, root.join(".claude/commands/escape.md")).unwrap();

        let meta = scan_workspace(&root).await;
        assert_eq!(meta.commands.len(), 2);
        assert!(
            meta.commands
                .iter()
                .any(|command| command.name == "check" && command.desc == "Run checks")
        );
        assert!(
            meta.commands
                .iter()
                .any(|command| command.name == "demo" && command.desc == "Demo skill")
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(outside);
    }
}
