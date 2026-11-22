use super::*;

#[test]
fn resolves_claude_launch_settings_without_exposing_provider_credentials() {
    let defaults = SettingsSnapshot::decode("{}")
        .claude_instance(None, "claudeAgent")
        .unwrap();
    assert_eq!(defaults.binary_path, "claude");
    assert!(defaults.home_path.is_empty());
    let settings = SettingsSnapshot::decode(
        r#"{
        "providerInstances": {"claudeAgent": {"driver": "claudeAgent", "enabled": false, "config": {
            "binaryPath": " ~/bin/claude ", "homePath": " ~/.claude-fixture ",
            "launchArgs": " --chrome ", "autoCompactWindow": "200000", "idleCompaction": "onReturn",
            "customModels": [{"slug":"local-enterprise", "name":"Local"}]
        }, "environment":[{"name":"FIXTURE_KEY", "value":"private-value", "sensitive":true}]}},
        "projectSettingsOverrides": {"allows":{"providerInstances":{"claudeAgent":{"enabled":true}}}}
    }"#,
    );
    assert!(settings.claude_instance(None, "claudeAgent").is_none());
    let instance = settings
        .claude_instance(Some("allows"), "claudeAgent")
        .unwrap();
    assert_eq!(instance.binary_path, "~/bin/claude");
    assert_eq!(instance.home_path, "~/.claude-fixture");
    assert_eq!(instance.launch_args, " --chrome ");
    assert_eq!(instance.auto_compact_window, Some(200000));
    assert_eq!(instance.custom_models[0]["slug"], "local-enterprise");
    assert!(
        matches!(instance.environment[0].value, EnvironmentValue::Literal(ref text) if text == "private-value")
    );
    assert!(!settings.for_client().to_string().contains("private-value"));
    assert!(settings.claude_instance(None, "codex").is_none());
}

#[test]
fn a_persisted_file_supplies_private_codex_launch_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.json");
    let file = SettingsFile::new(path.clone());
    assert_eq!(
        file.read()
            .unwrap()
            .codex_instance(None, "codex")
            .unwrap()
            .binary_path,
        "codex"
    );

    fs::write(
        &path,
        r#"{
        "providerInstances": { "work": { "driver": "codex", "config": {
            "homePath": "/custom/home", "shadowHomePath": "/custom/shadow",
            "customModels": ["custom-model"]
        } } }
    }"#,
    )
    .unwrap();
    let snapshot = file.read().unwrap();
    let launch = snapshot.codex_instance(None, "work").unwrap();
    assert_eq!(launch.home_path, "/custom/home");
    assert_eq!(launch.shadow_home_path, "/custom/shadow");
    assert_eq!(launch.custom_models, [Value::from("custom-model")]);
}

#[test]
fn fresh_settings_expose_no_explicit_provider_but_can_launch_codex() {
    let snapshot = SettingsSnapshot::decode("{}");
    assert_eq!(
        snapshot.for_client()["providerInstances"],
        serde_json::json!({})
    );
    assert_eq!(snapshot.for_client()["defaultModelSelection"], Value::Null);
    let instance = snapshot.codex_instance(None, "codex").unwrap();
    assert_eq!(instance.binary_path, "codex");
    assert!(instance.environment.is_empty());
    assert!(matches!(
        snapshot.codex_instance(None, "missing"),
        Err(CodexInstanceError::Unavailable)
    ));
}

#[test]
fn settings_project_gates_and_redaction_share_normalized_instances() {
    let settings = SettingsSnapshot::decode(
        r#"{
        "providerInstances": {
            "codex": {"driver":"codex", "enabled": true, "config":{"enabled":false,"binaryPath":"/disabled"}},
            "work": {"driver":"codex", "config":{"binaryPath":" /local/codex ","apiKey":"private-config"}, "environment":[
                {"name":"PUBLIC","value":"visible"},
                {"name":"PRIVATE","sensitive":true,"value":"private-literal"},
                {"name":"SAVED","sensitive":true,"valueRedacted":true},
                {"name":"REF","secret":{"provider":"1Password","reference":"op://local/item/field"}}
            ]}
        },
        "projectSettingsOverrides": {
            "allows": {"providerInstances":{"codex":{"enabled":true}}},
            "denies": {"providerInstances":{"work":{"enabled":false}}}
        },
        "secretProviders": {
            "1Password": {"kind":"one_password", "label":"Work vault", "config":{"account":"local-account","token":"private-source"}}
        }
    }"#,
    );
    assert!(matches!(
        settings.codex_instance(None, "codex"),
        Err(CodexInstanceError::Disabled)
    ));
    assert_eq!(
        settings
            .codex_instance(Some("allows"), "codex")
            .unwrap()
            .binary_path,
        "/disabled"
    );
    assert!(matches!(
        settings.codex_instance(Some("denies"), "work"),
        Err(CodexInstanceError::Disabled)
    ));
    let work = settings.codex_instance(None, "work").unwrap();
    assert_eq!(work.binary_path, "/local/codex");
    assert!(
        matches!(&work.environment[1].value, EnvironmentValue::Literal(value) if value == "private-literal")
    );
    assert!(matches!(
        &work.environment[2].value,
        EnvironmentValue::Saved
    ));
    assert!(
        matches!(&work.environment[3].value, EnvironmentValue::Reference { provider, .. } if provider == "1Password")
    );
    let public = settings.for_client();
    let public_json = public.to_string();
    assert!(!public_json.contains("private-"));
    assert_eq!(
        public["providerInstances"]["work"]["environment"][0]["value"],
        "visible"
    );
    assert_eq!(
        public["providerInstances"]["work"]["environment"][1]["valueRedacted"],
        true
    );
    assert_eq!(
        public["providerInstances"]["work"]["environment"][3]["secret"]["provider"],
        "1Password"
    );
    assert_eq!(
        public["secretProviders"]["1Password"]["config"],
        serde_json::json!({"account":"local-account"})
    );
}

