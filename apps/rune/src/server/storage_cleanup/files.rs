use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::server::settings::SettingsFile;

use super::{Entry, Kind, Outcome};

const SECONDS_PER_DAY: u64 = 86_400;

pub(in crate::server) fn sweep(settings: &SettingsFile, now: SystemTime) -> io::Result<Vec<Entry>> {
    let snapshot = settings.read().map_err(io::Error::other)?.for_client();
    let state = settings
        .path()
        .parent()
        .ok_or_else(|| io::Error::other("Missing storage directory"))?;
    let mut entries = Vec::new();
    for (kind, directory, key, label) in [
        (
            Kind::BrowserArtifacts,
            "browser-artifacts",
            "browserArtifactsAfterDays",
            "browser artifacts",
        ),
        (Kind::Logs, "logs", "logsAfterDays", "rotated logs"),
    ] {
        let Some(days) = snapshot["storageCleanup"][key].as_u64() else {
            continue;
        };
        let Some(cutoff) =
            now.checked_sub(Duration::from_secs(days.saturating_mul(SECONDS_PER_DAY)))
        else {
            continue;
        };
        let mut removed = Removed::default();
        let root = state.join(directory);
        let removed_result = clean_files(&root, cutoff, kind == Kind::Logs, &mut removed, || {
            let current = settings.read().map_err(io::Error::other)?.for_client();
            Ok(current["storageCleanup"][key].as_u64() == Some(days))
        });
        let (outcome, reason) = match removed_result {
            Ok(()) if removed.files == 0 => (Outcome::Kept, "No expired files".to_owned()),
            Ok(()) => (
                Outcome::Removed,
                format!("Removed {} {label}", removed.files),
            ),
            Err(error) => (Outcome::Failed, error.to_string()),
        };
        entries.push(Entry {
            kind,
            outcome,
            reason,
            path: None,
            project_path: None,
            thread_id: None,
            thread_title: None,
            bytes: (removed.files > 0).then_some(removed.bytes),
            files: (removed.files > 0).then_some(removed.files),
        });
    }
    Ok(entries)
}

#[derive(Default)]
struct Removed {
    files: u64,
    bytes: u64,
}

fn clean_files(
    root: &Path,
    cutoff: SystemTime,
    rotated_logs: bool,
    removed: &mut Removed,
    still_configured: impl Fn() -> io::Result<bool>,
) -> io::Result<()> {
    let physical = match root.canonicalize() {
        Ok(path) => path,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let expected = root
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .zip(root.file_name())
        .map(|(parent, name)| parent.join(name));
    if expected.as_deref() != Some(&physical) || !physical.is_dir() {
        return Ok(());
    }
    let mut folders = vec![physical.clone()];
    while let Some(folder) = folders.pop() {
        for item in fs::read_dir(folder)? {
            let path = item?.path();
            let metadata = path.symlink_metadata()?;
            if metadata.file_type().is_symlink() || path.canonicalize()? != path {
                continue;
            }
            if rotated_logs && metadata.is_dir() {
                folders.push(path);
                continue;
            }
            if !metadata.is_file()
                || rotated_logs && !is_rotated_log(&path)
                || !metadata.modified().is_ok_and(|modified| modified < cutoff)
            {
                continue;
            }
            if !still_configured()? {
                return Ok(());
            }
            fs::remove_file(&path)?;
            removed.files += 1;
            removed.bytes = removed.bytes.saturating_add(metadata.len());
        }
    }
    Ok(())
}

fn is_rotated_log(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((stem, generation)) = name.rsplit_once('.') else {
        return false;
    };
    (stem.ends_with(".log") || stem.ends_with(".ndjson"))
        && !generation.is_empty()
        && generation.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_rotated_logs_and_top_level_artifacts_follow_separate_retention_rules() {
        let profile = tempfile::tempdir().unwrap();
        let settings = profile.path().join("settings.json");
        fs::write(
            &settings,
            r#"{"storageCleanup":{"logsAfterDays":2,"browserArtifactsAfterDays":2}}"#,
        )
        .unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let now = old + Duration::from_secs(SECONDS_PER_DAY * 3);
        for relative in [
            "logs/providers/events.log.2",
            "logs/providers/events.log",
            "logs/providers/events.log.lock",
            "logs/stream.ndjson.2",
            "browser-artifacts/capture.png",
            "browser-artifacts/nested/capture.png",
        ] {
            let path = profile.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let file = fs::File::create(&path).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let entries = sweep(&SettingsFile::new(settings), now).unwrap();
        assert_eq!(entries[0].files, Some(1));
        assert_eq!(entries[1].files, Some(2));
        assert!(!profile.path().join("logs/providers/events.log.2").exists());
        assert!(profile.path().join("logs/providers/events.log").exists());
        assert!(
            profile
                .path()
                .join("logs/providers/events.log.lock")
                .exists()
        );
        assert!(
            profile
                .path()
                .join("browser-artifacts/nested/capture.png")
                .exists()
        );
    }
}
