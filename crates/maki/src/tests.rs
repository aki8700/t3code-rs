use super::*;
use std::process::Command;
use tempfile::{TempDir, tempdir};

fn fixture(dir: &TempDir, name: &str, text: impl AsRef<[u8]>) -> std::path::PathBuf {
    let path = dir.path().join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    path
}

#[test]
fn indexes_rust_signatures_with_ranges() {
    let dir = tempdir().unwrap();
    let path = fixture(
        &dir,
        "lib.rs",
        "pub trait Speak {\n    fn speak(&self);\n}\nstruct Bird;\nenum Color { Red }\nimpl Speak for Bird {\n    fn speak(&self) {\n        println!(\"hello\");\n    }\n}\n",
    );
    let index = code_index(&path, 0, 20).unwrap();
    let symbols = index["files"][0]["symbols"].as_array().unwrap();
    assert_eq!(symbols[0]["kind"], "trait");
    assert_eq!(symbols[0]["range"], "L1-L3");
    assert_eq!(symbols[1]["signature"], "fn speak(&self);");
    assert_eq!(symbols[2]["kind"], "struct");
    assert_eq!(symbols[3]["kind"], "enum");
    assert_eq!(symbols[4]["signature"], "impl Speak for Bird");
    assert_eq!(symbols[5]["range"], "L7-L9");
    assert_eq!(symbols[5]["signature"], "fn speak(&self)");
    let limited = code_index(&path, 0, 1).unwrap();
    assert_eq!(limited["files"][0]["symbols"].as_array().unwrap().len(), 1);
    assert_eq!(limited["truncated"], true);
}

#[test]
fn indexes_typescript_and_python_declarations() {
    let dir = tempdir().unwrap();
    let ts = fixture(
        &dir,
        "main.ts",
        "export interface Named { name: string; }\nexport class Thing {\n  run(n: number): number { return n + 1; }\n}\nexport const calculate = (x: number) => x * 2;\nexport { other } from './other';\nexport const version = 1;\n",
    );
    let py = fixture(
        &dir,
        "main.py",
        "class Thing:\n    def run(self, n):\n        return n + 1\n\ndef build():\n    return Thing()\n",
    );
    for (path, expected) in [
        (&ts, &["Named", "Thing", "run", "calculate"][..]),
        (&py, &["Thing", "run", "build"][..]),
    ] {
        let index = code_index(path, 0, 20).unwrap();
        let symbols = index["files"][0]["symbols"].as_array().unwrap();
        for (symbol, expected) in symbols.iter().zip(expected) {
            assert_eq!(symbol["name"], *expected);
        }
        assert!(symbols.len() >= expected.len());
    }
    let index = code_index(&ts, 0, 20).unwrap();
    assert_eq!(index["files"][0]["symbols"][1]["range"], "L2-L4");
    assert_eq!(index["files"][0]["symbols"][4]["kind"], "export");
    assert_eq!(index["files"][0]["symbols"][5]["name"], "version");
    assert!(
        index["files"][0]["symbols"][3]["signature"]
            .as_str()
            .unwrap()
            .starts_with("export const calculate")
    );
    let index = code_index(&py, 0, 20).unwrap();
    assert_eq!(index["files"][0]["symbols"][1]["range"], "L2-L3");
}

#[test]
fn indexes_javascript_go_bash_and_json() {
    let dir = tempdir().unwrap();
    for (file, code, name, kind) in [
        ("app.js", "export function run() {}", "run", "fn"),
        (
            "app.go",
            "package main\ntype Bird struct { Name string }\n",
            "Bird",
            "struct",
        ),
        ("app.sh", "start() { echo hello; }\n", "start", "fn"),
        (
            "app.json",
            "{\"feature\": {\"enabled\": true}}",
            "feature",
            "key",
        ),
    ] {
        let path = fixture(&dir, file, code);
        let index = code_index(&path, 0, 20).unwrap();
        assert_eq!(index["files"][0]["symbols"][0]["name"], name, "{index}");
        assert_eq!(index["files"][0]["symbols"][0]["kind"], kind);
    }
}

