use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path};

use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY, SHA256};

use super::git::Git;
use super::remove_worktree::AutomaticInspectionError;

const OPERATION: &str = "StorageCleanup.hiddenTrackedChanges";
const MAX_FLAGGED_FILES: usize = 10_000;

struct HiddenFile {
    name: String,
    mode: String,
    object: String,
}

enum ReadHiddenFile {
    Missing,
    Regular,
    Link(Vec<u8>),
    Other,
}

pub(super) async fn first_change(
    git: Git<'_>,
    worktree: &Path,
) -> Result<Option<String>, AutomaticInspectionError> {
    use AutomaticInspectionError::{Failed, Kept};

    let sparse = git
        .checked(
            OPERATION,
            &[
                "config",
                "--type=bool",
                "--default=false",
                "core.sparseCheckout",
            ],
            None,
        )
        .await?;
    let sparse = sparse.stdout.trim() == "true";
    let tracked = git
        .checked(OPERATION, &["ls-files", "-s", "-v", "-z"], None)
        .await?;
    if !tracked.stdout.is_empty() && !tracked.stdout.ends_with('\0') {
        return Err(Kept(
            "Hidden tracked files exceed the inspection limit".to_owned(),
        ));
    }
    let flagged: Vec<_> = tracked
        .stdout
        .split_terminator('\0')
        .filter_map(|record| {
            let (header, name) = record.split_once('\t')?;
            let mut fields = header.split_ascii_whitespace();
            let flag = fields.next()?.as_bytes().first().copied()?;
            let mode = fields.next()?;
            if !(flag.is_ascii_lowercase() || !sparse && flag == b'S') || mode == "160000" {
                return None;
            }
            let object = fields.next()?;
            Some(HiddenFile {
                name: name.to_owned(),
                mode: mode.to_owned(),
                object: object.to_owned(),
            })
        })
        .take(MAX_FLAGGED_FILES + 1)
        .collect();
    if flagged.len() > MAX_FLAGGED_FILES {
        return Err(Kept(
            "Hidden tracked files exceed the inspection limit".to_owned(),
        ));
    }
    let root = worktree.to_owned();
    let inspected = tokio::task::spawn_blocking(move || {
        flagged
            .into_iter()
            .map(|file| {
                let path = Path::new(&file.name);
                if file.name.contains(['\n', '\r', '\u{fffd}'])
                    || path
                        .components()
                        .any(|item| !matches!(item, Component::Normal(_)))
                {
                    return Ok((file, ReadHiddenFile::Other));
                }
                let path = root.join(path);
                if let Some(parent) = path.parent()
                    && parent
                        .canonicalize()
                        .is_ok_and(|physical| physical != parent)
                {
                    return Ok((file, ReadHiddenFile::Other));
                }
                let contents = match path.symlink_metadata() {
                    Ok(metadata) if metadata.is_symlink() => ReadHiddenFile::Link(
                        fs::read_link(path)?.as_os_str().as_encoded_bytes().to_vec(),
                    ),
                    Ok(metadata) if metadata.is_file() => ReadHiddenFile::Regular,
                    Ok(_) => ReadHiddenFile::Other,
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::NotFound | ErrorKind::NotADirectory
                        ) =>
                    {
                        ReadHiddenFile::Missing
                    }
                    Err(error) => return Err(error),
                };
                Ok((file, contents))
            })
            .collect::<std::io::Result<Vec<_>>>()
    })
    .await
    .map_err(|_| Failed("Could not inspect hidden tracked files.".to_owned()))?
    .map_err(|error| Failed(error.to_string()))?;
    for (file, snapshot) in inspected {
        let matches = match snapshot {
            ReadHiddenFile::Missing => continue,
            ReadHiddenFile::Other => false,
            ReadHiddenFile::Link(target) => {
                file.mode == "120000" && blob_matches(&target, &file.object)
            }
            ReadHiddenFile::Regular => {
                let path_option = format!("--path={}", file.name);
                let filter = if file.mode == "120000" {
                    "--no-filters"
                } else {
                    &path_option
                };
                let actual = git
                    .checked(OPERATION, &["hash-object", filter, "--", &file.name], None)
                    .await?;
                actual.stdout.trim() == file.object
            }
        };
        if !matches {
            return Ok(Some(file.name));
        }
    }
    Ok(None)
}

fn blob_matches(contents: &[u8], expected: &str) -> bool {
    let hash = match expected.len() {
        40 => &SHA1_FOR_LEGACY_USE_ONLY,
        64 => &SHA256,
        _ => return false,
    };
    let mut digest = Context::new(hash);
    digest.update(format!("blob {}\0", contents.len()).as_bytes());
    digest.update(contents);
    let digest = digest.finish();
    let mut encoded = String::with_capacity(expected.len());
    use std::fmt::Write;
    for byte in digest.as_ref() {
        write!(encoded, "{byte:02x}").expect("format Git object digest");
    }
    encoded == expected
}
