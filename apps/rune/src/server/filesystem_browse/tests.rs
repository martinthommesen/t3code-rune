use serde_json::{Value, json, to_value};

use super::*;

fn request(partial_path: &str, cwd: Option<&str>) -> BrowseInput {
    let mut request = json!({ "partialPath": partial_path });
    if let Some(cwd) = cwd {
        request["cwd"] = json!(cwd);
    }
    serde_json::from_value(request).unwrap()
}

fn as_directory(path: &Path) -> String {
    format!("{}{MAIN_SEPARATOR_STR}", path.display())
}

fn names(result: &BrowseResult) -> Vec<&str> {
    result
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect()
}

#[test]
fn directory_mode_and_hidden_prefix_list_only_matching_directories() {
    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path();
    for name in [".config", "config", "ConfigOther", "Documents"] {
        fs::create_dir(parent.join(name)).unwrap();
    }
    fs::write(parent.join("config-file"), "").unwrap();
    let directory = browse(request(&as_directory(parent), None)).unwrap();
    assert_eq!(
        names(&directory),
        [".config", "config", "ConfigOther", "Documents"]
    );
    let prefix = parent.join("con").to_string_lossy().into_owned();
    let ordinary = browse(request(&prefix, None)).unwrap();
    assert_eq!(names(&ordinary), ["config", "ConfigOther"]);
    let hidden_prefix = parent.join(".c").to_string_lossy().into_owned();
    let hidden = browse(request(&hidden_prefix, None)).unwrap();
    assert_eq!(names(&hidden), [".config"]);
    assert_eq!(
        hidden.entries[0].full_path,
        parent.join(".config").to_string_lossy()
    );
}

#[test]
fn explicit_relative_path_is_lexically_resolved_against_the_project() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir(fixture.path().join("docs")).unwrap();
    let result = browse(request("../missing/../d", project.to_str())).unwrap();
    assert_eq!(result.parent_path, fixture.path().to_string_lossy());
    assert_eq!(names(&result), ["docs"]);
    let error = to_value(browse(request("./docs", None)).unwrap_err()).unwrap();
    assert_eq!(
        error,
        json!({
            "_tag": "FilesystemBrowseError",
            "partialPath": "./docs",
            "failure": "current_project_required",
            "message": "Failed to browse filesystem path './docs'.",
            "cause": {
                "name": "WorkspaceEntriesCurrentProjectRequiredError",
                "message": "A current project is required to browse relative workspace path './docs'."
            }
        })
    );
}

#[cfg(not(windows))]
#[test]
fn windows_drive_or_unc_paths_on_posix_have_their_own_failure() {
    for partial in ["C:", "x:/users/", "c:\\users\\", "\\\\server\\share\\"] {
        let error = to_value(browse(request(partial, None)).unwrap_err()).unwrap();
        assert_eq!(error["failure"], "windows_path_unsupported");
        assert_eq!(error["partialPath"], partial);
        assert_eq!(
            error["platform"],
            if cfg!(target_os = "macos") {
                "darwin"
            } else {
                env::consts::OS
            }
        );
        assert_eq!(
            error["cause"]["name"],
            "WorkspaceEntriesWindowsPathUnsupportedError"
        );
        assert!(error.get("pathMissing").is_none());
    }
}

#[test]
fn only_missing_paths_set_the_creation_hint() {
    let fixture = tempfile::tempdir().unwrap();
    let missing = fixture.path().join("does-not-exist");
    let not_dir = fixture.path().join("file");
    fs::write(&not_dir, "").unwrap();
    let error = |parent: &Path| {
        to_value(browse(request(&as_directory(parent), None)).unwrap_err()).unwrap()
    };
    let missing_error = error(&missing);
    assert_eq!(missing_error["failure"], "read_directory_failed");
    assert_eq!(missing_error["parentPath"], missing.to_str().unwrap());
    assert_eq!(missing_error["pathMissing"], true);
    assert_eq!(
        missing_error["cause"]["cause"]["message"],
        format!(
            "ENOENT: no such file or directory, scandir '{}'",
            missing.display()
        )
    );
    let under_file_error = error(&not_dir);
    assert_eq!(under_file_error["failure"], "read_directory_failed");
    assert!(under_file_error.get("pathMissing").is_none());
    assert_eq!(
        under_file_error["cause"]["cause"]["message"],
        format!("ENOTDIR: not a directory, scandir '{}'", not_dir.display())
    );
}