#[test]
fn directory_index_skips_generated_binary_and_ignored_files() {
    let dir = tempdir().unwrap();
    for file in [
        "lib.rs",
        "nested/mod.rs",
        "target/build.rs",
        "node_modules/app.js",
        "dist/app.js",
        "Cargo.lock",
        "nested/pnpm-lock.yaml",
        ".git/config",
        "ignored.rs",
    ] {
        fixture(&dir, file, "fn visible() {}\n");
    }
    fixture(&dir, "binary.rs", b"abc\0\xff");
    fixture(&dir, ".gitignore", "ignored.rs\n");
    fixture(&dir, "README.md", "# Heading\n\nProse.\n");
    let index = code_index(dir.path(), 4, 20).unwrap();
    let files = index["files"].as_array().unwrap();
    let mut paths = files
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [".gitignore", "README.md", "lib.rs", "nested/mod.rs"]
    );
    let readme = files
        .iter()
        .find(|file| file["path"] == "README.md")
        .unwrap();
    assert_eq!(readme["outline"], json!(["L1: # Heading"]));
    let shallow = code_index(dir.path(), 1, 20).unwrap();
    assert_eq!(shallow["files"].as_array().unwrap().len(), 3);
}

#[test]
fn slices_exact_symbols_and_inclusive_lines() {
    let dir = tempdir().unwrap();
    let path = fixture(
        &dir,
        "lib.rs",
        "fn before() {}\nfn bird() {\n    say();\n} fn after() {}\n",
    );
    let symbol = code_slice(&path, Some("bird"), None, None, 100).unwrap();
    assert_eq!(symbol["text"], "L2: fn bird() {\nL3:     say();\nL4: }");
    assert_eq!(symbol["range"], "L2-L4");
    let lines = code_slice(&path, None, Some(3), Some(4), 100).unwrap();
    assert_eq!(lines["text"], "L3:     say();\nL4: } fn after() {}");
    assert!(code_slice(&path, Some("bir"), None, None, 100).is_err());
    assert!(code_slice(&path, Some("bird"), Some(1), None, 100).is_err());
    assert!(code_slice(&path, None, Some(0), None, 100).is_err());
}

#[test]
fn slices_utf8_and_rejects_oversized_files() {
    let dir = tempdir().unwrap();
    let path = fixture(&dir, "notes.txt", "鳥羽\r\nnext\n");
    let sliced = code_slice(&path, None, Some(1), Some(1), 8).unwrap();
    assert_eq!(sliced["text"], "L1: 鳥");
    assert_eq!(sliced["truncated"], true);
    let path = fixture(&dir, "large.rs", vec![b'x'; MAX_FILE_BYTES + 1]);
    assert!(code_index(&path, 0, 100).unwrap_err().contains("2 MiB"));
}

