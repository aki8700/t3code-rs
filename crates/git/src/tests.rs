use super::*;
use std::fs;

#[tokio::test]
async fn worktree_checkout_preserves_raw_content_with_configured_filters() {
    let fixture = TempRepo::new().await;
    fixture.write(
        ".gitattributes",
        "lfs.txt filter=lfs\ncustom.txt filter=fixture\n",
    );
    fixture.write("lfs.txt", "stored lfs text\n");
    fixture.write("custom.txt", "stored custom text\n");
    fixture.commit().await;

    for filter in ["lfs", "fixture"] {
        git_output(
            &fixture.repo,
            &["config", &format!("filter.{filter}.required"), "true"],
        )
        .await
        .unwrap();
        for setting in ["smudge", "clean", "process"] {
            git_output(
                &fixture.repo,
                &["config", &format!("filter.{filter}.{setting}"), "/dev/null"],
            )
            .await
            .unwrap();
        }
    }
    let manager = GitManager::new(fixture.root.join("worktrees"));
    let (worktree, _) = manager
        .create_worktree(&fixture.repo, "filters", None)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(worktree.join("lfs.txt")).unwrap(),
        "stored lfs text\n"
    );
    assert_eq!(
        fs::read_to_string(worktree.join("custom.txt")).unwrap(),
        "stored custom text\n"
    );
    fs::write(worktree.join("custom.txt"), "changed\n").unwrap();
    assert!(
        !GitManager::diff_summary(&worktree, false)
            .await
            .unwrap()
            .clean
    );
}

#[tokio::test]
async fn worktree_include_copies_only_listed_ignored_files() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fixture = TempRepo::new().await;
    let write = |name: &str, content: &str| {
        let path = fixture.repo.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    write(
        ".gitignore",
        ".env\n*.local\nvendor/\ndeps/\n.claude/\nlink-*\nlogs/*\n",
    );
    write(
        ".worktreeinclude",
        ".env\ntracked.txt\nvendor/**/config.json\n**/settings.json\n\
         **/.claude/skills/*.md\nlink-*\nlogs/\n!logs/skip.log\n",
    );
    write("tracked.txt", "committed\n");
    fixture.commit().await;
    write("tracked.txt", "dirty\n");
    let copied = [
        ".env",
        "sub/.env",
        "vendor/a/config.json",
        ".claude/skills/a.md",
        "logs/keep.log",
    ];
    // `logs/skip.log` stays included: Git cannot re-exclude below a matched directory.
    for name in copied {
        write(name, name);
    }
    write("logs/skip.log", "skip");
    write("other.local", "unlisted");
    write("deps/pkg/settings.json", "unreached by **/");
    write("untracked.txt", "not ignored");
    fs::set_permissions(fixture.repo.join(".env"), fs::Permissions::from_mode(0o750)).unwrap();
    fs::write(fixture.root.join("outside"), "secret").unwrap();
    symlink(fixture.root.join("outside"), fixture.repo.join("link-out")).unwrap();
    symlink(".git/config", fixture.repo.join("link-git")).unwrap();
    symlink(".env", fixture.repo.join("link-in")).unwrap();

    let manager = GitManager::new(fixture.root.join("worktrees"));
    // A subdirectory of the checkout resolves to the same repository root.
    let (worktree, _) = manager
        .create_worktree(&fixture.repo.join("sub"), "include", None)
        .await
        .unwrap();

    for name in copied {
        assert_eq!(fs::read_to_string(worktree.join(name)).unwrap(), name);
    }
    assert!(worktree.join("logs/skip.log").is_file());
    let mode = fs::metadata(worktree.join(".env")).unwrap().permissions();
    assert_eq!(mode.mode() & 0o777, 0o750);
    assert_eq!(
        fs::read_to_string(worktree.join("link-in")).unwrap(),
        ".env"
    );
    for name in [
        "other.local",
        "deps/pkg/settings.json",
        "untracked.txt",
        "link-out",
        "link-git",
    ] {
        assert!(worktree.join(name).symlink_metadata().is_err(), "{name}");
    }
    assert_eq!(
        fs::read_to_string(worktree.join("tracked.txt")).unwrap(),
        "committed\n"
    );
}

