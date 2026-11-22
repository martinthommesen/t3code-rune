mod bootstrap;
mod credentials;
mod environment_identity;
mod filesystem_browse;
mod git_checkpoint;
mod launch_model;
mod legacy_thread_shells;
mod locale;
mod migrations;
mod named_projects;
mod project_clone;
mod project_commands;
mod repository_identity;
mod rpc;
mod settings;
mod shell_http;
mod store;
mod terminal;
mod terminal_rpc;
mod text_generation;
mod thread_commands;
mod thread_launch;
mod turn_commands;
mod turn_worker;
mod vcs_checkout;
mod vcs_refs;
mod vcs_status;

pub(crate) use settings::parse_settings_json;

use std::error::Error;
use std::io::{self, IsTerminal};
use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, Form, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

use credentials::{Credentials, Permissions, SESSION_LIFETIME};
use environment_identity::Identity;
use project_clone::ProjectClones;
use project_commands::ProjectCommands;
use settings::SettingsFile;
use store::Store;
use text_generation::initial_title;
use thread_commands::ThreadCommands;
use turn_commands::TurnCommands;
use turn_worker::{TurnQueue, TurnWorker};

const PROTOCOL_VERSION: u8 = 2;
const TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const BOOTSTRAP_TOKEN: &str = "urn:t3:params:oauth:token-type:environment-bootstrap";
const ACCESS_TOKEN: &str = "urn:ietf:params:oauth:token-type:access_token";
const MAX_RPC_MESSAGE: usize = 16 * 1024;
const MAX_LOCAL_RPC_MESSAGE: usize = 512 * 1024;
const OWNERSHIP_DATABASE: &str = "server-owner.sqlite";

struct Environment {
    port: u16,
    identity: Identity,
    credentials: Mutex<Credentials>,
    store: Store,
    local: Option<Arc<LocalOperations>>,
}

type SharedEnvironment = Arc<Environment>;

struct LocalOperations {
    projects: ProjectCommands,
    named_projects_root: PathBuf,
    project_clones: ProjectClones,
    repository_identities: repository_identity::Identities,
    threads: ThreadCommands,
    turns: TurnCommands,
    turn_queue: TurnQueue,
    settings: SettingsFile,
    projection_changes: broadcast::Sender<ProjectionChange>,
    terminals: Arc<terminal::TerminalManager>,
    vcs_status: vcs_status::Service,
}

#[derive(Clone)]
enum ProjectionChange {
    Project,
    Thread(String),
}

pub(crate) struct LocalHost {
    address: SocketAddr,
    state_dir: PathBuf,
    pairing_code: Uuid,
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    _ownership: rusqlite::Connection,
}

