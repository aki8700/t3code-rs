//! Bounded, local code discovery for MCP clients.

mod diff;
mod econ;
mod file;
mod syntax;

pub use diff::{code_diff, code_diff_in_worktree, code_diff_scoped};
pub use econ::{CompactionEconInput, CompactionEconResult, Prices, compaction_econ};

use globset::GlobBuilder;
use ignore::{Walk, WalkBuilder};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    io::Read,
    path::Path,
    sync::{Mutex, OnceLock},
};
use syntax::{Grammar, enclosing_symbol, symbols};

const INDEX_CACHE_MAX_ENTRIES: usize = 512;

#[derive(Clone)]
struct CachedFileIndex {
    declarations: Vec<Value>,
    outline: Vec<String>,
}

type IndexCacheKey = (Option<&'static str>, u64, usize);

static INDEX_CACHE: OnceLock<Mutex<HashMap<IndexCacheKey, CachedFileIndex>>> = OnceLock::new();

fn index_cache() -> &'static Mutex<HashMap<IndexCacheKey, CachedFileIndex>> {
    INDEX_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn content_hash(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

/// Cap a UTF-8 context section at `max_bytes` on a char boundary, appending `"\n[truncated]"`
/// when truncated so callers can enforce per-item and total context budgets safely.
pub fn cap_section_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    const SUFFIX: &str = "\n[truncated]";
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let budget = max_bytes.saturating_sub(SUFFIX.len());
    let prefix = utf8_prefix(text, budget);
    let mut out = String::with_capacity(prefix.len() + SUFFIX.len());
    out.push_str(prefix);
    out.push_str(SUFFIX);
    (out, true)
}

pub const DEFAULT_MAX_BYTES: usize = 32 * 1024;
pub const DEFAULT_MAX_DEPTH: usize = 4;
pub const DEFAULT_MAX_LINES: usize = 200;
pub const DEFAULT_MAX_HITS: usize = 100;

const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = MAX_FILE_BYTES;
const MAX_INDEX_LINES: usize = 4_000;
const MAX_GREP_HITS: usize = 10_000;
const MAX_GREP_FILES: usize = 2_000;
const MAX_GREP_READ_BYTES: usize = 32 * 1024 * 1024;
const MAX_GREP_LINE_CHARS: usize = 2_000;

const GENERATED_GLOBS: &[&str] = &[
    "**/*.lock",
    "**/package-lock.json",
    "**/pnpm-lock.yaml",
    "**/npm-shrinkwrap.json",
    "**/bun.lockb",
    "**/*.min.js",
    "**/*.min.css",
    "**/*.map",
];

fn generated(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            GENERATED_GLOBS
                .iter()
                .map(|pattern| pattern.trim_start_matches("**/"))
                .any(|pattern| match pattern.strip_prefix('*') {
                    Some(suffix) => name.ends_with(suffix),
                    None => name == pattern,
                })
        })
}

fn walk(path: &Path, max_depth: Option<usize>) -> Walk {
    WalkBuilder::new(path)
        .max_depth(max_depth)
        .hidden(false)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | "node_modules" | "dist")
                ) && !generated(entry.path())
        })
        .build()
}

fn read_text(path: &Path) -> Result<String, String> {
    let mut remaining = MAX_FILE_BYTES + 1;
    read_text_bounded(path, &mut remaining)
}

fn read_text_bounded(path: &Path, remaining: &mut usize) -> Result<String, String> {
    let path_error = |error| format!("{}: {error}", path.display());
    let file = file::open(path).map_err(path_error)?;
    let metadata = file.metadata().map_err(path_error)?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(format!("{} exceeds the 2 MiB file limit", path.display()));
    }
    if metadata.len() > *remaining as u64 {
        *remaining = 0;
        return Err("grep read limit reached".into());
    }
    let limit = (MAX_FILE_BYTES + 1).min(*remaining);
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let result = file.take(limit as u64).read_to_end(&mut bytes);
    *remaining -= bytes.len();
    result.map_err(path_error)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!("{} exceeds the 2 MiB file limit", path.display()));
    }
    if bytes.len() == limit && metadata.len() < bytes.len() as u64 {
        return Err("grep read limit reached".into());
    }
    if bytes.contains(&0) {
        return Err(format!("{} is a binary file", path.display()));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", path.display()))
}

pub(crate) fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn line_range(start: usize, end: usize) -> String {
    format!("L{start}-L{end}")
}

