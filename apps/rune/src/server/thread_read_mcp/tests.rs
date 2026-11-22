use std::time::Instant;

use reqwest::{Client, StatusCode};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::{ProviderScope, ThreadReadMcp};
use crate::server::credentials::{Credentials as DesktopCredentials, Permissions};

const ENV: &str = "local-environment";
const WHEN: &str = "2026-09-08T12:00:00.000Z";

struct Fixture {
    server: ThreadReadMcp,
    address: String,
    client: Client,
    host: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.host.abort();
    }
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
          "CREATE TABLE orchestration_v2_projection_threads(thread_id TEXT PRIMARY KEY, provider_instance_id TEXT, deleted_at TEXT, payload_json TEXT);
           CREATE TABLE orchestration_v2_projection_runtime_requests(thread_id TEXT, status TEXT);
           CREATE TABLE orchestration_v2_projection_turn_items(thread_id TEXT, turn_item_id TEXT, run_id TEXT, node_id TEXT, ordinal INTEGER, type TEXT, payload_json TEXT, PRIMARY KEY(thread_id, turn_item_id));
           CREATE TABLE orchestration_v2_projection_runs(thread_id TEXT, run_id TEXT, status TEXT, ordinal INTEGER, payload_json TEXT);
           CREATE TABLE orchestration_v2_projection_run_attempts(thread_id TEXT, run_id TEXT, root_node_id TEXT, status TEXT);
           CREATE TABLE orchestration_v2_projection_provider_turns(thread_id TEXT, ordinal INTEGER, payload_json TEXT);
           CREATE TABLE orchestration_v2_projection_messages(thread_id TEXT, message_id TEXT, payload_json TEXT);
           CREATE INDEX turn_items_order ON orchestration_v2_projection_turn_items(thread_id,ordinal,turn_item_id);"
        ).unwrap();
        for (id, project, archived, deleted) in [
            ("caller", "first-project", None, None),
            ("neighbor", "first-project", None, None),
            ("target", "second-project", Some(WHEN), None),
            ("deleted", "second-project", None, Some(WHEN)),
        ] {
            let thread = json!({"id":id,"projectId":project,"title":"Local history","createdBy":"user","creationSource":"web",
                "modelSelection":{"instanceId":"codex","model":"gpt-5.4"},"runtimeMode":"approval-required","interactionMode":"default",
                "createdAt":WHEN,"updatedAt":WHEN,"archivedAt":archived,"deletedAt":deleted,"branch":"main",
                "lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":id}});
            db.execute(
                "INSERT INTO orchestration_v2_projection_threads VALUES (?1,'codex',?2,?3)",
                params![id, deleted, thread.to_string()],
            )
            .unwrap();
        }
        let run = json!({"id":"run-1","ordinal":1,"status":"completed","modelSelection":{"instanceId":"codex","model":"gpt-5.4"},
            "activeAttemptId":"attempt-1","requestedAt":WHEN,"startedAt":WHEN,"completedAt":WHEN});
        db.execute("INSERT INTO orchestration_v2_projection_runs VALUES ('target','run-1','completed',1,?1)",[run.to_string()]).unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_provider_turns VALUES ('target',1,?1)",
            [json!({"runAttemptId":"attempt-1","model":"gpt-5.4"}).to_string()],
        )
        .unwrap();
        for index in 0..120 {
            let kind = if index == 1 {
                "command_execution"
            } else {
                "user_message"
            };
            let id = format!("item-{index}");
            let msg = format!("message-{index}");
            let text = if index == 2 {
                "🍎text"
            } else {
                "a short item"
            };
            let row = json!({"id":id,"runId":"run-1","type":kind,"status":"completed","title":null,
                "text":text,"input":"printf fixture","output":"fixture","updatedAt":WHEN,"messageId":msg});
            db.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES ('target',?1,'run-1',NULL,?2,?3,?4)",
                params![id,index,kind,row.to_string()]).unwrap();
            db.execute(
                "INSERT INTO orchestration_v2_projection_messages VALUES ('target',?1,?2)",
                params![
                    msg,
                    json!({"createdBy":"user","creationSource":"web"}).to_string()
                ],
            )
            .unwrap();
        }
        let server = ThreadReadMcp::new(path, ENV.into(), "0.1.0".into());
        let app = server.router();
        let socket = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = format!("http://{}/mcp", socket.local_addr().unwrap());
        let host = tokio::spawn(async move {
            axum::serve(socket, app).await.unwrap();
        });
        Self {
            server,
            address,
            client: Client::builder().no_proxy().build().unwrap(),
            host,
            _directory: directory,
        }
    }

    fn provider(&self, thread: &str, session: &str) -> String {
        self.server
            .issue(ProviderScope {
                thread_id: thread.into(),
                provider_instance_id: "codex".into(),
                provider_session_id: session.into(),
            })
            .unwrap()
    }

    fn post(&self, credential: &str) -> reqwest::RequestBuilder {
        self.client
            .post(&self.address)
            .header("authorization", credential)
    }

    async fn initialize(&self, credential: &str) -> String {
        let response=self.post(credential).json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
        session
    }

    async fn call(&self, credential: &str, session: &str, params: Value) -> Value {
        let response=self.post(credential).header("mcp-session-id",session)
            .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"t3_thread_read","arguments":params}}))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = response.json().await.unwrap();
        value["result"].clone()
    }
}

