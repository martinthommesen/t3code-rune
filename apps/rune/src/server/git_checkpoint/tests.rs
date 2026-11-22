use std::fs;
use std::process::Command;

use uuid::Uuid;

use super::*;

const SCOPE: &str = "checkpoint-scope:thread:rust parity:name:root";

struct Repository(PathBuf);

impl Repository {
    fn folder() -> Self {
        let path = std::env::temp_dir().join(format!("t3-checkpoint-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn new() -> Self {
        let repository = Self::folder();
        repository.git(["init", "--initial-branch=main", "--template="]);
        repository.git(["config", "user.name", "Test user"]);
        repository.git(["config", "user.email", "checkpoint@example.test"]);
        repository.git(["config", "core.autocrlf", "false"]);
        repository.git(["config", "commit.gpgsign", "false"]);
        repository
    }

    fn with_head() -> Self {
        let repository = Self::new();
        repository.write("tracked.txt", "original\n");
        repository.write("branch.txt", "main\n");
        repository.git(["add", "."]);
        repository.git(["commit", "-m", "initial"]);
        repository
    }

    fn git<const N: usize>(&self, arguments: [&str; N]) -> String {
        let output = Command::new("git")
            .current_dir(&self.0)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_QUARANTINE_PATH")
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn assert_no_scratch(&self) {
        let objects = fs::read_dir(self.0.join(".git/objects")).unwrap();
        assert!(objects.into_iter().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("tmp_objdir-t3-checkpoint-")
        }));
    }

    fn input<'a>(&'a self, reference: &'a str) -> CaptureInput<'a> {
        CaptureInput {
            cwd: &self.0,
            checkpoint_ref: reference,
            previous_checkpoint_ref: None,
            exclude_paths: &[],
        }
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn refs_use_the_existing_scope_hash_and_ordinal() {
    assert_eq!(
        checkpoint_ref(SCOPE, 12),
        "refs/t3/orchestration-v2/checkpoints/MTU2ZWU4MDUxNDlkZjlhNTIzNDRjMDIyYzJmNzQzNDI/ordinal/12"
    );
}

#[tokio::test]
async fn captures_the_tree_while_leaving_the_working_index_and_head_alone() {
    let repo = Repository::with_head();
    repo.write("gone.txt", "original\n");
    repo.write("excluded/path.txt", "original exclusion\n");
    repo.write(".gitignore", "ignored.txt\n");
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add initial paths"]);
    let head = repo.git(["rev-parse", "HEAD"]);
    repo.write("tracked.txt", "staged\n");
    repo.git(["add", "tracked.txt"]);
    repo.git(["update-index", "--assume-unchanged", "tracked.txt"]);
    repo.write("tracked.txt", "working tree version\n");
    repo.write("excluded/path.txt", "leave this out\n");
    repo.write("ignored.txt", "leave this out\n");
    let large = "new file contents\n".repeat(128);
    repo.write("untracked.txt", &large);
    fs::remove_file(repo.0.join("gone.txt")).unwrap();
    repo.git(["config", "core.bigFileThreshold", "1"]);
    let index = fs::read(repo.0.join(".git/index")).unwrap();
    fs::write(repo.0.join(".git/index.lock"), "another Git writer").unwrap();

    let reference = checkpoint_ref(SCOPE, 0);
    let result = capture(CaptureInput {
        exclude_paths: &["excluded", "ignored.txt"],
        ..repo.input(&reference)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, CapturedCheckpoint::Created);
    assert_eq!(repo.git(["rev-parse", "HEAD"]), head);
    assert_eq!(
        repo.git(["show", &format!("{reference}:tracked.txt")]),
        "working tree version\n"
    );
    assert_eq!(
        repo.git(["show", &format!("{reference}:excluded/path.txt")]),
        "original exclusion\n"
    );
    assert_eq!(
        repo.git(["show", &format!("{reference}:untracked.txt")]),
        large
    );
    let paths = repo.git(["ls-tree", "-r", "--name-only", &reference]);
    assert!(
        !paths
            .lines()
            .any(|path| matches!(path, "gone.txt" | "ignored.txt"))
    );
    assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read_to_string(repo.0.join(".git/index.lock")).unwrap(),
        "another Git writer"
    );
    repo.assert_no_scratch();
}

#[tokio::test]
async fn racing_captures_publish_once_and_retries_read_the_same_snapshot() {
    let repo = Repository::with_head();
    let reference = checkpoint_ref(SCOPE, 1);
    let (first, second) = tokio::join!(
        capture(repo.input(&reference)),
        capture(repo.input(&reference))
    );
    let first = first.unwrap().unwrap();
    let second = second.unwrap().unwrap();
    assert_ne!(first, second);
    let saved_commit = repo.git(["rev-parse", &reference]);
    repo.write("tracked.txt", "edited after capture\n");
    let retry = capture(repo.input(&reference)).await.unwrap().unwrap();
    assert_eq!(retry, CapturedCheckpoint::Existing);
    assert_eq!(repo.git(["rev-parse", &reference]), saved_commit);
    assert_eq!(
        repo.git(["show", &format!("{reference}:tracked.txt")]),
        "original\n"
    );
    repo.assert_no_scratch();
}

#[tokio::test]
async fn captures_an_unborn_branch_and_identifies_a_plain_directory() {
    let reference = checkpoint_ref(SCOPE, 0);
    let plain = Repository::folder();
    assert!(!is_workspace(&plain.0).await.unwrap());
    assert!(!has_ref(&plain.0, &reference).await.unwrap());
    assert!(capture(plain.input(&reference)).await.unwrap().is_none());
    let repo = Repository::new();
    assert!(is_workspace(&repo.0).await.unwrap());
    assert!(!has_ref(&repo.0, &reference).await.unwrap());
    repo.write("initial.txt", "before any user commit\n");
    assert_eq!(
        capture(repo.input(&reference)).await.unwrap(),
        Some(CapturedCheckpoint::Created)
    );
    assert!(has_ref(&repo.0, &reference).await.unwrap());
    assert_eq!(repo.git(["show", "-s", "--format=%P", &reference]), "\n");
    assert_eq!(
        repo.git(["show", &format!("{reference}:initial.txt")]),
        "before any user commit\n"
    );
    repo.assert_no_scratch();
}

#[tokio::test]
async fn accounts_for_a_branch_switch_without_attributing_its_files_to_the_turn() {
    let repo = Repository::with_head();
    repo.git(["switch", "-c", "destination"]);
    repo.write("branch.txt", "destination\n");
    repo.git(["add", "branch.txt"]);
    repo.git(["commit", "-m", "branch changes"]);
    let destination = repo.git(["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(["switch", "main"]);
    repo.write("tracked.txt", "edited before switch\n");
    let from = checkpoint_ref(SCOPE, 0);
    assert_eq!(
        capture(repo.input(&from)).await.unwrap(),
        Some(CapturedCheckpoint::Created)
    );
    let previous = repo.git(["rev-parse", &from]);
    repo.git(["switch", "destination"]);
    repo.write("tracked.txt", "edited after switch\n");
    let to = checkpoint_ref(SCOPE, 1);
    let checkpoint = capture(CaptureInput {
        previous_checkpoint_ref: Some(&from),
        ..repo.input(&to)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(checkpoint, CapturedCheckpoint::Created);
    let parents = repo.git(["show", "-s", "--format=%P", &to]);
    let parents: Vec<_> = parents.split_whitespace().collect();
    assert_eq!(parents.len(), 2);
    assert_eq!(parents[0], destination);
    let turn_start = parents[1];
    assert_eq!(
        repo.git(["show", "-s", "--format=%P", turn_start]).trim(),
        format!("{destination} {}", previous.trim())
    );
    assert_eq!(
        repo.git(["show", &format!("{turn_start}:branch.txt")]),
        "destination\n"
    );
    assert_eq!(
        repo.git(["show", &format!("{turn_start}:tracked.txt")]),
        "edited before switch\n"
    );
    assert_eq!(
        repo.git(["show", &format!("{to}:tracked.txt")]),
        "edited after switch\n"
    );
    let files = summarize(DiffInput {
        cwd: &repo.0,
        from_checkpoint_ref: &from,
        to_checkpoint_ref: &to,
        exclude_paths: &[],
    })
    .await
    .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "tracked.txt");
    assert_eq!((files[0].additions, files[0].deletions), (1, 1));
    repo.assert_no_scratch();
}

#[tokio::test]
async fn ignored_nested_workspace_uses_only_its_own_tracked_paths() {
    let repo = Repository::with_head();
    repo.write(".gitignore", "nested/\n");
    repo.git(["add", ".gitignore"]);
    repo.git(["commit", "-m", "exclude the nested workspace"]);
    repo.write("nested/staged.txt", "staged addition\n");
    repo.git(["add", "-f", "nested/staged.txt"]);
    repo.write("nested/staged.txt", "working copy\n");
    repo.write("nested/untracked.txt", "ignored\n");
    repo.write("tracked.txt", "outside this workspace\n");
    let reference = checkpoint_ref(SCOPE, 0);
    let nested = repo.0.join("nested");
    capture(CaptureInput {
        cwd: &nested,
        ..repo.input(&reference)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        repo.git(["show", &format!("{reference}:nested/staged.txt")]),
        "working copy\n"
    );
    assert_eq!(
        repo.git(["show", &format!("{reference}:tracked.txt")]),
        "original\n"
    );
    assert!(
        !repo
            .git(["ls-tree", "-r", "--name-only", &reference])
            .contains("nested/untracked.txt")
    );
    repo.assert_no_scratch();
}

#[tokio::test]
async fn sparse_cone_capture_preserves_paths_that_are_not_checked_out() {
    let repo = Repository::with_head();
    repo.write("included/file.txt", "original included\n");
    repo.write("outside/file.txt", "original outside\n");
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add folders"]);
    repo.git(["sparse-checkout", "set", "--cone", "included"]);
    assert!(!repo.0.join("outside/file.txt").exists());
    repo.write("included/file.txt", "edited\n");
    let reference = checkpoint_ref(SCOPE, 0);
    capture(repo.input(&reference)).await.unwrap().unwrap();
    assert_eq!(
        repo.git(["show", &format!("{reference}:included/file.txt")]),
        "edited\n"
    );
    assert_eq!(
        repo.git(["show", &format!("{reference}:outside/file.txt")]),
        "original outside\n"
    );
    repo.assert_no_scratch();
}

#[tokio::test]
async fn sparse_non_cone_capture_preserves_excluded_paths_and_records_deletions() {
    let repo = Repository::with_head();
    repo.write("include/first.txt", "before edit\n");
    repo.write("include/deleted.txt", "before deletion\n");
    repo.write("outside/file.txt", "outside sparse checkout\n");
    repo.git(["add", "."]);
    repo.git(["commit", "-m", "add sparse checkout paths"]);
    repo.git([
        "sparse-checkout",
        "set",
        "--no-cone",
        "/include/",
        "/tracked.txt",
    ]);
    assert!(!repo.0.join("outside/file.txt").exists());
    repo.git(["update-index", "--assume-unchanged", "include/first.txt"]);
    repo.git(["update-index", "--skip-worktree", "include/deleted.txt"]);
    repo.write("include/first.txt", "edited\n");
    fs::remove_file(repo.0.join("include/deleted.txt")).unwrap();
    let index = fs::read(repo.0.join(".git/index")).unwrap();
    let reference = checkpoint_ref(SCOPE, 0);
    capture(repo.input(&reference)).await.unwrap().unwrap();
    assert_eq!(
        repo.git(["show", &format!("{reference}:include/first.txt")]),
        "edited\n"
    );
    assert_eq!(
        repo.git(["show", &format!("{reference}:outside/file.txt")]),
        "outside sparse checkout\n"
    );
    assert!(
        !repo
            .git(["ls-tree", "-r", "--name-only", &reference])
            .contains("include/deleted.txt")
    );
    assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), index);
    repo.assert_no_scratch();
}

#[tokio::test]
async fn summarizes_renames_binary_changes_and_exact_paths_with_scope_exclusions() {
    let repo = Repository::with_head();
    repo.git(["config", "diff.renames", "true"]);
    repo.write("before.txt", "renamed content\n");
    repo.write("excluded/file.txt", "original contents\n");
    let from = checkpoint_ref(SCOPE, 0);
    capture(repo.input(&from)).await.unwrap().unwrap();
    fs::rename(repo.0.join("before.txt"), repo.0.join("renamed.txt")).unwrap();
    repo.write("excluded/file.txt", "changed in its own scope\n");
    repo.write(" kept whitespace.txt ", "line one\nline two\n");
    fs::write(repo.0.join("binary.bin"), [0, 1, 200]).unwrap();
    let to = checkpoint_ref(SCOPE, 1);
    capture(repo.input(&to)).await.unwrap().unwrap();
    let index = fs::read(repo.0.join(".git/index")).unwrap();
    let files: Vec<CheckpointFile> = summarize(DiffInput {
        cwd: &repo.0,
        from_checkpoint_ref: &from,
        to_checkpoint_ref: &to,
        exclude_paths: &["excluded"],
    })
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(files).unwrap(),
        serde_json::json!([
            {"path": " kept whitespace.txt ", "kind": "modified", "additions": 2, "deletions": 0},
            {"path": "binary.bin", "kind": "modified", "additions": 0, "deletions": 0},
            {"path": "renamed.txt", "kind": "modified", "additions": 0, "deletions": 0}
        ])
    );
    assert!(
        summarize(DiffInput {
            cwd: &repo.0,
            from_checkpoint_ref: &checkpoint_ref(SCOPE, 2),
            to_checkpoint_ref: &to,
            exclude_paths: &[],
        })
        .await
        .unwrap()
        .is_empty()
    );
    assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), index);
    repo.assert_no_scratch();
}

#[tokio::test]
async fn summarizes_local_commits_and_dirty_changes_without_attributing_local_fetch_contents() {
    let source = Repository::with_head();
    let repo = Repository::new();
    repo.git(["remote", "add", "fixture", source.0.to_str().unwrap()]);
    repo.git(["fetch", "fixture"]);
    repo.git(["switch", "-C", "main", "--track", "fixture/main"]);
    let from = checkpoint_ref(SCOPE, 0);
    capture(repo.input(&from)).await.unwrap().unwrap();
    source.write("brought-in.txt", "from the fetched repository\n");
    source.git(["add", "brought-in.txt"]);
    source.git(["commit", "-m", "fetched addition"]);
    repo.git(["fetch", "fixture"]);
    repo.git(["merge", "--ff-only", "fixture/main"]);
    repo.write("local-commit.txt", "committed in this repository\n");
    repo.git(["add", "local-commit.txt"]);
    repo.git(["commit", "-m", "locally authored addition"]);
    repo.write("tracked.txt", "dirty after local commit\n");
    let to = checkpoint_ref(SCOPE, 1);
    capture(CaptureInput {
        previous_checkpoint_ref: Some(&from),
        ..repo.input(&to)
    })
    .await
    .unwrap()
    .unwrap();
    let files = summarize(DiffInput {
        cwd: &repo.0,
        from_checkpoint_ref: &from,
        to_checkpoint_ref: &to,
        exclude_paths: &[],
    })
    .await
    .unwrap();
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["local-commit.txt", "tracked.txt"]
    );
    repo.assert_no_scratch();
}