#[tokio::test]
async fn worktree_include_is_optional_and_respects_the_checked_out_revision() {
    let fixture = TempRepo::new().await;
    fixture.write("tracked.txt", "committed\n");
    fixture.commit().await;
    fixture.write(".gitignore", ".env\n");
    fixture.write(".env", "secret");
    let manager = GitManager::new(fixture.root.join("worktrees"));
    let (without, _) = manager
        .create_worktree(&fixture.repo, "none", None)
        .await
        .unwrap();
    assert!(!without.join(".env").exists());

    // The ignore rule is uncommitted, so a copy would show up as a new file.
    fixture.write(".worktreeinclude", ".env\n");
    let (unignored, _) = manager
        .create_worktree(&fixture.repo, "unignored", None)
        .await
        .unwrap();
    assert!(!unignored.join(".env").exists());
}

struct TempRepo {
    root: PathBuf,
    repo: PathBuf,
}

impl TempRepo {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("t3-git-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let fixture = Self { root, repo };
        git_output(&fixture.repo, &["init", "--initial-branch=main"])
            .await
            .unwrap();
        fixture
    }

    fn write(&self, name: &str, content: &str) {
        fs::write(self.repo.join(name), content).unwrap();
    }

    async fn commit(&self) {
        git_output(&self.repo, &["add", "--", "."]).await.unwrap();
        git_output(
            &self.repo,
            &[
                "-c",
                "user.name=Git test",
                "-c",
                "user.email=git-test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-m",
                "Initial files",
            ],
        )
        .await
        .unwrap();
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn worktree_isolation_and_whitespace_diff() {
    let fixture = TempRepo::new().await;
    let repo = &fixture.repo;
    assert!(!GitManager::is_git_repo(&fixture.root).await);
    assert!(GitManager::is_git_repo(repo).await);
    fixture.write("changed.txt", "base\n");
    fixture.write("whitespace.txt", "one two\n");
    fixture.commit().await;

    let manager = GitManager::new(fixture.root.join("worktrees"));
    let (first, branch) = manager
        .create_worktree(repo, " feature/調査.. ", Some("main"))
        .await
        .unwrap();
    let (second, second_branch) = manager
        .create_worktree(repo, " feature/調査.. ", None)
        .await
        .unwrap();
    let (third, third_branch) = manager.create_worktree(repo, "", None).await.unwrap();
    assert!(branch.starts_with("t3rs/feature-"));
    assert!(third_branch.starts_with("t3rs/worktree-"));
    assert_ne!(first, second);
    assert_ne!(branch, second_branch);
    assert_eq!(
        GitManager::current_branch(&first).await.unwrap().as_deref(),
        Some(branch.as_str())
    );
    let branches = GitManager::list_branches(repo).await.unwrap();
    for expected in ["main", &branch, &second_branch, &third_branch] {
        assert!(branches.iter().any(|branch| branch == expected));
    }

    fs::write(first.join("whitespace.txt"), "one   two\n").unwrap();
    assert!(GitManager::diff_summary(&first, true).await.unwrap().clean);
    assert!(!GitManager::diff_summary(&first, false).await.unwrap().clean);

    fs::write(first.join("changed.txt"), "base\nstaged\n").unwrap();
    git_output(&first, &["add", "--", "changed.txt"])
        .await
        .unwrap();
    fs::write(first.join("changed.txt"), "base\nstaged\nunstaged\n").unwrap();
    fs::write(first.join("new file.txt"), "line 1\nline 2").unwrap();
    let visible = GitManager::diff_summary(&first, false).await.unwrap();
    assert_eq!((visible.total_additions, visible.total_deletions), (5, 1));
    let filtered = GitManager::diff_summary(&first, true).await.unwrap();
    assert!(!filtered.clean);
    assert_eq!(filtered.files.len(), 2);
    assert_eq!((filtered.total_additions, filtered.total_deletions), (4, 0));
    let tracked = filtered
        .files
        .iter()
        .find(|file| file.path == "changed.txt")
        .unwrap();
    assert_eq!(tracked.status, "MM");
    assert_eq!(tracked.additions, 2);
    assert!(tracked.patch.contains("+staged\n+unstaged"));
    let untracked = filtered
        .files
        .iter()
        .find(|file| file.path == "new file.txt")
        .unwrap();
    assert_eq!(
        (
            untracked.status.as_str(),
            untracked.additions,
            untracked.deletions
        ),
        ("??", 2, 0)
    );
    assert!(untracked.patch.contains("+line 1\n+line 2"));
    assert_eq!(
        fs::read_to_string(repo.join("changed.txt")).unwrap(),
        "base\n"
    );
    assert!(GitManager::diff_summary(repo, false).await.unwrap().clean);
    assert!(
        GitManager::diff_summary(&second, false)
            .await
            .unwrap()
            .clean
    );

    for path in [first, second, third] {
        manager.remove_worktree(repo, &path).await.unwrap();
        assert!(!path.exists());
    }
    let worktrees = git_output(repo, &["worktree", "list", "--porcelain"])
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&worktrees)
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1
    );
}

