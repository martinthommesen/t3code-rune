mod credentials;
mod history;
mod projection;

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use credentials::Credentials;
pub(super) use credentials::ProviderScope;
use projection::Read;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const SESSION: &str = "mcp-session-id";
const PROTOCOLS: [&str; 2] = ["2025-06-18", "2026-07-28"];
const TOOL: &str = "t3_thread_read";

#[derive(Clone)]
pub(super) struct ThreadReadMcp {
    state: Arc<Runtime>,
}

struct Runtime {
    database: PathBuf,
    environment_id: String,
    version: String,
    credentials: Mutex<Credentials>,
    tool: Value,
}

impl ThreadReadMcp {
    pub(super) fn new(database: PathBuf, environment_id: String, version: String) -> Self {
        Self {
            state: Arc::new(Runtime {
                database,
                environment_id,
                version,
                credentials: Mutex::default(),
                tool: serde_json::from_str(include_str!("thread_read_mcp/tool.json"))
                    .expect("T3 thread read tool schema"),
            }),
        }
    }

    pub(super) fn router(&self) -> Router {
        Router::new()
            .route("/mcp", post(handle).get(handle).delete(handle))
            .with_state(Arc::clone(&self.state))
    }

    pub(super) fn issue(&self, scope: ProviderScope) -> io::Result<String> {
        self.credentials().issue(scope, Instant::now())
    }

    pub(super) fn touch_thread(&self, thread_id: &str) {
        self.credentials().touch(thread_id, Instant::now());
    }

    pub(super) fn revoke_thread(&self, thread_id: &str) {
        self.credentials().revoke_thread(thread_id);
    }

    pub(super) fn revoke_provider_session(&self, provider_session_id: &str) {
        self.credentials().revoke_session(provider_session_id);
    }

    fn credentials(&self) -> std::sync::MutexGuard<'_, Credentials> {
        self.state
            .credentials
            .lock()
            .expect("provider credential registry")
    }
}

#[derive(Deserialize)]
struct Envelope {
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
struct ToolCall {
    name: String,
    #[serde(default)]
    arguments: Value,
}

async fn handle(State(server): State<Arc<Runtime>>, request: Request) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(token) = bearer(request.headers()).map(str::to_owned) else {
        return unauthorized();
    };
    let now = Instant::now();
    let scope = server
        .credentials
        .lock()
        .expect("provider credential registry")
        .resolve(&token, now);
    let Some(scope) = scope else {
        return unauthorized();
    };
    let session = request
        .headers()
        .get(SESSION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| Uuid::try_parse(header).ok());
    if request.method() == axum::http::Method::DELETE {
        return match session {
            Some(session)
                if server
                    .credentials
                    .lock()
                    .expect("provider credential registry")
                    .close_session(&token, session, now) =>
            {
                StatusCode::NO_CONTENT
            }
            _ => StatusCode::NOT_FOUND,
        }
        .into_response();
    }
    if request.method() != axum::http::Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Ok(body) = to_bytes(request.into_body(), MAX_REQUEST_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let Ok(call) = serde_json::from_slice::<Envelope>(&body) else {
        return rpc_error(Value::Null, -32700, "Parse error");
    };
    if call.jsonrpc != "2.0"
        || !matches!(call.id, Value::Null | Value::String(_) | Value::Number(_))
    {
        return rpc_error(Value::Null, -32600, "Invalid Request");
    }
    if call.method == "initialize" {
        if call.id.is_null() {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let Some(version) = call.params["protocolVersion"].as_str() else {
            return rpc_error(call.id, -32602, "Invalid initialize parameters");
        };
        let protocol = if PROTOCOLS.contains(&version) {
            version
        } else {
            PROTOCOLS[1]
        };
        let Some(session) = server
            .credentials
            .lock()
            .expect("provider credential registry")
            .initialize(&token, Instant::now())
        else {
            return unauthorized();
        };
        let mut response = rpc_result(
            call.id,
            json!({"protocolVersion":protocol,"capabilities":{"tools":{}},"serverInfo":{"name":"T3 Code","version":server.version}}),
        );
        response.headers_mut().insert(
            SESSION,
            HeaderValue::from_str(&session.to_string()).expect("MCP session id"),
        );
        return response;
    }
    let Some(session) = session else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !server
        .credentials
        .lock()
        .expect("provider credential registry")
        .has_session(&token, session, Instant::now())
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if call.id.is_null() {
        return StatusCode::ACCEPTED.into_response();
    }
    match call.method.as_str() {
        "ping" => rpc_result(call.id, json!({})),
        "tools/list" => rpc_result(call.id, json!({"tools":[server.tool]})),
        "tools/call" => {
            let Ok(tool) = serde_json::from_value::<ToolCall>(call.params) else {
                return rpc_error(call.id, -32602, "Invalid tool call parameters");
            };
            if tool.name != TOOL {
                return rpc_error(call.id, -32602, "Unknown tool");
            }
            let Ok(input) = serde_json::from_value::<Read>(tool.arguments) else {
                return rpc_error(call.id, -32602, "Invalid thread read arguments");
            };
            if !input.valid() {
                return rpc_error(call.id, -32602, "Invalid thread read arguments");
            }
            if input
                .environment_id
                .as_deref()
                .is_some_and(|id| id != server.environment_id)
            {
                return rpc_result(
                    call.id,
                    tool_failure(
                        "capability_denied",
                        "Linked environments is off on this machine, so no tool can act on another machine. Leave out environmentId to work here; the user can turn it on in Settings → Labs.",
                    ),
                );
            }
            let output = tokio::task::spawn_blocking(move || {
                projection::read(&server.database, &server.environment_id, &scope, &input)
            })
            .await;
            let result = match output {
                Ok(Ok(value)) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Ok(Err(projection::Failure::MissingTarget(thread))) => tool_failure(
                    "thread_not_found",
                    &format!("Thread {thread} is no longer available."),
                ),
                Ok(Err(projection::Failure::Unsupported)) => tool_failure(
                    "capability_denied",
                    "This reader supports ordinary local threads.",
                ),
                _ => tool_failure(
                    "orchestration_error",
                    "The operation could not be completed.",
                ),
            };
            rpc_result(call.id, result)
        }
        _ => rpc_error(call.id, -32601, "Method not found"),
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED,Json(json!({"error":"invalid_mcp_credential","message":"A valid T3 Code MCP credential is required."}))).into_response()
}

fn rpc_result(id: Value, result: Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}

fn tool_failure(code: &str, message: &str) -> Value {
    let detail = json!({"_tag":"OrchestratorMcpFailure","code":code,"message":message});
    json!({"content":[{"type":"text","text":detail.to_string()}],"isError":true})
}

#[cfg(test)]
mod tests;
