use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde_json::json;

use super::*;

fn executable(directory: &Path, script: &str) -> PathBuf {
    let executable = directory.join("claude");
    fs::write(&executable, format!("#!/bin/sh\n{script}")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

#[tokio::test]
async fn runs_plain_title_with_isolated_working_directory_and_exact_prompt() {
    let fixture = tempfile::tempdir().unwrap();
    let trace = fixture.path().join("trace");
    let input = fixture.path().join("stdin");
    let executable = executable(
        fixture.path(),
        r#"pwd > "$TRACE"
printf '%s\n' "$@" >> "$TRACE"
cat > "$INPUT"
printf '%s\n' '{"type":"result","structured_output":{"title":"A thread title","needsRefinement":false}}'
"#,
    );
    let schema = json!({"type":"object","additionalProperties":false});
    let prompt = "First line.\nSecond line without trailing newline.";
    let result = run_title(
        &executable,
        &HashMap::from([
            (OsString::from("TRACE"), trace.clone().into_os_string()),
            (OsString::from("INPUT"), input.clone().into_os_string()),
        ]),
        TitleRequest {
            prompt,
            schema: &schema,
            model: "claude-sonnet-4-6",
        },
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        json!({"title":"A thread title","needsRefinement":false})
    );
    assert_eq!(fs::read_to_string(input).unwrap(), prompt);
    let arguments = fs::read_to_string(trace).unwrap();
    let (cwd, argv) = arguments.split_once('\n').unwrap();
    let cwd = Path::new(cwd);
    assert!(
        cwd.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("t3code-claude-title-")
    );
    assert!(!cwd.exists());
    assert_ne!(cwd, fixture.path());
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        [
            "-p",
            "--output-format",
            "json",
            "--json-schema",
            &schema.to_string(),
            "--model",
            "claude-sonnet-4-6",
            "--effort",
            "high",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--tools",
            "",
            "--disable-slash-commands",
            "--no-session-persistence",
            "--strict-mcp-config",
            "--permission-mode",
            "dontAsk",
        ],
    );
}

#[tokio::test]
async fn negotiates_the_session_persistence_flag_only_when_the_cli_rejects_it() {
    let fixture = tempfile::tempdir().unwrap();
    let trace = fixture.path().join("trace");
    let executable = executable(
        fixture.path(),
        r#"pwd >> "$TRACE"
for arg in "$@"; do
    if [ "$arg" = --no-session-persistence ]; then
        printf '%s\n' "error: unknown option '--no-session-persistence'" >&2
        exit 1
    fi
done
cat >/dev/null
printf '%s\n' '[{"type":"assistant"},{"type":"result","structured_output":{"title":"Retry"}}]'
"#,
    );
    let result = run_title(
        &executable,
        &HashMap::from([(OsString::from("TRACE"), trace.clone().into_os_string())]),
        TitleRequest {
            prompt: "A title prompt",
            schema: &json!({}),
            model: "claude-sonnet-4-6",
        },
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(result, json!({"title":"Retry"}));
    let paths = fs::read_to_string(trace).unwrap();
    let [first, second] = paths.lines().collect::<Vec<_>>()[..] else {
        panic!("expected two process invocations");
    };
    assert_eq!(first, second);
    assert!(!Path::new(first).exists());
}

#[tokio::test]
async fn limits_captured_output_and_total_request_time() {
    let fixture = tempfile::tempdir().unwrap();
    let schema = json!({});
    let environment = HashMap::new();
    for (script, deadline, kind) in [
        (
            "cat >/dev/null\nexec yes oversized",
            Duration::from_secs(3),
            io::ErrorKind::InvalidData,
        ),
        (
            "cat >/dev/null\nexec sleep 15",
            Duration::from_millis(50),
            io::ErrorKind::TimedOut,
        ),
    ] {
        let command = executable(fixture.path(), script);
        let failure = run_title(
            &command,
            &environment,
            TitleRequest {
                prompt: "A title prompt",
                schema: &schema,
                model: "claude-sonnet-4-6",
            },
            deadline,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind(), kind);
    }
}