#[tokio::test]
async fn staged_rename_and_literal_filenames_from_nested_directory() {
    let fixture = TempRepo::new().await;
    let repo = &fixture.repo;
    fs::create_dir(repo.join("nested")).unwrap();
    fixture.write("old name.txt", "unchanged\n".repeat(20).as_str());
    fixture.write("nested/[literal].txt", "one\n");
    fixture.commit().await;
    git_output(repo, &["mv", "--", "old name.txt", "-new name.txt"])
        .await
        .unwrap();
    fixture.write(
        "-new name.txt",
        format!("{}extra\n", "unchanged\n".repeat(20)).as_str(),
    );
    fixture.write("nested/[literal].txt", "one\ntwo\n");
    git_output(
        repo,
        &["--literal-pathspecs", "add", "--", "nested/[literal].txt"],
    )
    .await
    .unwrap();
    let diff = GitManager::diff_summary(&repo.join("nested"), false)
        .await
        .unwrap();
    assert_eq!(diff.files.len(), 2);
    assert_eq!((diff.total_additions, diff.total_deletions), (2, 0));
    let renamed = diff
        .files
        .iter()
        .find(|file| file.path == "-new name.txt")
        .unwrap();
    assert_eq!(renamed.status, "RM");
    assert_eq!((renamed.additions, renamed.deletions), (1, 0));
    assert!(renamed.patch.contains("rename from old name.txt"));
    assert!(renamed.patch.contains("rename to -new name.txt"));
    assert!(
        diff.files
            .iter()
            .any(|file| file.path == "nested/[literal].txt" && file.status == "M ")
    );
    git_output(repo, &["checkout", "--detach"]).await.unwrap();
    assert!(GitManager::current_branch(repo).await.unwrap().is_none());
}

#[test]
fn porcelain_consumes_both_rename_and_copy_paths() {
    let entries = status_entries(b"R  new\tname\0old\nname\0C  copy\0original\0?? new\0").unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].path, b"new\tname");
    assert_eq!(entries[0].rename_from, Some(b"old\nname".as_slice()));
    assert_eq!(entries[1].path, b"copy");
    assert_eq!(entries[1].status, b"C ");
    assert_eq!(entries[2].path, b"new");
    assert!(status_entries(b"R  new\0").is_err());
    assert_eq!(
        numstat(b"1\t2\t\0old\0new\x000\t1\tplain\0").unwrap(),
        (1, 3)
    );
}

