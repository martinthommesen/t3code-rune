use std::collections::VecDeque;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path as Route, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use futures_util::stream;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{SharedEnvironment, Store, workspace_file};

const GRANT_LIFETIME: Duration = Duration::from_secs(3600);
const CAPACITY: usize = 512;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Input {
    thread_id: String,
    path: String,
}

impl Input {
    pub(super) fn applies(payload: &Value) -> bool {
        payload["resource"]["_tag"] == "media-file"
            && payload["resource"]["disposition"] == "attachment"
    }

    pub(super) fn decode(payload: &Value) -> Option<Self> {
        let input: Self = serde_json::from_value(payload["resource"].clone()).ok()?;
        let relative = &input.path;
        (!input.thread_id.is_empty()
            && !relative.is_empty()
            && relative.encode_utf16().count() <= 512
            && !relative.starts_with(['/', '~', '\\'])
            && !relative.ends_with(['/', '\\'])
            && !relative
                .chars()
                .any(|c| c.is_control() || matches!(c, ':' | '\\'))
            && relative
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..")))
        .then_some(input)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Issued {
    relative_url: String,
    expires_at: u64,
}

struct Grant {
    token: Uuid,
    expires: Instant,
    cwd: PathBuf,
    requested: String,
    path: PathBuf,
    metadata: Metadata,
}

#[derive(Default)]
pub(super) struct Service(Mutex<VecDeque<Grant>>);

impl Service {
    pub(super) fn issue(
        &self,
        store: &Store,
        state_dir: &Path,
        input: Input,
    ) -> Result<Issued, Value> {
        let resource = json!({
            "_tag": "media-file",
            "threadId": input.thread_id,
            "path": input.path,
            "disposition": "attachment",
        });
        let failure = |tag| json!({"_tag": tag, "resource": resource});
        let (project, worktree) = store
            .terminal_workspace(&input.thread_id)
            .map_err(|cause| {
                json!({
                    "_tag": "AssetWorkspaceContextResolutionError",
                    "resource": resource,
                    "cause": {"name": "Error", "message": cause.to_string()},
                })
            })?
            .ok_or_else(|| failure("AssetWorkspaceContextNotFoundError"))?;
        let cwd = worktree.unwrap_or(project);
        let (path, _, metadata) = open(&cwd, &input.path, state_dir)
            .map_err(|_| failure("AssetWorkspaceAssetNotFoundError"))?;
        let name = path
            .file_name()
            .and_then(|part| part.to_str())
            .ok_or_else(|| failure("AssetWorkspaceAssetNotFoundError"))?;
        let token = Uuid::new_v4();
        let mut url = Url::parse("http://workspace.invalid/api/assets/workspace/")
            .expect("static workspace asset base");
        url.path_segments_mut()
            .expect("asset URL path")
            .pop_if_empty()
            .push(&token.to_string())
            .push(name);
        let relative_url = url.path().to_owned();
        let now = Instant::now();
        let expires_at = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000
            + i128::from(GRANT_LIFETIME.as_millis() as u64);
        let mut grants = self.0.lock().expect("workspace attachments");
        grants.retain(|grant| grant.expires > now);
        if grants.len() == CAPACITY {
            grants.pop_front();
        }
        grants.push_back(Grant {
            token,
            expires: now + GRANT_LIFETIME,
            cwd,
            requested: input.path,
            path,
            metadata,
        });
        Ok(Issued {
            relative_url,
            expires_at: u64::try_from(expires_at).unwrap_or_default(),
        })
    }

    fn file(&self, token: Uuid, name: &str, state_dir: &Path) -> io::Result<(File, u64)> {
        let (cwd, requested, path, expected) = {
            let grants = self.0.lock().expect("workspace attachments");
            let grant = grants
                .iter()
                .find(|grant| grant.token == token && grant.expires > Instant::now())
                .filter(|grant| grant.path.file_name().is_some_and(|part| part == name))
                .ok_or(io::ErrorKind::NotFound)?;
            (
                grant.cwd.clone(),
                grant.requested.clone(),
                grant.path.clone(),
                grant.metadata.clone(),
            )
        };
        let (opened_path, file, metadata) = open(&cwd, &requested, state_dir)?;
        if opened_path != path || !same_identity(&expected, &metadata) {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok((file, metadata.len()))
    }
}

fn open(cwd: &Path, requested: &str, state_dir: &Path) -> io::Result<(PathBuf, File, Metadata)> {
    let (path, file) = workspace_file::open_attachment(cwd, requested, state_dir)?
        .ok_or(io::ErrorKind::NotFound)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || path.canonicalize()? != path
        || !same_identity(&metadata, &path.metadata()?)
    {
        return Err(io::ErrorKind::NotFound.into());
    }
    Ok((path, file, metadata))
}

fn same_identity(left: &Metadata, right: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        left.created().ok() == right.created().ok() && left.len() == right.len()
    }
}

pub(super) async fn get(
    State(environment): State<SharedEnvironment>,
    Route((token, name)): Route<(Uuid, String)>,
    method: Method,
) -> Response {
    let opened = tokio::task::spawn_blocking(move || {
        let local = environment.local.as_ref().ok_or(io::ErrorKind::NotFound)?;
        let state_dir = local
            .settings
            .path()
            .parent()
            .expect("local state directory");
        local.workspace_attachments.file(token, &name, state_dir)
    })
    .await;
    let Ok(Ok((file, size))) = opened else {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_FOUND;
        return response;
    };
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(stream::try_unfold(
            (file, size),
            |(mut file, remaining)| async move {
                if remaining == 0 {
                    return Ok::<_, io::Error>(None);
                }
                tokio::task::spawn_blocking(move || {
                    let count = remaining.min(64 * 1024);
                    let mut bytes = vec![0; count as usize];
                    file.read_exact(&mut bytes)?;
                    Ok(Some((bytes, (file, remaining - count))))
                })
                .await
                .map_err(io::Error::other)?
            },
        ))
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, size.into());
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_resources_use_relative_regular_file_paths() {
        let payload = |path| {
            json!({"resource":{
                "_tag": "media-file",
                "threadId": "thread",
                "path": path,
                "disposition": "attachment"
            }})
        };
        assert!(Input::decode(&payload("notes/binary.blob")).is_some());
        for path in ["/tmp/file", "../file", "notes/../file", "notes/", "C:/file"] {
            assert!(Input::decode(&payload(path)).is_none(), "{path}");
        }
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("notes")).unwrap();
        assert!(open(workspace.path(), "notes", workspace.path()).is_err());
    }
}
