use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{hmac, rand::SecureRandom};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Claims {
    pub(super) version: u8,
    #[serde(flatten)]
    pub(super) grant: Grant,
    pub(super) expires_at: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(super) enum Grant {
    ProjectFavicon {
        workspace_root: PathBuf,
        relative_path: Option<PathBuf>,
    },
    ProjectFaviconExternal {
        file_path: PathBuf,
    },
}

pub(super) struct Signer {
    directory: PathBuf,
    key: OnceLock<hmac::Key>,
}

impl Signer {
    pub(super) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            key: OnceLock::new(),
        }
    }

    fn key(&self) -> io::Result<&hmac::Key> {
        if let Some(key) = self.key.get() {
            return Ok(key);
        }
        let secret = get_or_create(&self.directory)?;
        Ok(self
            .key
            .get_or_init(|| hmac::Key::new(hmac::HMAC_SHA256, &secret)))
    }

    pub(super) fn issue(&self, claims: &Claims) -> io::Result<String> {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
        let signature = hmac::sign(self.key()?, payload.as_bytes());
        Ok(format!("{payload}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    pub(super) fn verify(&self, token: &str) -> Option<Claims> {
        let mut parts = token.split('.');
        let payload = parts.next()?.as_bytes();
        let signature = URL_SAFE_NO_PAD.decode(parts.next()?).ok()?;
        if signature.len() != 32 {
            return None;
        }
        hmac::verify(self.key().ok()?, payload, &signature).ok()?;
        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        (claims.version == 1 && claims.expires_at > now()).then_some(claims)
    }
}

fn get_or_create(directory: &Path) -> io::Result<Vec<u8>> {
    let path = directory.join("asset-access-signing-key.bin");
    let replacing_empty = match fs::read(&path) {
        Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    let mut bytes = vec![0; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("Could not generate an asset signing key."))?;
    let mut temporary = NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    let persisted = if replacing_empty {
        temporary.persist(&path)
    } else {
        temporary.persist_noclobber(&path)
    };
    match persisted {
        Ok(_) => Ok(bytes),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => fs::read(path),
        Err(error) => Err(error.error),
    }
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