#[test]
fn legacy_lenient_json_and_provider_models_match_current_wire_fields() {
    let snapshot = SettingsSnapshot::decode(
        r#"{
        // Older settings placed Codex setup under providers.
        "providers": {"codex":{"enabled":false,"binaryPath":" /legacy/codex "}},
        "providerInstances": {
            "codex": {"driver":"codex","config":{"launchArgs":"--config url=\"https://example.test/*a*/\""}},
        },
        "defaultModelSelection":{"provider":"codex","model":" custom/model ","options":{"z":true,"02":"leading","2":"index","a":"tail"}},
        "responseStreamingMode":"token",
        "compactIdleThreads":"beforeCacheExpires",
    }"#,
    );
    let instance = snapshot.codex_instance(None, "codex").unwrap();
    assert_eq!(instance.binary_path, "codex");
    assert_eq!(
        instance.launch_args,
        "--config url=\"https://example.test/*a*/\""
    );
    let public = snapshot.for_client();
    assert_eq!(public["responseStreamingMode"], "paragraph");
    assert_eq!(public["compactIdleThreads"], true);
    assert_eq!(public["idleCompaction"], "beforeCacheExpires");
    assert_eq!(
        public["defaultModelSelection"],
        serde_json::json!({
            "instanceId":"codex", "model":"custom/model", "options":[
                {"id":"2","value":"index"}, {"id":"z","value":true},
                {"id":"02","value":"leading"}, {"id":"a","value":"tail"}
            ]
        })
    );
}

#[test]
fn invalid_persisted_settings_get_safe_defaults_without_permitting_work() {
    for invalid in [
        "{",
        r#"{"providerInstances":{"codex":{"driver":"codex","enabled":"false"}}}"#,
        r#"{"projectSettingsOverrides":{"project":{"providerInstances":{"codex":{"enabled":"false"}}}}}"#,
    ] {
        let snapshot = SettingsSnapshot::decode(invalid);
        let public = snapshot.for_client();
        assert_eq!(public["continueThreadsAfterServerUpdate"], false);
        assert_eq!(public["usageHistoryRetentionDays"], "unlimited");
        assert!(matches!(
            snapshot.codex_instance(None, "codex"),
            Err(CodexInstanceError::InvalidSettings)
        ));
    }
}

#[test]
fn rejects_a_null_project_workspace_mode_without_keeping_the_environment_default() {
    let settings = SettingsSnapshot::decode(
        r#"{
        "defaultThreadEnvMode": "local",
        "projectSettingsFolded": true,
        "projectSettingsOverrides": {"project": {"defaultThreadEnvMode": null}}
    }"#,
    );
    assert!(!settings.readable);
    let public = settings.for_client();
    assert!(public.get("defaultThreadEnvMode").is_none());
    assert_eq!(public["continueThreadsAfterServerUpdate"], false);
    assert_eq!(public["projectSettingsOverrides"], serde_json::json!({}));
}

#[test]
fn first_project_override_write_folds_legacy_defaults_without_reviving_a_reset() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("statev2.sqlite");
    let settings_path = temp.path().join("settings.json");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(include_str!("../bootstrap/schema.sql"))
        .unwrap();
    connection
        .execute(
            "INSERT INTO projection_projects
             (project_id,title,workspace_root,default_thread_env_mode,auto_pull,created_at,updated_at,scripts_json)
             VALUES ('legacy','legacy','/legacy','worktree',1,'2026-01-01','2026-01-01','[]')",
            [],
        )
        .unwrap();
    fs::write(
        &settings_path,
        r#"{"projectAutoPullOverrides":{"legacy":false},"projectSettingsOverrides":{
            "legacy":{"providerInstances":{"codex":{"enabled":true}},"futurePreference":3}
        }}"#,
    )
    .unwrap();
    let file = SettingsFile::new(settings_path.clone());
    let update = serde_json::json!({"patch":{"projectSettingsOverrides":{"second":{"defaultAutoPull":true}}}});
    let written = file
        .update(Patch::decode(&update).unwrap(), &database)
        .unwrap();
    assert_eq!(written["projectSettingsFolded"], true);
    assert_eq!(
        written["projectSettingsOverrides"]["legacy"],
        serde_json::json!({
            "defaultThreadEnvMode":"worktree", "defaultAutoPull":false,
            "providerInstances":{"codex":{"enabled":true}}, "futurePreference":3,
        })
    );
    let reset = serde_json::json!({"patch":{"projectSettingsOverrides":{"legacy":null}}});
    file.update(Patch::decode(&reset).unwrap(), &database)
        .unwrap();
    let restored = SettingsFile::new(settings_path)
        .read()
        .unwrap()
        .for_client();
    assert!(restored["projectSettingsOverrides"].get("legacy").is_none());
    assert!(restored["projectAutoPullOverrides"].get("legacy").is_none());
    let mode = connection
        .query_row(
            "SELECT default_thread_env_mode FROM projection_projects WHERE project_id='legacy'",
            [],
            |record| record.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(mode, "worktree");
}