#[tokio::test]
async fn unborn_git_includes_staged_and_untracked_files_and_caps_patch() {
    let fixture = TempRepo::new().await;
    fixture.write("-", "untracked\n\nlast");
    let blank = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        GitManager::diff_summary(&fixture.repo, false),
    )
    .await
    .expect("a file named - must not be read from stdin")
    .unwrap();
    assert_eq!(blank.files[0].path, "-");
    assert_eq!(blank.files[0].additions, 3);
    assert!(blank.files[0].patch.contains("+untracked"));

    fixture.write("staged.txt", "staged\n");
    git_output(&fixture.repo, &["add", "staged.txt"])
        .await
        .unwrap();
    fixture.write("staged.txt", "staged\nunstaged\n");
    fixture.write("long.txt", &"文字起こし\n".repeat(20_000));
    let diff = GitManager::diff_summary(&fixture.repo, false)
        .await
        .unwrap();
    assert_eq!(diff.files.len(), 3);
    assert_eq!((diff.total_additions, diff.total_deletions), (20_005, 0));
    let staged = diff
        .files
        .iter()
        .find(|file| file.path == "staged.txt")
        .unwrap();
    assert_eq!(staged.status, "AM");
    assert!(staged.patch.contains("+staged\n+unstaged"));
    let long = diff
        .files
        .iter()
        .find(|file| file.path == "long.txt")
        .unwrap();
    assert_eq!(long.additions, 20_000);
    assert!((PATCH_LIMIT - 3..=PATCH_LIMIT).contains(&long.patch.len()));
}