/// Index declarations in one file or the text files below a directory.
pub fn code_index(path: &Path, max_depth: usize, max_lines: usize) -> Result<Value, String> {
    let mut remaining = max_lines.min(MAX_INDEX_LINES);
    let mut files = Vec::new();
    let mut truncated = false;
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        for entry in walk(path, Some(max_depth)).filter_map(Result::ok) {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(source) = read_text(entry.path()) else {
                continue;
            };
            if remaining == 0 {
                truncated = true;
                break;
            }
            let label = entry.path().strip_prefix(path).unwrap_or(entry.path());
            let (file, clipped) = index_file(entry.path(), label, &source, &mut remaining);
            files.push(file);
            truncated |= clipped;
        }
    } else {
        let source = read_text(path)?;
        if remaining == 0 {
            truncated = true;
        } else {
            let (file, clipped) = index_file(path, path, &source, &mut remaining);
            files.push(file);
            truncated |= clipped;
        }
    }
    Ok(json!({"files": files, "truncated": truncated}))
}

fn parse_file_index_cached(grammar: Option<&Grammar>, source: &str) -> CachedFileIndex {
    let key: IndexCacheKey = (grammar.map(|g| g.name), content_hash(source), source.len());
    {
        let cache = index_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
    }
    let mut declarations = Vec::new();
    let mut outline = Vec::new();
    if let Some(tree) = grammar.and_then(|grammar| grammar.parse(source)) {
        for symbol in symbols(&tree, source) {
            let (start, end) = symbol.lines();
            declarations.push(json!({
                "kind": symbol.kind,
                "name": utf8_prefix(symbol.name, 240),
                "signature": symbol.signature(source),
                "range": line_range(start, end),
            }));
        }
    } else {
        for (index, line) in source.lines().enumerate() {
            let heading = line.trim_start();
            if !heading.starts_with('[')
                && !(heading.starts_with('#') && heading.trim_start_matches('#').starts_with(' '))
            {
                continue;
            }
            outline.push(format!("L{}: {}", index + 1, utf8_prefix(heading, 240)));
        }
    }
    let entry = CachedFileIndex {
        declarations,
        outline,
    };
    let mut cache = index_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= INDEX_CACHE_MAX_ENTRIES {
        cache.clear();
    }
    cache.insert(key, entry.clone());
    entry
}

fn index_file(path: &Path, label: &Path, source: &str, remaining: &mut usize) -> (Value, bool) {
    let grammar = Grammar::for_path(path);
    let cached = parse_file_index_cached(grammar.as_ref(), source);
    let mut declarations = Vec::new();
    let mut outline = Vec::new();
    let mut truncated = false;
    if !cached.declarations.is_empty() {
        for decl in cached.declarations {
            if *remaining == 0 {
                truncated = true;
                break;
            }
            declarations.push(decl);
            *remaining -= 1;
        }
    } else {
        for line in cached.outline {
            if *remaining == 0 {
                truncated = true;
                break;
            }
            outline.push(line);
            *remaining -= 1;
        }
    }
    if declarations.is_empty() && outline.is_empty() {
        *remaining -= 1;
    }
    let mut file = json!({
        "path": label.to_string_lossy(),
        "language": grammar.map(|grammar| grammar.name),
        "lines": source.lines().count(),
    });
    if !declarations.is_empty() {
        file["symbols"] = Value::Array(declarations);
    }
    if !outline.is_empty() {
        file["outline"] = json!(outline);
    }
    (file, truncated)
}

/// Read an exact declaration or a 1-based inclusive range of file lines.
pub fn code_slice(
    path: &Path,
    symbol: Option<&str>,
    start: Option<usize>,
    end: Option<usize>,
    max_bytes: usize,
) -> Result<Value, String> {
    if symbol.is_some() && (start.is_some() || end.is_some()) {
        return Err("choose either a symbol or a line range".into());
    }
    let source = read_text(path)?;
    let (source, start, end) = if let Some(name) = symbol {
        let grammar = Grammar::for_path(path).ok_or("no tree-sitter grammar for this file")?;
        let tree = grammar.parse(&source).ok_or("could not parse file")?;
        let symbol = symbols(&tree, &source)
            .find(|symbol| symbol.name == name)
            .ok_or_else(|| format!("symbol {name:?} not found"))?;
        let (start, end) = symbol.lines();
        (&source[symbol.node.byte_range()], start, end)
    } else {
        let start = start.unwrap_or(1);
        if start == 0 || end.is_some_and(|end| end < start) {
            return Err("line ranges are 1-based and require start <= end".into());
        }
        let line_count = source.lines().count();
        (
            &source[..],
            start,
            end.unwrap_or(line_count).min(line_count),
        )
    };
    let skip = if symbol.is_some() { 0 } else { start - 1 };
    let count = if end < start { 0 } else { end - start + 1 };
    let max_bytes = max_bytes.min(MAX_OUTPUT_BYTES);
    let mut text = String::new();
    let mut truncated = false;
    for (offset, line) in source.lines().skip(skip).take(count).enumerate() {
        let prefix = format!(
            "{}L{}: ",
            if offset == 0 { "" } else { "\n" },
            start + offset
        );
        let available = max_bytes - text.len();
        if prefix.len() > available {
            truncated = true;
            break;
        }
        text.push_str(&prefix);
        let visible = utf8_prefix(line, available - prefix.len());
        text.push_str(visible);
        if visible.len() < line.len() {
            truncated = true;
            break;
        }
    }
    Ok(json!({
        "path": path.to_string_lossy(),
        "range": (count > 0).then(|| line_range(start, end)),
        "text": text,
        "truncated": truncated,
    }))
}

