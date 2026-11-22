use std::path::{Component, PathBuf};

use rune::{El, FileDialogOptions, Key, Tx};
use uuid::Uuid;

use super::{AddProject, Palette, ProjectStep, State, button, folder, path_input};

const PICKER: &str = "project-open-native-folder";

fn is_browse_query(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with("~/")
        || folder::is_relative(path) && !matches!(path, "." | "..")
        || cfg!(windows) && path_input::is_windows_absolute(path)
}

pub(super) fn footer(form: &AddProject, palette: Palette, local_environment: bool) -> Option<El> {
    let query = form.path.text.trim_matches(path_input::whitespace);
    if !local_environment || !is_browse_query(query) {
        return None;
    }
    let label = match std::env::consts::OS {
        "macos" => "Open in Finder",
        "windows" => "Open in File Explorer",
        _ => "Open in Files",
    };
    Some(
        button(
            PICKER,
            label,
            palette,
            false,
            form.pending.is_none() && form.native_folder_picker.is_none(),
            open,
        )
        .label(label)
        .on_key(|state, event, tx| {
            if event.key == Key::Enter || matches!(&event.key, Key::Text(value) if value == " ") {
                open(state, tx);
                true
            } else {
                false
            }
        }),
    )
}

fn directory(form: &AddProject) -> Option<PathBuf> {
    let browse = form.browse.as_ref()?;
    let query = form.path.text.trim_matches(path_input::whitespace);
    let chosen = if folder::has_separator(query) {
        browse
            .listing
            .as_ref()
            .filter(|_| browse.requested_path == query)
            .map_or(query, |listing| listing.parent_path.as_str())
    } else {
        folder::browse_directory(query)
    };
    let resolved =
        folder::resolve(chosen, browse.cwd.as_deref()).unwrap_or_else(|| chosen.to_owned());
    let path = if resolved == "~" {
        std::env::home_dir()?
    } else if let Some(relative) = resolved
        .strip_prefix("~/")
        .or_else(|| resolved.strip_prefix("~\\"))
    {
        std::env::home_dir()?.join(relative)
    } else if cfg!(windows) && resolved.len() == 2 && path_input::is_windows_absolute(&resolved) {
        PathBuf::from(format!("{resolved}\\"))
    } else {
        PathBuf::from(resolved)
    };
    let absolute = std::path::absolute(path).ok()?;
    let mut directory = PathBuf::new();
    for component in absolute.components() {
        if component == Component::ParentDir {
            directory.pop();
        } else {
            directory.push(component);
        }
    }
    Some(directory)
}

fn open(state: &mut State, tx: &mut Tx) {
    let Some(form) = state.add_project.as_mut().filter(|form| {
        state.local_environment
            && matches!(form.step, ProjectStep::Folder)
            && form.browse.is_some()
            && is_browse_query(form.path.text.trim_matches(path_input::whitespace))
            && form.pending.is_none()
            && form.native_folder_picker.is_none()
    }) else {
        return;
    };
    let options = FileDialogOptions {
        title: "Select Folder".into(),
        directory: directory(form),
        ..Default::default()
    };
    let operation = Uuid::new_v4();
    form.native_folder_picker = Some(operation);
    tx.open_folder(options, move |state: &mut State, answer, tx| {
        let Some(form) = state
            .add_project
            .as_mut()
            .filter(|form| form.native_folder_picker == Some(operation))
        else {
            return;
        };
        form.native_folder_picker = None;
        let Ok(Some(path)) = answer else {
            return;
        };
        let Some(path) = path.to_str() else {
            form.error = Some("The selected folder path could not be read.".into());
            return;
        };
        form.path.replace_text(path);
        if let Some(browse) = form.browse.as_mut() {
            browse.listing = None;
        }
        state.submit_project(tx);
    });
}