#[tokio::test]
async fn git_overrides_repository_hooks_and_fsmonitor_configuration() {
    let fixture = TempRepo::new().await;
    for (key, configured, effective) in [
        ("core.hooksPath", "repo-hooks", "/dev/null\n"),
        ("core.fsmonitor", "repo-monitor", "false\n"),
    ] {
        let status = Command::new("git")
            .current_dir(&fixture.repo)
            .args(["config", "--local", key, configured])
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(
            git_output(&fixture.repo, &["config", "--get", key])
                .await
                .unwrap(),
            effective.as_bytes()
        );
    }
    let command = git(&fixture.repo);
    let env: std::collections::HashMap<_, _> = command.as_std().get_envs().collect();
    for (key, value) in [("GIT_OPTIONAL_LOCKS", "0"), ("GIT_CONFIG_NOSYSTEM", "1")] {
        assert_eq!(env.get(OsStr::new(key)), Some(&Some(OsStr::new(value))));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn untracked_binary_and_symlink_are_reported_without_reading_the_link_target() {
    let fixture = TempRepo::new().await;
    let outside = fixture.root.join("outside.txt");
    fs::write(&outside, "outside content\nsecond line\n").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.repo.join("link")).unwrap();
    fs::write(fixture.repo.join("binary"), b"\0\xff\n").unwrap();
    let diff = GitManager::diff_summary(&fixture.repo, true).await.unwrap();
    assert_eq!(diff.files.len(), 2);
    let binary = diff
        .files
        .iter()
        .find(|file| file.path == "binary")
        .unwrap();
    assert_eq!(binary.additions, 0);
    assert!(binary.patch.contains("Binary files"));
    let link = diff.files.iter().find(|file| file.path == "link").unwrap();
    assert_eq!(link.additions, 1);
    assert!(!link.patch.contains("outside content"));
}

async fn snapshot_refs(repo: &Path) -> Vec<String> {
    let out = git_output(
        repo,
        &["for-each-ref", "--format=%(refname)", "refs/t3/snapshots/"],
    )
    .await
    .unwrap();
    String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn show(repo: &Path, spec: String) -> CoreResult<Vec<u8>> {
    git_output(repo, &["show", &spec]).await
}

#[tokio::test]
async fn remove_worktree_pins_uncommitted_work_under_snapshot_ref() {
    let fixture = TempRepo::new().await;
    fixture.write(".gitignore", "ignored.log\n");
    fixture.write("tracked.txt", "base\n");
    fixture.commit().await;
    let manager = GitManager::new(fixture.root.join("worktrees"));
    let (worktree, branch) = manager
        .create_worktree(&fixture.repo, "dirty", None)
        .await
        .unwrap();
    fs::write(worktree.join("tracked.txt"), "edited\n").unwrap();
    fs::write(worktree.join("new.txt"), "untracked\n").unwrap();
    fs::write(worktree.join("ignored.log"), "noise\n").unwrap();

    let snapshot = manager
        .remove_worktree(&fixture.repo, &worktree)
        .await
        .unwrap()
        .expect("dirty worktree is snapshotted");

    assert!(!worktree.exists());
    assert_eq!(snapshot_refs(&fixture.repo).await, vec![snapshot.clone()]);
    assert!(snapshot.starts_with("refs/t3/snapshots/t3rs-dirty-"));
    let repo = &fixture.repo;
    assert_eq!(
        show(repo, format!("{snapshot}:tracked.txt")).await.unwrap(),
        b"edited\n"
    );
    assert_eq!(
        show(repo, format!("{snapshot}:new.txt")).await.unwrap(),
        b"untracked\n"
    );
    assert!(show(repo, format!("{snapshot}:ignored.log")).await.is_err());
    // The snapshot sits on the branch head, and the branch itself is kept.
    let parent = git_output(repo, &["rev-parse", &format!("{snapshot}^")])
        .await
        .unwrap();
    let head = git_output(repo, &["rev-parse", &branch]).await.unwrap();
    assert_eq!(parent, head);
}

#[tokio::test]
async fn new_snapshot_prunes_expired_refs_and_keeps_younger_ones() {
    let fixture = TempRepo::new().await;
    fixture.write("tracked.txt", "base\n");
    fixture.commit().await;
    let now = chrono::Utc::now();
    let expired = format!(
        "refs/t3/snapshots/expired-{}",
        (now - chrono::Duration::days(15)).format("%Y%m%dT%H%M%S%3fZ")
    );
    let recent = format!(
        "refs/t3/snapshots/recent-{}",
        (now - chrono::Duration::days(13)).format("%Y%m%dT%H%M%S%3fZ")
    );
    for reference in [&expired, &recent] {
        git_output(&fixture.repo, &["update-ref", reference, "HEAD"])
            .await
            .unwrap();
    }

    let manager = GitManager::new(fixture.root.join("worktrees"));
    let (worktree, _) = manager
        .create_worktree(&fixture.repo, "retention", None)
        .await
        .unwrap();
    fs::write(worktree.join("tracked.txt"), "edited\n").unwrap();
    let snapshot = manager
        .remove_worktree(&fixture.repo, &worktree)
        .await
        .unwrap()
        .expect("dirty worktree is snapshotted");

    let refs = snapshot_refs(&fixture.repo).await;
    assert!(!refs.contains(&expired));
    assert!(refs.contains(&recent));
    assert!(refs.contains(&snapshot));
    assert_eq!(refs.len(), 2);
}

#[tokio::test]
async fn remove_worktree_skips_snapshot_for_clean_worktree() {
    let fixture = TempRepo::new().await;
    fixture.write(".gitignore", "ignored.log\n");
    fixture.write("tracked.txt", "base\n");
    fixture.commit().await;
    let manager = GitManager::new(fixture.root.join("worktrees"));
    let (worktree, _) = manager
        .create_worktree(&fixture.repo, "clean", None)
        .await
        .unwrap();
    fs::write(worktree.join("ignored.log"), "noise\n").unwrap();

    let snapshot = manager
        .remove_worktree(&fixture.repo, &worktree)
        .await
        .unwrap();

    assert_eq!(snapshot, None);
    assert!(!worktree.exists());
    assert!(snapshot_refs(&fixture.repo).await.is_empty());
}

#[tokio::test]
async fn remove_worktree_keeps_worktree_when_snapshot_fails() {
    let fixture = TempRepo::new().await;
    fixture.write("tracked.txt", "base\n");
    fixture.commit().await;
    // A ref named refs/t3/snapshots blocks every ref below it (directory/file conflict).
    git_output(&fixture.repo, &["update-ref", "refs/t3/snapshots", "HEAD"])
        .await
        .unwrap();
    let manager = GitManager::new(fixture.root.join("worktrees"));
    let (worktree, _) = manager
        .create_worktree(&fixture.repo, "blocked", None)
        .await
        .unwrap();
    fs::write(worktree.join("tracked.txt"), "edited\n").unwrap();

    let error = manager
        .remove_worktree(&fixture.repo, &worktree)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("refs/t3/snapshots"), "{error}");

    assert_eq!(
        fs::read_to_string(worktree.join("tracked.txt")).unwrap(),
        "edited\n"
    );
}