#[test]
fn grep_groups_hits_with_nested_symbols_and_skips_generated_files() {
    let dir = tempdir().unwrap();
    fixture(
        &dir,
        "src/main.rs",
        "struct Bird {\n    needle: bool,\n}\nimpl Bird {\n    fn fly() {\n        let needle = 1;\n    }\n}\n",
    );
    fixture(&dir, "app.py", "class Bird:\n    needle = 3\n");
    for file in [
        "pnpm-lock.yaml",
        "Cargo.lock",
        "package-lock.json",
        "target/output.rs",
    ] {
        fixture(&dir, file, "needle");
    }
    fixture(&dir, "binary.txt", b"needle\0");
    fixture(&dir, "minified.js", format!("needle{}", "x".repeat(2_000)));
    let result = code_grep("nee.le", dir.path(), None, 20, 4096).unwrap();
    assert_eq!(result["hits"], 3);
    let files = result["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let rust = files
        .iter()
        .find(|file| file["path"] == "src/main.rs")
        .unwrap();
    assert_eq!(rust["matches"][0]["symbol"], "struct Bird");
    assert_eq!(rust["matches"][1]["symbol"], "fn fly");
    assert_eq!(rust["matches"][1]["line"], 6);
    let leading_space = code_grep(r"^\s+fn fly", dir.path(), None, 20, 4096).unwrap();
    assert_eq!(leading_space["files"][0]["matches"][0]["symbol"], "fn fly");
    let py = files.iter().find(|file| file["path"] == "app.py").unwrap();
    assert_eq!(py["matches"][0]["symbol"], "class Bird");
    let limited = code_grep("needle", dir.path(), Some("**/*.rs"), 1, 4096).unwrap();
    assert_eq!(limited["files"][0]["path"], "src/main.rs");
    assert_eq!(limited["hits"], 1);
    assert_eq!(limited["truncated"], true);
    let by_name = code_grep("needle", dir.path(), Some("*.rs"), 10, 4096).unwrap();
    assert_eq!(by_name["hits"], 2);
    let limited = code_grep("needle", dir.path(), None, 10, 1).unwrap();
    assert_eq!(limited["hits"], 0);
    assert_eq!(limited["truncated"], true);
}

fn run_git(dir: &TempDir, args: &[&str]) {
    let output = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn diff_excludes_generated_files_and_counts_only_selected_changes() {
    let dir = tempdir().unwrap();
    run_git(&dir, &["init", "-q"]);
    let names = [
        "main.rs",
        "Cargo.lock",
        "nested/code.rs",
        "nested/pnpm-lock.yaml",
        "nested/app.min.js",
    ];
    for name in names {
        fixture(&dir, name, "before\n");
    }
    run_git(&dir, &["add", "."]);
    run_git(
        &dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ],
    );
    for name in names {
        fixture(&dir, name, "after 鳥\n");
    }
    let diff = code_diff(dir.path(), Some("HEAD"), None, 4096).unwrap();
    assert_eq!(diff["skipped_generated"], 3, "{diff}");
    assert!(
        diff["shortstat"]
            .as_str()
            .unwrap()
            .contains("2 files changed")
    );
    assert_eq!(diff["name_status"], "M\tmain.rs\nM\tnested/code.rs");
    let hunks = diff["hunks"].as_str().unwrap();
    assert!(hunks.contains("+after 鳥"));
    assert!(!hunks.contains("Cargo.lock"));
    let diff = code_diff(&dir.path().join("nested"), None, None, 4096).unwrap();
    assert_eq!(diff["name_status"], "M\tcode.rs");
    assert_eq!(diff["skipped_generated"], 2, "{diff}");
    fs::remove_file(dir.path().join("main.rs")).unwrap();
    let diff = code_diff_scoped(dir.path(), None, Some("main.rs"), 16, dir.path()).unwrap();
    assert_eq!(diff["name_status"], "D\tmain.rs");
    assert_eq!(diff["hunks"].as_str().unwrap().len(), 16);
    assert_eq!(diff["truncated"], true);
    assert_eq!(diff["skipped_generated"], 0);
    let path = dir.path().join("main.rs");
    let diff = code_diff_scoped(dir.path(), None, path.to_str(), 4096, dir.path()).unwrap();
    assert_eq!(diff["name_status"], "D\tmain.rs");
}

#[test]
fn diff_does_not_run_repo_clean_filters() {
    let dir = tempdir().unwrap();
    run_git(&dir, &["init", "-q"]);
    fixture(&dir, "main.rs", "before\n");
    run_git(&dir, &["add", "main.rs"]);
    fixture(&dir, ".gitattributes", "*.rs filter=custom\n");
    run_git(
        &dir,
        &["config", "filter.custom.clean", "touch filter-ran; cat"],
    );
    run_git(&dir, &["config", "filter.custom.required", "true"]);
    fixture(&dir, "main.rs", "after with a different length\n");
    let diff = code_diff(dir.path(), None, None, 4096).unwrap();
    assert!(diff["hunks"].as_str().unwrap().contains("+after"));
    assert!(!dir.path().join("filter-ran").exists());
}

#[test]
fn compaction_economics_match_reference_costs() {
    let mut input = CompactionEconInput {
        prefix: 10_000,
        replaced: 130_000,
        summary: 4_000,
        tail: 26_000,
        instruction: 500,
        summarizer_reuses_prefix: true,
        prices: Prices {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write: 1.25,
        },
        remaining_turns: None,
    };
    let result = compaction_econ(input);
    assert_eq!(result.one_time_cost, 69_000.0);
    assert_eq!(result.saving_per_turn, 12_600.0);
    assert_eq!(result.break_even_turns, Some(69_000.0 / 12_600.0));
    assert_eq!(result.worth_compacting, None);
    input.remaining_turns = result.break_even_turns;
    assert_eq!(compaction_econ(input).worth_compacting, Some(true));
    input.remaining_turns = Some(5.0);
    assert_eq!(compaction_econ(input).worth_compacting, Some(false));
    input.summarizer_reuses_prefix = false;
    input.prices.cache_write = 0.0;
    assert_eq!(compaction_econ(input).one_time_cost, 187_500.0);
    input.summary = input.replaced;
    let result = compaction_econ(input);
    assert_eq!(result.break_even_turns, None);
    assert_eq!(result.worth_compacting, Some(false));
}
