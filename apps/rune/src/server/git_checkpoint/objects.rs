use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

pub(super) struct Scratch {
    index: PathBuf,
    objects: PathBuf,
    shared: PathBuf,
}

impl Scratch {
    pub(super) fn new(common: &Path) -> io::Result<Self> {
        let shared = common.join("objects");
        let objects = shared.join(format!("tmp_objdir-t3-checkpoint-{}", Uuid::new_v4()));
        fs::create_dir(&objects)?;
        Ok(Self {
            index: objects.join("index"),
            objects,
            shared,
        })
    }

    pub(super) fn isolated(directory: &Path, common: &Path) -> io::Result<Self> {
        let objects = directory.join("objects");
        fs::create_dir(&objects)?;
        Ok(Self {
            index: directory.join("index"),
            objects,
            shared: common.join("objects"),
        })
    }

    pub(super) fn index(&self) -> &Path {
        &self.index
    }

    pub(super) fn objects(&self) -> &Path {
        &self.objects
    }

    pub(super) fn shared_objects(&self) -> &Path {
        &self.shared
    }

    pub(super) fn publish(&self) -> io::Result<()> {
        for directory in fs::read_dir(&self.objects)? {
            let directory = directory?;
            if !directory.file_type()?.is_dir() {
                continue;
            }
            let name = directory.file_name();
            let Some(name) = name.to_str() else { continue };
            if name != "pack" && (name.len() != 2 || !is_object_hex(name)) {
                continue;
            }

            let source = directory.path();
            let destination = self.shared.join(name);
            ensure_directory(&source, &destination)?;
            if name == "pack" {
                publish_packs(&source, &destination)?;
            } else {
                for entry in fs::read_dir(&source)? {
                    let entry = entry?;
                    let file = entry.file_name();
                    if entry.file_type()?.is_file()
                        && file.to_str().is_some_and(|name| {
                            matches!(name.len(), 38 | 62) && is_object_hex(name)
                        })
                    {
                        publish_file(&entry.path(), &destination.join(file))?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.objects);
    }
}

fn is_object_hex(name: &str) -> bool {
    name.bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn ensure_directory(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::create_dir(destination) {
        Ok(()) => fs::set_permissions(destination, fs::metadata(source)?.permissions()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && destination.is_dir() => {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn publish_file(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported | io::ErrorKind::Other
            ) =>
        {
            if destination.exists() {
                return Ok(());
            }
            match fs::rename(source, destination) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                result => result,
            }
        }
        Err(error) => Err(error),
    }
}

fn publish_packs(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let filename = entry.file_name();
        let Some(stem) = filename
            .to_str()
            .and_then(|name| name.strip_suffix(".pack"))
        else {
            continue;
        };
        let Some(oid) = stem.strip_prefix("pack-") else {
            continue;
        };
        if !matches!(oid.len(), 40 | 64) || !is_object_hex(oid) || !entry.file_type()?.is_file() {
            continue;
        }
        if !source.join(format!("{stem}.idx")).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Missing checkpoint pack index",
            ));
        }
        for extension in ["keep", "promisor", "rev", "idx", "pack"] {
            let name = format!("{stem}.{extension}");
            let from = source.join(&name);
            if from.is_file() {
                publish_file(&from, &destination.join(name))?;
            }
        }
    }
    Ok(())
}
