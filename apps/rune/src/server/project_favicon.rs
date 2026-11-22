mod capability;
mod discovery;
mod http;
mod project_file;

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ring::digest;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::project_clone::ProjectClones;
use super::project_commands::workspace_path;
use capability::{Claims, Grant, Signer};
use discovery::{ProbeError, filesystem_cause, is_file, normalize, not_found, within_workspace};

pub(super) use discovery::trim;
pub(super) use http::get;

const CACHE_CAPACITY: usize = 512;
const CACHE_HIT_LIFETIME: Duration = Duration::from_secs(600);
const CACHE_MISS_LIFETIME: Duration = Duration::from_secs(60);
const TOKEN_BUCKET_MS: u64 = 30 * 60 * 1_000;
const FALLBACK_MARKER: &str = "project-favicon-missing";

#[derive(Deserialize)]
pub(super) struct Input {
    resource: Resource,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "_tag", rename_all = "kebab-case")]
enum Resource {
    ProjectFavicon {
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

impl Input {
    pub(super) fn decode(payload: Value) -> Option<Self> {
        if payload["resource"].get("path").is_some_and(Value::is_null) {
            return None;
        }
        let mut input = serde_json::from_value::<Self>(payload).ok()?;
        let Resource::ProjectFavicon { cwd, path } = &mut input.resource;
        *cwd = trim(cwd).to_owned();
        if cwd.is_empty() || cwd.encode_utf16().count() > 1024 {
            return None;
        }
        if let Some(path) = path {
            *path = trim(path).to_owned();
            if !valid_user_path(path) {
                return None;
            }
        }
        Some(input)
    }
}

pub(super) fn valid_user_path(value: &str) -> bool {
    value.encode_utf16().count() <= 1024
        && matches!(
            extension(value).as_deref(),
            Some("avif" | "gif" | "icns" | "ico" | "jpeg" | "jpg" | "png" | "svg" | "webp")
        )
}

fn supported_image(value: &str) -> bool {
    matches!(
        extension(value).as_deref(),
        Some(
            "avif"
                | "bmp"
                | "gif"
                | "heic"
                | "heif"
                | "icns"
                | "ico"
                | "jpeg"
                | "jpg"
                | "png"
                | "svg"
                | "tif"
                | "tiff"
                | "webp"
        )
    )
}

fn extension(value: &str) -> Option<String> {
    Some(value.rsplit_once('.')?.1.to_ascii_lowercase())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Issued {
    relative_url: String,
    expires_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_path: Option<String>,
}

struct CachedPath {
    workspace: PathBuf,
    override_path: Option<String>,
    path: Option<PathBuf>,
    expires: Instant,
}

pub(super) struct Service {
    state_dir: PathBuf,
    signer: Signer,
    paths: Mutex<VecDeque<CachedPath>>,
}

impl Service {
    pub(super) fn new(state_dir: PathBuf) -> Self {
        Self {
            signer: Signer::new(state_dir.join("secrets")),
            state_dir,
            paths: Mutex::default(),
        }
    }

    pub(super) fn invalidate(&self, workspace: &Path) {
        self.paths
            .lock()
            .expect("favicon cache")
            .retain(|entry| entry.workspace != workspace);
    }

    pub(super) fn issue(
        &self,
        database: &Path,
        clones: &ProjectClones,
        input: Input,
    ) -> Result<Issued, Value> {
        let Resource::ProjectFavicon { cwd, .. } = &input.resource;
        let error = |name: &str| json!({"_tag": name, "resource": input.resource});
        let with_cause = |name: &str, cause: &dyn std::fmt::Display| {
            json!({
                "_tag": name,
                "resource": input.resource,
                "cause": { "name": "Error", "message": cause.to_string() },
            })
        };
        let (project_id, override_path) = registered_project(database, cwd)
            .map_err(|cause| with_cause("AssetWorkspaceContextResolutionError", &cause))?
            .ok_or_else(|| error("AssetWorkspaceContextNotFoundError"))?;
        let workspace = workspace_path(cwd)
            .map(PathBuf::from)
            .map_err(|cause| with_cause("AssetWorkspaceRootNormalizationError", &cause))?;
        normalize_workspace_directory(&workspace).map_err(|cause| {
            json!({
                "_tag": "AssetWorkspaceRootNormalizationError",
                "resource": input.resource,
                "cause": cause,
            })
        })?;

        let found = match clones.checkout_ready(&project_id, cwd) {
            Some(false) => None,
            checkout => self
                .resolve(&workspace, override_path.as_deref(), checkout == Some(true))
                .map_err(|cause| {
                    json!({
                        "_tag": "AssetProjectFaviconResolutionError",
                        "resource": input.resource,
                        "cause": cause.cause(&workspace),
                    })
                })?,
        };
        let root = fs::canonicalize(&workspace)
            .map_err(|cause| with_cause("AssetWorkspaceResolutionError", &cause))?;

        let (grant, source_path, file_name) = if let Some(path) = found {
            let external = override_path.as_deref().is_some_and(|value| {
                let value = Path::new(value);
                value.is_absolute() && normalize(&path) == normalize(value)
            });
            let source = if external {
                path.as_path()
            } else {
                path.strip_prefix(&workspace)
                    .map_err(|_| error("AssetProjectFaviconNotFoundError"))?
            };
            let source_path = source.to_string_lossy().into_owned();
            if !supported_image(&source_path) {
                return Err(error("AssetPreviewTypeValidationError"));
            }
            let actual = if external {
                self.canonical_file(source)
            } else {
                self.workspace_file(&root, source)
            }
            .map_err(|cause| with_cause("AssetProjectFaviconInspectionError", &cause))?
            .ok_or_else(|| error("AssetProjectFaviconNotFoundError"))?;
            let revision = sha256_file(&actual)
                .map_err(|cause| with_cause("AssetProjectFaviconInspectionError", &cause))?;
            let file_name = source.file_name().unwrap_or_default().to_string_lossy();
            let grant = if external {
                Grant::ProjectFaviconExternal { file_path: actual }
            } else {
                Grant::ProjectFavicon {
                    workspace_root: root,
                    relative_path: Some(source.to_owned()),
                }
            };
            (grant, Some(source_path), format!("v{revision}-{file_name}"))
        } else {
            (
                Grant::ProjectFavicon {
                    workspace_root: root,
                    relative_path: None,
                },
                None,
                FALLBACK_MARKER.to_owned(),
            )
        };
        let expires_at = (capability::now() / TOKEN_BUCKET_MS + 2) * TOKEN_BUCKET_MS;
        let token = self
            .signer
            .issue(&Claims {
                version: 1,
                grant,
                expires_at,
            })
            .map_err(|cause| with_cause("AssetSigningKeyLoadError", &cause))?;
        Ok(Issued {
            relative_url: format!("/api/assets/{token}/{}", uri_component(&file_name)),
            expires_at,
            source_path,
        })
    }

    fn resolve(
        &self,
        workspace: &Path,
        override_path: Option<&str>,
        cloned: bool,
    ) -> Result<Option<PathBuf>, ProbeError> {
        let now = Instant::now();
        let mut cache = self.paths.lock().expect("favicon cache");
        cache.retain(|entry| entry.expires > now && (!cloned || entry.workspace != workspace));
        let position = cache.iter().position(|entry| {
            entry.workspace == workspace && entry.override_path.as_deref() == override_path
        });
        let cached = position.and_then(|position| cache.remove(position));
        let previous = cached.as_ref().map(|entry| entry.path.clone());
        if let Some(cached) = cached {
            cache.push_back(cached);
        }
        drop(cache);
        if let Some(previous) = previous {
            match previous.as_deref() {
                None => return Ok(None),
                Some(path) if is_file(path).map_err(|error| ProbeError::stat(path, error))? => {
                    return Ok(previous);
                }
                Some(_) => {}
            }
        }

        let path = discovery::resolve(workspace, override_path)?;
        let expires = now
            + if path.is_some() {
                CACHE_HIT_LIFETIME
            } else {
                CACHE_MISS_LIFETIME
            };
        let mut cache = self.paths.lock().expect("favicon cache");
        cache.retain(|entry| {
            entry.workspace != workspace || entry.override_path.as_deref() != override_path
        });
        if cache.len() == CACHE_CAPACITY {
            cache.pop_front();
        }
        cache.push_back(CachedPath {
            workspace: workspace.to_owned(),
            override_path: override_path.map(str::to_owned),
            path: path.clone(),
            expires,
        });
        Ok(path)
    }

    fn file(&self, token: &str) -> Option<PathBuf> {
        match self.signer.verify(token)?.grant {
            Grant::ProjectFavicon {
                workspace_root,
                relative_path: Some(relative_path),
            } => self
                .workspace_file(&workspace_root, &relative_path)
                .ok()
                .flatten(),
            Grant::ProjectFavicon {
                relative_path: None,
                ..
            } => None,
            Grant::ProjectFaviconExternal { file_path } => self
                .canonical_file(&file_path)
                .ok()
                .flatten()
                .filter(|canonical| canonical == &file_path),
        }
    }

    fn canonical_file(&self, path: &Path) -> io::Result<Option<PathBuf>> {
        let Some(file) = not_found(fs::canonicalize(path))? else {
            return Ok(None);
        };
        Ok((is_file(&file)? && !self.internal_path(&file)).then_some(file))
    }

    fn workspace_file(&self, root: &Path, relative: &Path) -> io::Result<Option<PathBuf>> {
        let Some(path) = within_workspace(root, relative) else {
            return Ok(None);
        };
        let Some(root) = not_found(fs::canonicalize(root))? else {
            return Ok(None);
        };
        Ok(self.canonical_file(&path)?.filter(|file| {
            file.strip_prefix(&root).is_ok_and(|relative| {
                !relative.as_os_str().is_empty() && !relative.to_string_lossy().starts_with("..")
            })
        }))
    }

    fn internal_path(&self, file: &Path) -> bool {
        let secrets = self.state_dir.join("secrets");
        let secrets = fs::canonicalize(&secrets).unwrap_or(secrets);
        file.starts_with(secrets)
            || (file.parent() == Some(&self.state_dir)
                && file
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(".sqlite")))
            || file.starts_with("/dev/fd")
    }
}

fn normalize_workspace_directory(workspace: &Path) -> Result<(), Value> {
    let displayed = workspace.display();
    match fs::metadata(workspace) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(json!({
            "name": "WorkspaceRootNotDirectoryError",
            "message": format!("Workspace root is not a directory: {displayed}"),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(json!({
            "name": "WorkspaceRootNotExistsError",
            "message": format!("Workspace root does not exist: {displayed}"),
        })),
        Err(error) => Err(json!({
            "name": "WorkspaceRootStatFailedError",
            "message": format!("Failed to stat workspace root '{displayed}' during 'validate-existing'."),
            "cause": filesystem_cause("stat", workspace, &error),
        })),
    }
}

fn registered_project(
    database: &Path,
    workspace: &str,
) -> rusqlite::Result<Option<(String, Option<String>)>> {
    let database = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    database.busy_timeout(Duration::from_secs(2))?;
    database
        .query_row(
            "SELECT project_id, favicon_path FROM projection_projects
             WHERE workspace_root = ?1 AND deleted_at IS NULL
             ORDER BY created_at, project_id LIMIT 1",
            [workspace],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
}

fn sha256_file(path: &Path) -> io::Result<String> {
    use std::fmt::Write as _;

    let mut file = File::open(path)?;
    let mut hash = digest::Context::new(&digest::SHA256);
    let mut buffer = [0; 32 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hash.finish().as_ref() {
        write!(hex, "{byte:02x}").expect("hash formatted into string");
    }
    Ok(hex)
}

fn uri_component(value: &str) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            output.push(char::from(byte));
        } else {
            write!(output, "%{byte:02X}").expect("URI formatted into string");
        }
    }
    output
}
