use super::*;

#[test]
fn missing_preference_defaults_to_confirming_and_round_trips_when_disabled() {
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("userdata");
    let mut settings = ClientSettings::at_state_dir(&state_dir);
    assert!(settings.confirm_terminal_close());
    assert!(!state_dir.exists());
    settings.set_confirm_terminal_close(false).unwrap();
    assert!(!settings.confirm_terminal_close());
    assert!(!ClientSettings::at_state_dir(&state_dir).confirm_terminal_close());
    assert_eq!(
        fs::read_to_string(state_dir.join(FILE_NAME)).unwrap(),
        "{\"confirmTerminalClose\":false}\n"
    );
}

#[test]
fn inline_thread_details_persist_only_closed_scoped_threads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(FILE_NAME);
    fs::write(&path, r#"{"settings":{"confirmTerminalClose":false}}"#).unwrap();
    let mut settings = ClientSettings::at_state_dir(directory.path());
    assert!(settings.thread_details_inline_open("one", "thread"));
    settings
        .set_thread_details_inline_open("one", "thread", false)
        .unwrap();
    let mut settings = ClientSettings::at_state_dir(directory.path());
    assert!(!settings.thread_details_inline_open("one", "thread"));
    assert!(settings.thread_details_inline_open("two", "thread"));
    assert!(settings.thread_details_inline_open("one", "other"));
    assert!(!settings.confirm_terminal_close());
    settings
        .set_thread_details_inline_open("one", "thread", true)
        .unwrap();
    assert_eq!(
        read_document(&path).unwrap()["settings"],
        serde_json::json!({
            "confirmTerminalClose": false
        })
    );
}

#[test]
fn patching_legacy_json_preserves_other_document_values() {
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join(FILE_NAME);
    fs::write(
        &file,
        r#"{
            // Valid older desktop wrapper
            "settings": {"confirmTerminalClose": false, "timestampFormat": "24-hour", },
            "version": 1,
        }"#,
    )
    .unwrap();
    let mut settings = ClientSettings::at_state_dir(state.path());
    assert!(!settings.confirm_terminal_close());
    settings.set_confirm_terminal_close(true).unwrap();
    let saved = read_document(&file).unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["settings"]["timestampFormat"], "24-hour");
    assert_eq!(saved["settings"]["confirmTerminalClose"], true);
}

#[test]
fn corrupt_preferences_default_to_confirming_and_are_not_replaced() {
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join(FILE_NAME);
    let content = "{\"settings\": {\"confirmTerminalClose\": \"disabled\"}}";
    fs::write(&file, content).unwrap();
    let mut settings = ClientSettings::at_state_dir(state.path());
    assert!(settings.confirm_terminal_close());
    assert_eq!(
        settings
            .set_confirm_terminal_close(false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(fs::read_to_string(file).unwrap(), content);
    assert!(settings.confirm_terminal_close());
}

#[cfg(unix)]
#[test]
fn writes_to_relative_symlink_target_and_keeps_the_link() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("userdata");
    fs::create_dir(&state).unwrap();
    let path = state.join(FILE_NAME);
    let target = root.path().join("preferences.json");
    std::os::unix::fs::symlink("../preferences.json", &path).unwrap();
    let mut settings = ClientSettings::at_state_dir(&state);
    settings.set_confirm_terminal_close(false).unwrap();
    assert_eq!(
        fs::read_link(path).unwrap(),
        Path::new("../preferences.json")
    );
    assert_eq!(
        read_document(&target).unwrap()[TERMINAL_CONFIRMATION],
        false
    );
}

#[test]
fn appearance_overrides_system_and_restores_from_a_separate_preference() {
    use AppearanceMode::{Dark, Light, System};

    let state = tempfile::tempdir().unwrap();
    let mut settings = ClientSettings::at_state_dir(state.path());
    let theme = state.path().join("theme-preferences.json");
    assert_eq!(settings.appearance_mode(), System);
    assert!(!theme.exists());
    for (mode, on_light_system, on_dark_system) in [
        (Dark, true, true),
        (Light, false, false),
        (System, false, true),
    ] {
        settings.set_appearance_mode(mode).unwrap();
        let reopened = ClientSettings::at_state_dir(state.path());
        assert_eq!(reopened.appearance_mode(), mode);
        assert_eq!(reopened.appearance_mode().is_dark(false), on_light_system);
        assert_eq!(reopened.appearance_mode().is_dark(true), on_dark_system);
        assert_eq!(
            read_document(&theme).unwrap()["t3code:theme-appearance-mode"],
            mode.as_str()
        );
    }
    assert!(!state.path().join(FILE_NAME).exists());
}

#[test]
fn an_existing_native_mode_takes_precedence_over_an_electron_profile() {
    let state = tempfile::tempdir().unwrap();
    let mut settings = ClientSettings::at_state_dir(state.path());
    settings.set_appearance_mode(AppearanceMode::Light).unwrap();
    settings
        .import_electron_appearance(&state.path().join("unused-source"))
        .unwrap();
    assert_eq!(settings.appearance_mode(), AppearanceMode::Light);
}
