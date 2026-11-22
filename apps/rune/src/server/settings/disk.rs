use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::Fields;

pub(super) fn read(path: &Path) -> io::Result<Fields> {
    match fs::read_to_string(path) {
        Ok(contents) => match super::jsonc::parse(&contents) {
            Some(Value::Object(fields)) => Ok(fields),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid settings file.",
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Fields::new()),
        Err(error) => Err(error),
    }
}

pub(super) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = resolve_target(path)?;
    let parent = target
        .parent()
        .expect("settings file has a parent directory");
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        match fs::metadata(&target) {
            Ok(metadata) => temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(
                    metadata.permissions().mode() & 0o777,
                ))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
    }
    temporary.as_file().sync_all()?;
    temporary.persist(&target).map_err(|error| error.error)?;
    #[cfg(unix)]
    match fs::File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => (),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::Unsupported
            ) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn resolve_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_owned();
    for _ in 0..=40 {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&current)?;
                let directory =
                    fs::canonicalize(current.parent().expect("settings link has a parent"))?;
                current = lexical_path(&directory.join(target));
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "Too many levels of symbolic links.",
    ))
}

fn lexical_path(path: &Path) -> PathBuf {
    let mut value = PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir {
            value.pop();
        } else {
            value.push(component);
        }
    }
    value
}