impl LocalHost {
    pub(crate) fn start(database: PathBuf) -> Result<Self, Box<dyn Error>> {
        let database = database_location(&database)?;
        let ownership = rusqlite::Connection::open(database.with_file_name(OWNERSHIP_DATABASE))?;
        ownership.busy_timeout(Duration::from_millis(250))?;
        ownership
            .execute_batch("BEGIN EXCLUSIVE")
            .map_err(|error| {
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "Another T3 Code server owns this database directory.",
                    )
                } else {
                    io::Error::other(error)
                }
            })?;
        bootstrap::snapshot_legacy_database(&database)?;
        bootstrap::initialize(&database)?;
        migrations::migrate(&database)?;
        legacy_thread_shells::reconcile(&database)?;
        let state_dir = database
            .parent()
            .expect("The database path is absolute")
            .to_owned();
        let environment_id = environment_identity::load_id(&state_dir)?;
        let terminals = Arc::new(terminal::TerminalManager::with_state_dir(&state_dir)?);
        let named_projects_root = named_projects::root_for(&state_dir);
        let settings = SettingsFile::new(state_dir.join("settings.json"));
        let cwd = std::env::current_dir()?;
        let store = Store::open(database.clone())?;
        let turns = TurnCommands::new(database.clone());
        let threads = ThreadCommands::open(database.clone())?;
        let projects = ProjectCommands::open(database.clone())?;
        let projection_changes = broadcast::channel(64).0;
        TurnWorker::recover_before_spawn(&turns, &projection_changes)?;
        initial_title::recover(&database)?;
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let listener = {
            let _entered = runtime.enter();
            TcpListener::from_std(listener)?
        };
        let (shutdown, stop) = oneshot::channel();
        let (ready, startup) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("t3-local-host".into())
            .spawn(move || {
                runtime.block_on(async {
                    let identity = Identity::local(environment_id, &cwd).await;
                    let turn_worker = TurnWorker::spawn(
                        turns.clone(),
                        settings.clone(),
                        projection_changes.clone(),
                    );
                    let title_settings = settings.clone();
                    let title_changes = projection_changes.clone();
                    let title_queue = turn_worker.sender();
                    let titles = tokio::spawn(async move {
                        if let Err(error) =
                            initial_title::run(database, title_settings, title_changes, title_queue)
                                .await
                        {
                            eprintln!("The local thread title worker stopped: {error}");
                        }
                    });
                    let repository_identities = repository_identity::Identities::default();
                    let project_clones = ProjectClones::new(
                        projects.clone(),
                        projection_changes.clone(),
                        repository_identities.clone(),
                    );
                    let local = LocalOperations {
                        projects,
                        named_projects_root,
                        project_clones: project_clones.clone(),
                        repository_identities,
                        threads,
                        turns,
                        turn_queue: turn_worker.sender(),
                        settings,
                        projection_changes,
                        terminals,
                        vcs_status: vcs_status::Service::default(),
                    };
                    let (environment, code) =
                        environment(address, identity, store, Some(Arc::new(local)));
                    if ready.send(code).is_err() {
                        titles.abort();
                        let _ = titles.await;
                        let _ = turn_worker.shutdown().await;
                        return;
                    }
                    tokio::select! {
                        _ = stop => (),
                        result = axum::serve(listener, router(environment)) => {
                            if let Err(error) = result {
                                eprintln!("The local T3 Code server stopped: {error}");
                            }
                        }
                    }
                    titles.abort();
                    let _ = titles.await;
                    project_clones.shutdown().await;
                    if let Err(error) = turn_worker.shutdown().await {
                        eprintln!("The local Codex worker stopped: {error}");
                    }
                });
            })?;
        let pairing_code = startup.recv()?;
        Ok(Self {
            address,
            state_dir,
            pairing_code,
            shutdown: Some(shutdown),
            worker: Some(worker),
            _ownership: ownership,
        })
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub(crate) fn pairing_code(&self) -> String {
        self.pairing_code.to_string()
    }
}

fn database_location(requested: &Path) -> io::Result<PathBuf> {
    let invalid_path = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Choose a path for the T3 Code SQLite database.",
        )
    };
    let filename = requested.file_name().ok_or_else(invalid_path)?;
    let database = match std::fs::canonicalize(requested) {
        Ok(path) if path.is_file() => path,
        Ok(_) => return Err(invalid_path()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if requested.symlink_metadata().is_ok() {
                return Err(error);
            }
            let parent = requested
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)?;
            std::fs::canonicalize(parent)?.join(filename)
        }
        Err(error) => return Err(error),
    };
    if database
        .file_name()
        .is_some_and(|name| name == OWNERSHIP_DATABASE)
    {
        return Err(invalid_path());
    }
    Ok(database)
}

impl Drop for LocalHost {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn environment(
    address: SocketAddr,
    identity: Identity,
    store: Store,
    local: Option<Arc<LocalOperations>>,
) -> (SharedEnvironment, Uuid) {
    let (credentials, code) = Credentials::new(Instant::now());
    let environment = Arc::new(Environment {
        port: address.port(),
        identity,
        credentials: Mutex::new(credentials),
        store,
        local,
    });
    (environment, code)
}

pub(crate) fn run(database: PathBuf, port: u16) -> Result<(), Box<dyn Error>> {
    if !io::stderr().is_terminal() {
        return Err("The read-only server needs a terminal to display its pairing code.".into());
    }
    let store = Store::open(database)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
            let address = listener.local_addr()?;
            let (environment, code) = environment(address, Identity::history(), store, None);
            eprintln!("T3 Code History is listening at http://{address}");
            eprintln!("Pairing code: {code}");
            axum::serve(listener, router(environment)).await
        })?;
    Ok(())
}

fn router(environment: SharedEnvironment) -> Router {
    Router::new()
        .route("/.well-known/t3/environment", get(descriptor))
        .route("/oauth/token", post(exchange))
        .route("/api/auth/websocket-ticket/rpc", post(issue_ticket))
        .route("/api/orchestration/shell", get(shell_http::snapshot))
        .route("/ws", get(upgrade))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&environment),
            local_requests,
        ))
        .with_state(environment)
}

