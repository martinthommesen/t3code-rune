use std::path::Path;

pub(super) fn protects(state_dir: &Path, path: &Path) -> bool {
    let secrets = state_dir.join("secrets");
    let secrets = secrets.canonicalize().unwrap_or(secrets);
    let state = state_dir.canonicalize();
    path.starts_with(secrets)
        || (path.parent() == Some(state.as_deref().unwrap_or(state_dir))
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".sqlite")))
        || path.starts_with("/dev/fd")
}