#[cfg(unix)]
#[test]
fn symlink_entries_are_omitted_and_explicitly_navigated_links_keep_the_alias() {
    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path();
    let real = parent.join("real");
    fs::create_dir_all(real.join("child")).unwrap();
    let alias = parent.join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let looped = parent.join("loop");
    std::os::unix::fs::symlink("loop", &looped).unwrap();

    let all = browse(request(&as_directory(parent), None)).unwrap();
    assert_eq!(names(&all), ["real"]);
    let navigated = browse(request(&as_directory(&alias), None)).unwrap();
    assert_eq!(navigated.parent_path, alias.to_string_lossy());
    assert_eq!(names(&navigated), ["child"]);
    assert_eq!(
        navigated.entries[0].full_path,
        alias.join("child").to_string_lossy()
    );
    let error = to_value(browse(request(&as_directory(&looped), None)).unwrap_err()).unwrap();
    assert_eq!(
        error["cause"]["cause"]["message"],
        format!(
            "ELOOP: too many symbolic links encountered, scandir '{}'",
            looped.display()
        )
    );
    assert!(error.get("pathMissing").is_none());
}

#[cfg(unix)]
#[test]
fn wire_encoding_trims_padding_and_rejects_a_blank_directory_name() {
    let fixture = tempfile::tempdir().unwrap();
    let padded = fixture.path().join(" Padded ");
    fs::create_dir(&padded).unwrap();
    let value = to_value(browse(request(&as_directory(fixture.path()), None)).unwrap()).unwrap();
    assert_eq!(value["entries"][0]["name"], "Padded");
    assert_eq!(
        value["entries"][0]["fullPath"],
        padded.to_string_lossy().trim_end()
    );

    fs::create_dir(fixture.path().join(" ")).unwrap();
    let result = browse(request(&as_directory(fixture.path()), None)).unwrap();
    assert_eq!(
        to_value(result).unwrap_err().to_string(),
        "Expected a non-blank string\n  at [\"entries\"][0][\"name\"]"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn canonically_equivalent_names_use_the_directory_entry_order() {
    let fixture = tempfile::tempdir().unwrap();
    fs::create_dir(fixture.path().join("é")).unwrap();
    fs::create_dir(fixture.path().join("e\u{301}")).unwrap();
    let result = browse(request(&as_directory(fixture.path()), None)).unwrap();
    assert_eq!(names(&result), ["e\u{301}", "é"]);
}

#[test]
fn request_length_is_checked_in_utf16_after_javascript_trimming() {
    let allowed = "👻".repeat(256);
    let decode = |request: Value| serde_json::from_value::<BrowseInput>(request);
    let input =
        decode(json!({ "partialPath": format!("\u{feff}{allowed}\t"), "cwd": null })).unwrap();
    assert_eq!(input.partial_path, allowed);
    assert_eq!(input.cwd, Some(None));
    let relative = decode(json!({ "partialPath": "./docs", "cwd": null })).unwrap();
    let error = to_value(browse(relative).unwrap_err()).unwrap();
    assert!(error.get("cwd").is_some_and(Value::is_null));
    for partial in ["👻".repeat(257), "\u{feff}\t ".to_owned()] {
        assert!(decode(json!({ "partialPath": partial })).is_err());
    }
    assert!(decode(json!({ "partialPath": "/", "cwd": " ".to_owned() })).is_err());
    assert!(decode(json!({ "partialPath": "/", "cwd": "d".repeat(513) })).is_err());
    assert_eq!(
        decode(json!({ "partialPath": "\u{85}" }))
            .unwrap()
            .partial_path,
        "\u{85}"
    );
}
