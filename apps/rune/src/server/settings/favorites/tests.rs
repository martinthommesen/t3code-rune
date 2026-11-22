use std::fs;

use serde_json::json;

use super::super::{Patch, SettingsFile};

#[test]
fn favorite_deltas_are_authorized_serialized_published_and_restored() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("settings.json");
    let database = folder.path().join("statev2.sqlite");
    let initial = json!({
        "favorites": [{"provider":"work","model":"gpt-5.4"}],
        "providerModelPreferences": {"work":{"hiddenModels":["other"],"modelOrder":["gpt-5.4"]}},
        "defaultRuntimeMode": "approval-required",
        "futureSetting": {"value":true}
    });
    fs::write(&path, initial.to_string()).unwrap();
    let settings = SettingsFile::new(path.clone());
    let mut updates = settings.subscribe_changes();
    let write = |provider: &str, model: &str, favorite: bool| {
        let patch = Patch::decode(&json!({"patch":{"setModelFavorites":[{"provider":provider,"model":model,"favorite":favorite}]}})).unwrap();
        assert_eq!(patch.required_permission(false), Some("settings:write"));
        assert_eq!(patch.required_permission(true), None);
        assert_eq!(patch.unsupported(), None);
        settings.update(patch, &database).unwrap()
    };
    std::thread::scope(|scope| {
        scope.spawn(|| write("codex", "gpt-5.4", true));
        scope.spawn(|| write("codex", "gpt-5.4", true));
    });
    let restored = SettingsFile::new(path.clone()).read().unwrap().for_client();
    assert_eq!(
        restored["favorites"],
        json!([{"provider":"work","model":"gpt-5.4"},{"provider":"codex","model":"gpt-5.4"}])
    );
    assert_eq!(
        restored["providerModelPreferences"],
        initial["providerModelPreferences"]
    );
    let after = write("codex", "gpt-5.4", false);
    assert_eq!(after["favorites"], initial["favorites"]);
    assert_eq!(
        updates.try_recv().unwrap()["favorites"],
        restored["favorites"]
    );
    assert_eq!(
        updates.try_recv().unwrap()["favorites"],
        restored["favorites"]
    );
    assert_eq!(
        updates.try_recv().unwrap()["favorites"],
        initial["favorites"]
    );
    let persisted: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(persisted, initial);
}

#[test]
fn malformed_favorite_delta_is_rejected_during_decode() {
    let patch = json!({"patch":{"setModelFavorites":[
        {"provider":"codex","model":"gpt-5.4","favorite":true},
        {"provider":"claudeAgent","model":"claude-sonnet-4-6","favorite":"false"}
    ]}});
    let Err(error) = Patch::decode(&patch) else {
        panic!("favorite delta accepted an invalid desired state");
    };
    assert_eq!(
        error,
        "Expected boolean\n  at [\"patch\"][\"setModelFavorites\"][1][\"favorite\"]"
    );
}