#[tokio::test]
async fn reads_an_archived_other_project_with_canonical_bounded_positions() {
    let fixture = Fixture::new().await;
    let credential = fixture.provider("caller", "provider-a");
    fixture.server.touch_thread("caller");
    let session = fixture.initialize(&credential).await;
    let page = fixture
        .call(
            &credential,
            &session,
            json!({"threadId":"target","environmentId":ENV,"limit":2}),
        )
        .await;
    assert_eq!(page["isError"], false);
    let body = &page["structuredContent"];
    assert_eq!(body["thread"]["archived"], true);
    assert_eq!(body["thread"]["projectId"], "second-project");
    assert_eq!(body["thread"]["itemCount"], 120);
    assert_eq!(body["items"][0]["position"], 118);
    assert_eq!(body["items"][1]["position"], 119);
    assert_eq!(body["earlierPosition"], 118);
    assert_eq!(body["nextPosition"], 119);
    assert_eq!(body["hasMore"], false);
    let forward = fixture
        .call(
            &credential,
            &session,
            json!({"threadId":"target","afterPosition":-1,"limit":2}),
        )
        .await;
    assert_eq!(forward["structuredContent"]["items"][1]["position"], 2);
    assert_eq!(forward["structuredContent"]["nextPosition"], 2);
    assert_eq!(forward["structuredContent"]["hasMore"], true);
    let activity = fixture
        .call(
            &credential,
            &session,
            json!({"threadId":"target","view":"activity","afterPosition":0,"limit":1}),
        )
        .await;
    assert_eq!(
        activity["structuredContent"]["items"][0]["type"],
        "command_execution"
    );
    assert_eq!(activity["structuredContent"]["items"][0]["position"], 1);
    let clipped = fixture
        .call(
            &credential,
            &session,
            json!({"threadId":"target","itemId":"item-2","maxCharsPerItem":1}),
        )
        .await;
    assert_eq!(clipped["structuredContent"]["items"][0]["text"], "🍎");
    assert_eq!(
        clipped["structuredContent"]["items"][0]["nextTextOffset"],
        2
    );
    let bounded = fixture
        .call(
            &credential,
            &session,
            json!({"threadId":"target","limit":1_000_000,"maxCharsPerItem":1_000_000}),
        )
        .await;
    assert!(
        bounded["structuredContent"]["items"]
            .as_array()
            .unwrap()
            .len()
            <= 100
    );
    assert!(bounded["structuredContent"].to_string().len() <= 30_200);
}

#[tokio::test]
async fn rejects_wrong_origin_token_environment_and_detached_session() {
    let fixture = Fixture::new().await;
    let credential = fixture.provider("caller", "provider-a");
    let session = fixture.initialize(&credential).await;
    let other = fixture.provider("neighbor", "provider-b");
    let other_session = fixture.initialize(&other).await;
    let now = Instant::now();
    let (mut desktop, pairing) = DesktopCredentials::new(now);
    let access = desktop
        .exchange(pairing, Permissions::default(), now)
        .unwrap();
    assert!(desktop.session_permissions(access, now).is_some());
    let wrong_auth = fixture
        .post(&format!("Bearer {access}"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_auth.status(), StatusCode::UNAUTHORIZED);
    let origin = fixture
        .post(&credential)
        .header("origin", "http://127.0.0.1")
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(origin.status(), StatusCode::FORBIDDEN);
    let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let wrong_session = fixture
        .post(&other)
        .header("mcp-session-id", &session)
        .json(&list)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_session.status(), StatusCode::NOT_FOUND);
    let discovered = fixture
        .post(&other)
        .header("mcp-session-id", &other_session)
        .json(&list)
        .send()
        .await
        .unwrap();
    assert_eq!(
        discovered.json::<Value>().await.unwrap()["result"]["tools"][0]["name"],
        "t3_thread_read"
    );
    let foreign = fixture
        .call(
            &credential,
            &session,
            json!({"threadId":"target","environmentId":"not-this-machine"}),
        )
        .await;
    assert_eq!(foreign["isError"], true);
    assert!(foreign.get("structuredContent").is_none());
    let detail: Value =
        serde_json::from_str(foreign["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(detail["code"], "capability_denied");
    let deleted = fixture
        .call(&credential, &session, json!({"threadId":"deleted"}))
        .await;
    let detail: Value =
        serde_json::from_str(deleted["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(detail["code"], "thread_not_found");
    fixture.server.revoke_provider_session("provider-a");
    let detached = fixture
        .post(&credential)
        .header("mcp-session-id", &session)
        .json(&list)
        .send()
        .await
        .unwrap();
    assert_eq!(detached.status(), StatusCode::UNAUTHORIZED);
    let preserved = fixture
        .call(&other, &other_session, json!({"threadId":"target"}))
        .await;
    assert_eq!(preserved["isError"], false);
    fixture.server.revoke_thread("neighbor");
    assert_eq!(
        fixture
            .post(&other)
            .json(&list)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