/// Search UTF-8 source lines and identify the declaration containing each match.
pub fn code_grep(
    pattern: &str,
    path: &Path,
    glob: Option<&str>,
    max_hits: usize,
    max_bytes: usize,
) -> Result<Value, String> {
    let regex = Regex::new(pattern).map_err(|error| error.to_string())?;
    let glob = glob
        .map(|pattern| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map(|glob| (glob.compile_matcher(), !pattern.contains('/')))
        })
        .transpose()
        .map_err(|error| error.to_string())?;
    let max_hits = max_hits.min(MAX_GREP_HITS);
    let max_bytes = max_bytes.min(MAX_OUTPUT_BYTES);
    let mut hits = 0;
    let mut bytes = 0;
    let mut truncated = false;
    let mut files = Vec::new();
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let entries = if metadata.is_dir() {
        Box::new(
            walk(path, None)
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                .map(ignore::DirEntry::into_path),
        ) as Box<dyn Iterator<Item = _>>
    } else {
        Box::new(std::iter::once(path.to_owned()))
    };
    let base = if metadata.is_dir() {
        path
    } else {
        path.parent().unwrap_or(Path::new(""))
    };
    let mut remaining = MAX_GREP_READ_BYTES;
    for (scanned, file) in entries.enumerate() {
        if scanned == MAX_GREP_FILES || remaining == 0 {
            truncated = true;
            break;
        }
        let label = file.strip_prefix(base).unwrap_or(&file);
        if generated(&file)
            || glob.as_ref().is_some_and(|(glob, by_name)| {
                let candidate = if *by_name {
                    label.file_name().map(Path::new).unwrap_or(label)
                } else {
                    label
                };
                !glob.is_match(candidate)
            })
        {
            continue;
        }
        let source = match read_text_bounded(&file, &mut remaining) {
            Ok(source) => source,
            Err(_) if remaining == 0 => {
                truncated = true;
                break;
            }
            Err(_) => continue,
        };
        let mut output_full = false;
        let mut tree = None;
        let mut matches = Vec::new();
        let mut offset = 0;
        for (index, line) in source.split_inclusive('\n').enumerate() {
            let text = line.lines().next().unwrap_or_default();
            let line_start = offset;
            offset += line.len();
            if text.len() > MAX_GREP_LINE_CHARS && text.chars().nth(MAX_GREP_LINE_CHARS).is_some() {
                truncated = true;
                continue;
            }
            let Some(found) = regex.find(text) else {
                continue;
            };
            if hits == max_hits {
                output_full = true;
                break;
            }
            let tree = tree.get_or_insert_with(|| {
                Grammar::for_path(&file).and_then(|grammar| grammar.parse(&source))
            });
            let matched_line_tail = &text[found.start()..];
            let column =
                found.start() + matched_line_tail.len() - matched_line_tail.trim_start().len();
            let symbol = tree
                .as_ref()
                .and_then(|tree| enclosing_symbol(tree, &source, line_start + column))
                .map(|symbol| format!("{} {}", symbol.kind, utf8_prefix(symbol.name, 240)));
            let hit = json!({"line": index + 1, "text": text, "symbol": symbol});
            let cost = hit.to_string().len()
                + 1
                + if matches.is_empty() {
                    json!(label.to_string_lossy()).to_string().len() + 24
                } else {
                    0
                };
            if cost > max_bytes - bytes {
                output_full = true;
                break;
            }
            bytes += cost;
            hits += 1;
            matches.push(hit);
        }
        if !matches.is_empty() {
            files.push(json!({"path": label.to_string_lossy(), "matches": matches}));
        }
        if output_full {
            truncated = true;
            break;
        }
    }
    Ok(json!({"files": files, "hits": hits, "truncated": truncated}))
}

#[cfg(test)]
mod tests;