#[test]
fn startup_reconciliation_keeps_the_file_when_the_project_lookup_fails_after_folding() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("statev2.sqlite");
    let settings_path = temp.path().join("settings.json");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE projection_projects (
          project_id TEXT, default_model_selection_json TEXT, default_thread_env_mode TEXT,
          auto_pull INTEGER, scripts_json TEXT, deleted_at TEXT
        );
        INSERT INTO projection_projects VALUES ('live', NULL, 'worktree', 1, '[]', NULL);
        INSERT INTO projection_projects VALUES (NULL, NULL, NULL, 0, '[]', '2026-01-01');",
        )
        .unwrap();
    let old = b"{\n  // Preserve the original document on error.\n  \"projectAutoPullOverrides\": {\"live\":false}\n}\n";
    fs::write(&settings_path, old).unwrap();
    let file = SettingsFile::new(settings_path.clone());
    assert!(file.reconcile_projects_on_startup(&database).is_err());
    assert_eq!(fs::read(settings_path).unwrap(), old);
}

#[test]
fn startup_reconciliation_leaves_invalid_stored_settings_for_repair() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let old =
        b"{\"projectSettingsOverrides\":{\"live\":{\"worktreeCleanup\":{\"mode\":\"wrong\"}}}}";
    fs::write(&path, old).unwrap();
    let file = SettingsFile::new(path.clone());
    file.reconcile_projects_on_startup(&temp.path().join("not-present.sqlite"))
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), old);
    assert_eq!(
        file.read().unwrap().for_client()["continueThreadsAfterServerUpdate"],
        false
    );
}

#[test]
fn configured_worktree_deletion_reaches_the_server_and_client_policy() {
    use crate::worktree_cleanup::{DeletePolicy, KeepWhen};

    let defaults =
        SettingsSnapshot::decode(r#"{"storageCleanup":{"worktreeOnDelete":true}}"#).for_client();
    assert_eq!(
        defaults["storageCleanup"]["worktreeKeepWhen"],
        "uncommitted-changes"
    );
    assert_eq!(
        DeletePolicy::from_settings(&defaults).when_deleted("project"),
        Some(KeepWhen::UncommittedChanges)
    );
    let with_override = SettingsSnapshot::decode(
        r#"{
        "storageCleanup":{"worktreeOnDelete":true},
        "worktreeCleanup":{"mode":"off"},
        "projectSettingsOverrides":{"project":{"worktreeCleanup":{"mode":"custom","rules":{
          "worktreeKeepWhen":"tracked-changes","worktreeAfterDays":null,"worktreeOnMerge":false,
          "worktreeOnDelete":true,"worktreeUnchanged":false
        }}}}
    }"#,
    )
    .for_client();
    let policy = DeletePolicy::from_settings(&with_override);
    assert_eq!(policy.when_deleted("other"), None);
    assert_eq!(
        policy.when_deleted("project"),
        Some(KeepWhen::TrackedChanges)
    );
}

#[test]
fn reports_each_observed_settings_revision_once_even_when_its_values_match() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let contents = b"{\"defaultAutoPull\":true}\n";
    fs::write(&path, contents).unwrap();
    let file = SettingsFile::new(path.clone());
    file.initialize_watch();
    let mut changes = file.subscribe_changes();
    file.observe();
    assert!(changes.try_recv().is_err());

    disk::write(&path, contents).unwrap();
    file.observe();
    assert_eq!(changes.try_recv().unwrap()["defaultAutoPull"], true);
    file.observe();
    assert!(changes.try_recv().is_err());

    let patch = serde_json::json!({"patch":{"defaultAutoPull":false}});
    let result = file.update(
        Patch::decode(&patch).unwrap(),
        &temp.path().join("statev2.sqlite"),
    );
    assert_eq!(result.unwrap()["defaultAutoPull"], false);
    assert_eq!(changes.try_recv().unwrap()["defaultAutoPull"], false);
    file.observe();
    assert_eq!(changes.try_recv().unwrap()["defaultAutoPull"], false);
    file.observe();
    assert!(changes.try_recv().is_err());
}