async fn local_requests(
    State(environment): State<SharedEnvironment>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let headers = request.headers();
    let local_host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .is_some_and(|host| {
            if host == "127.0.0.1" {
                return environment.port == 80;
            }
            host.parse::<SocketAddr>().is_ok_and(|address| {
                address.ip() == Ipv4Addr::LOCALHOST && address.port() == environment.port
            })
        });
    if !local_host || headers.contains_key(header::ORIGIN) {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn descriptor(State(environment): State<SharedEnvironment>) -> Json<Value> {
    let local = environment.local.is_some();
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => "unknown",
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => "other",
    };
    Json(json!({
        "environmentId": environment.identity.id,
        "label": environment.identity.label,
        "platform": {"os": os, "arch": arch},
        "serverVersion": env!("CARGO_PKG_VERSION"),
        "orchestrationProtocolVersion": PROTOCOL_VERSION,
        "capabilities": {"t3ReadOnly": true, "t3ProjectWrites": local, "t3NamedProjects": local, "t3CloneProjects": local, "t3PromptWrites": local, "t3ThreadLaunch": local, "t3WorktreeLaunch": local, "t3RemoteWorktreeLaunch": local, "t3WorktreeSetup": local, "t3WorktreeQueue": local, "t3ShellHttp": local, "t3CheckoutSwitch": local, "t3ThreadStop": local, "t3ArchiveSnapshots": local, "t3ThreadVisits": local, "t3Terminal": local}
    }))
}

#[derive(Deserialize)]
struct TokenExchange {
    grant_type: String,
    subject_token: String,
    subject_token_type: String,
    requested_token_type: String,
    scope: Option<String>,
}

async fn exchange(
    State(environment): State<SharedEnvironment>,
    headers: HeaderMap,
    Form(exchange): Form<TokenExchange>,
) -> Result<Json<Value>, StatusCode> {
    let supported_scope = exchange.scope.as_deref().is_none_or(|scope| {
        scope
            .split_whitespace()
            .any(|value| value == "orchestration:read")
    });
    if exchange.grant_type != TOKEN_EXCHANGE
        || exchange.subject_token_type != BOOTSTRAP_TOKEN
        || exchange.requested_token_type != ACCESS_TOKEN
        || !supported_scope
        || headers.contains_key("dpop")
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let code = Uuid::parse_str(&exchange.subject_token).map_err(|_| StatusCode::UNAUTHORIZED)?;
    let allows = |name| {
        environment.local.is_some()
            && exchange
                .scope
                .as_deref()
                .is_none_or(|scope| scope.split_whitespace().any(|value| value == name))
    };
    let permissions = Permissions {
        orchestration_operate: allows("orchestration:operate"),
        filesystem_read: allows("filesystem:read"),
        source_control_write: allows("source-control:write"),
        terminal_read: allows("terminal:read"),
        terminal_operate: allows("terminal:operate"),
    };
    let access_token = environment
        .credentials
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .exchange(code, permissions, Instant::now())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    Ok(Json(json!({
        "access_token": access_token.to_string(),
        "issued_token_type": ACCESS_TOKEN,
        "token_type": "Bearer",
        "expires_in": SESSION_LIFETIME.as_secs(),
        "scope": permissions.scopes()
    })))
}

async fn issue_ticket(
    State(environment): State<SharedEnvironment>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let access_token = bearer_token(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let (ticket, ttl) = environment
        .credentials
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .issue_ticket(access_token, Instant::now())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let expires_at = (OffsetDateTime::now_utc() + ttl)
        .format(&Rfc3339)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        json!({"ticket": ticket.to_string(), "expiresAt": expires_at}),
    ))
}

fn bearer_token(headers: &HeaderMap) -> Option<Uuid> {
    let (scheme, value) = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    Uuid::parse_str(value).ok()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SocketQuery {
    orchestration_protocol: u8,
    ws_ticket: String,
}

async fn upgrade(
    State(environment): State<SharedEnvironment>,
    Query(query): Query<SocketQuery>,
    websocket: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    if query.orchestration_protocol != PROTOCOL_VERSION {
        return Err(StatusCode::BAD_REQUEST);
    }
    let ticket = Uuid::parse_str(&query.ws_ticket).map_err(|_| StatusCode::UNAUTHORIZED)?;
    let (expires, permissions) = environment
        .credentials
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .consume_ticket(ticket, Instant::now())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let store = environment.store.clone();
    let local = environment.local.clone();
    let message_limit = if local.is_some() {
        MAX_LOCAL_RPC_MESSAGE
    } else {
        MAX_RPC_MESSAGE
    };
    Ok(websocket
        .max_message_size(message_limit)
        .max_frame_size(message_limit)
        .on_upgrade(move |socket| async move {
            let deadline = tokio::time::Instant::from_std(expires);
            let _ =
                tokio::time::timeout_at(deadline, rpc::serve(socket, store, local, permissions))
                    .await;
        }))
}
