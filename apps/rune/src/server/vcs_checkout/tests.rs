use std::fs;
use std::process::Command;

use rusqlite::Connection;
use serde_json::json;

use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    repository: PathBuf,
    database: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        fs::create_dir(&repository).unwrap();
        let database = directory.path().join("state.sqlite");
        Connection::open(&database)
            .unwrap()
            .execute_batch(include_str!("../bootstrap/schema.sql"))
            .unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        fs::write(repository.join("notes.txt"), "main\n").unwrap();
        git(&repository, &["add", "notes.txt"]);
        git(&repository, &["commit", "-qm", "Main"]);
        git(&repository, &["checkout", "-qb", "feature"]);
        fs::write(repository.join("notes.txt"), "feature\n").unwrap();
        git(&repository, &["commit", "-qam", "Feature"]);
        git(&repository, &["checkout", "-q", "main"]);
        Self {
            _directory: directory,
            repository,
            database,
        }
    }

    async fn switch(&self, name: &str) -> Result<SwitchRefResult, GitCommandError> {
        switch_ref(
            &self.database,
            SwitchRefInput::parse(json!({
                "cwd": self.repository,
                "refName": name,
            }))
            .unwrap(),
        )
        .await
    }
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn switching_a_stale_reference_cannot_restore_a_dirty_path() {
    let fixture = Fixture::new();
    fs::write(fixture.repository.join("notes.txt"), "uncommitted\n").unwrap();
    let error = serde_json::to_value(fixture.switch("feature").await.err().unwrap()).unwrap();
    assert_eq!(
        error["detail"],
        "Your local changes would be overwritten by switching branches. Commit, stash, or discard them, then switch again."
    );
    let missing = serde_json::to_value(fixture.switch("notes.txt").await.err().unwrap()).unwrap();
    assert_eq!(missing["operation"], CHECKOUT);
    assert_eq!(
        git(&fixture.repository, &["branch", "--show-current"]),
        "main"
    );
    assert_eq!(
        fs::read_to_string(fixture.repository.join("notes.txt")).unwrap(),
        "uncommitted\n"
    );
}

#[tokio::test]
async fn an_archived_active_thread_blocks_its_nested_shared_checkout() {
    let fixture = Fixture::new();
    let nested = fixture.repository.join("packages").join("existing");
    fs::create_dir_all(&nested).unwrap();
    let database = Connection::open(&fixture.database).unwrap();
    database.execute(
        "INSERT INTO projection_projects (project_id, title, workspace_root, created_at, updated_at, scripts_json)
         VALUES ('nested','Nested',?1,'2026-06-01','2026-06-01','[]')",
        [nested.to_str().unwrap()],
    ).unwrap();
    database.execute_batch(
        "INSERT INTO orchestration_v2_projection_threads
           (thread_id, project_id, title, default_provider, runtime_mode, interaction_mode, created_at, updated_at, archived_at, payload_json)
         VALUES ('thread','nested','Archived work','codex','full-access','default','2026-06-01','2026-06-01','2026-06-02','{}');
         INSERT INTO orchestration_v2_projection_runs
           (run_id, thread_id, status, ordinal, provider, requested_at, payload_json)
         VALUES ('run','thread','running',1,'codex','2026-06-01','{}');"
    ).unwrap();
    let error = serde_json::to_value(fixture.switch("feature").await.err().unwrap()).unwrap();
    assert_eq!(
        error["detail"],
        "'Archived work' is still working in this checkout. Wait for it to finish, or use a worktree for a separate branch."
    );
    assert_eq!(
        git(&fixture.repository, &["branch", "--show-current"]),
        "main"
    );
    database
        .execute(
            "UPDATE orchestration_v2_projection_runs SET status='completed'",
            [],
        )
        .unwrap();
    let switched = fixture.switch("feature").await.unwrap();
    assert_eq!(switched.ref_name.as_deref(), Some("feature"));
    assert_eq!(
        fs::read_to_string(fixture.repository.join("notes.txt")).unwrap(),
        "feature\n"
    );
}

#[tokio::test]
async fn branch_creation_survives_a_failed_checkout() {
    let fixture = Fixture::new();
    let locked_index = fixture.repository.join(".git/index.lock");
    fs::write(&locked_index, "fixture owns this lock").unwrap();
    let input = CreateRefInput::parse(json!({
        "cwd": fixture.repository,
        "refName": "keep-this-branch",
        "switchRef": true,
    }))
    .unwrap();
    let error = create_ref(&fixture.database, input).await.unwrap_err();
    let wire = serde_json::to_value(error).unwrap();
    assert_eq!(wire["operation"], CHECKOUT);
    assert_eq!(
        git(&fixture.repository, &["branch", "--show-current"]),
        "main"
    );
    assert_eq!(
        git(
            &fixture.repository,
            &["rev-parse", "refs/heads/keep-this-branch"]
        ),
        git(&fixture.repository, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        fs::read_to_string(locked_index).unwrap(),
        "fixture owns this lock"
    );
}
