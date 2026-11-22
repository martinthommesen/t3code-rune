use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rune::{
    Color, El, FileDialogOptions, Icon, Key, KeyEvent, ScrollSpec, SurfaceFit, TextEdit, TextStyle,
    Tx, col, el, icon, row, text, text_input,
};
use uuid::Uuid;

use crate::connection::{FaviconTarget, ProjectEdit, ProjectEditKind, ProjectImages};
use crate::ui::{Palette, State, Workspace, settings::CONTROL, sidebar::project_badge};

use super::{PROJECT_FONT, find_project};

const DIALOG: &str = "project-icon-files";
const SEARCH: &str = "project-icon-files-search";
const LIST: &str = "project-icon-file-results";
const OPENER: &str = "choose-project-icon-file";
const DEBOUNCE: Duration = Duration::from_millis(120);
const KEY_FONT: TextStyle = TextStyle::new(11.0, 500.0).lh(16.0);

static SEARCH_ICON: Icon = Icon {
    name: "project-icon-file-search",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></g>"#,
};

pub(super) struct Picker {
    root: String,
    name: String,
    query: TextEdit,
    due: Option<Instant>,
    active: Option<Uuid>,
    loading: bool,
    error: Option<String>,
    results: Vec<FaviconTarget>,
    highlighted: usize,
    external_open: bool,
}

impl Picker {
    fn new(root: String, name: String) -> Self {
        Self {
            root,
            name,
            query: TextEdit::default(),
            due: None,
            active: None,
            loading: true,
            error: None,
            results: Vec::new(),
            highlighted: 0,
            external_open: false,
        }
    }
}

fn selected_picker(state: &mut State) -> Option<&mut Picker> {
    state.settings.project.as_mut()?.file_picker.as_mut()
}

impl State {
    fn open_project_icon_files(&mut self, tx: &mut Tx) {
        let (Some(workspace), Some(form)) = (&self.workspace, &mut self.settings.project) else {
            return;
        };
        if !workspace.project_writes
            || !workspace.filesystem_read
            || workspace.reconnecting
            || form.pending.contains(&ProjectEditKind::Icon)
        {
            return;
        }
        if let Some(project) = find_project(workspace, &form.project_id) {
            form.file_picker = Some(Picker::new(project.path.clone(), project.name.clone()));
            self.search_project_icon_files();
            tx.focus_when_shown(SEARCH, false);
        }
    }

    fn close_project_icon_files(&mut self, tx: &mut Tx) {
        if let Some(form) = &mut self.settings.project {
            form.file_picker = None;
        }
        if let (Some(workspace), Some(connection)) = (&mut self.workspace, &self.connection) {
            workspace.favicons.previews(Vec::new(), connection);
        }
        tx.focus(OPENER, tx.focus_visible());
    }

    fn search_project_icon_files(&mut self) {
        let Some(picker) = selected_picker(self) else {
            return;
        };
        let uid = Uuid::new_v4();
        let root = picker.root.clone();
        let mut query = String::new();
        let mut width = 0;
        for character in picker.query.text.trim().chars() {
            width += character.len_utf16();
            if width > 256 {
                break;
            }
            query.push(character);
        }
        picker.due = None;
        picker.active = Some(uid);
        if let Some(connection) = &self.connection {
            connection.search_project_images(uid, root, query);
        }
    }

    fn save_project_icon_file(&mut self, file: String, tx: &mut Tx) {
        self.edit_project_setting(ProjectEdit::IconFile(file));
        self.close_project_icon_files(tx);
    }

    fn browse_external_icon_file(&mut self, tx: &mut Tx) {
        if !self.local_environment {
            return;
        }
        let Some(picker) = selected_picker(self).filter(|picker| !picker.external_open) else {
            return;
        };
        picker.external_open = true;
        let root = picker.root.clone();
        let options = FileDialogOptions {
            title: "Open".into(),
            directory: Some(PathBuf::from(&root)),
            ..Default::default()
        }
        .filter("Images", t3_rune::project_icon::FILE_EXTENSIONS);
        tx.open_file(options, move |state: &mut State, answer, tx| {
            let Some(picker) = selected_picker(state).filter(|picker| picker.root == root) else {
                return;
            };
            picker.external_open = false;
            match answer {
                Ok(Some(path)) => {
                    if let Some(path) = path.to_str() {
                        state.save_project_icon_file(path.to_owned(), tx);
                    } else {
                        picker.error = Some("The selected image path could not be read.".into());
                    }
                }
                Ok(None) => tx.focus(SEARCH, false),
                Err(error) => picker.error = Some(error.to_string()),
            }
        });
    }
}

pub(in crate::ui) fn poll(state: &mut State, tx: &mut Tx) -> bool {
    let Some(due) = selected_picker(state).and_then(|picker| picker.due) else {
        return false;
    };
    if let Some(delay) = due.checked_duration_since(Instant::now()) {
        tx.wake_after(delay);
        return false;
    }
    state.search_project_icon_files();
    true
}

pub(in crate::ui) fn settle(
    state: &mut State,
    search_id: Uuid,
    result: Result<ProjectImages, String>,
) {
    let Some(picker) = selected_picker(state).filter(|picker| picker.active == Some(search_id))
    else {
        return;
    };
    picker.active = None;
    picker.loading = false;
    picker.highlighted = 0;
    picker.results = match result {
        Ok(result) => result
            .entries
            .into_iter()
            .filter(|entry| entry.is_file())
            .filter_map(|entry| {
                let path = entry.path;
                (!path.is_empty()).then(|| FaviconTarget {
                    cwd: picker.root.clone(),
                    path: Some(path),
                })
            })
            .collect(),
        Err(error) => {
            picker.error = Some(error);
            Vec::new()
        }
    };
    let previews = picker.results.clone();
    if let (Some(workspace), Some(connection)) = (&mut state.workspace, &state.connection) {
        workspace.favicons.previews(previews, connection);
    }
}

pub(super) fn opener(palette: Palette, enabled: bool) -> El {
    el().key(OPENER)
        .role("button")
        .label("Choose a project icon file")
        .focusable(enabled)
        .disabled(!enabled)
        .focus_ring(1.0, 1.0, palette.accent)
        .size(95.73, 28.0)
        .radius(6.0)
        .center()
        .border(1.0, palette.line)
        .hover(palette.hover)
        .on_press_tx(State::open_project_icon_files)
        .child(text("Choose file").font(CONTROL).color(palette.ink))
}

pub(in crate::ui) fn overlay(
    state: &State,
    palette: Palette,
    width: f32,
    height: f32,
) -> Option<El> {
    let workspace = state.workspace.as_ref()?;
    let picker = state.settings.project.as_ref()?.file_picker.as_ref()?;
    let width = (width - 24.0).clamp(280.0, 576.0);
    let visible_height = (height * 0.70 - 95.0).clamp(96.0, 360.0);
    let search = el().w_full().h(49.0).px(9.0).pt(7.0).child(
        el().w_full()
            .h(34.0)
            .child(
                text_input(&picker.query, PROJECT_FONT)
                    .key(SEARCH)
                    .role("searchbox")
                    .label("Choose project icon")
                    .placeholder("Search image files…", palette.muted)
                    .color(palette.ink)
                    .single_line()
                    .w_full()
                    .h(34.0)
                    .pl(24.0)
                    .on_edit(|state: &mut State| {
                        &mut selected_picker(state).expect("active image chooser").query
                    })
                    .on_input_change(queue_search)
                    .on_input_key(navigate)
                    .on_submit(select_highlighted),
            )
            .child(
                el().absolute(0.0, 9.0)
                    .size(16.0, 16.0)
                    .child(icon(&SEARCH_ICON, 16.0).color(palette.muted).a11y_hidden()),
            ),
    );
    let contents = if picker.results.is_empty() {
        let message = if let Some(error) = &picker.error {
            error
        } else if picker.loading {
            if picker.query.text.trim().is_empty() {
                "Indexing project files…"
            } else {
                "Searching project files…"
            }
        } else if picker.query.text.trim().is_empty() {
            "No image files found."
        } else {
            "No matching image files."
        };
        el().w_full()
            .h(96.0)
            .center()
            .child(text(message).font(PROJECT_FONT).color(palette.muted).wrap())
    } else {
        results(workspace, picker, palette, visible_height)
    };
    let footer = footer(state, picker, palette);
    let popup = col()
        .key(DIALOG)
        .role("dialog")
        .label("Choose project icon")
        .w(width)
        .min_h(126.0)
        .radius(12.0)
        .clip_rounded(12.0)
        .fill(palette.canvas)
        .border(1.0, palette.line)
        .border_layout()
        .hit()
        .child(search)
        .child(contents)
        .child(footer)
        .on_key(|state: &mut State, event, tx| {
            if event.key != Key::Escape {
                return false;
            }
            state.close_project_icon_files(tx);
            true
        });
    Some(
        col()
            .key("project-icon-files-overlay")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .items_center()
            .pt(height * 0.10)
            .child(
                el().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                    .fill(Color::BLACK.alpha(0.54))
                    .on_press_tx(State::close_project_icon_files),
            )
            .child(popup),
    )
}

fn queue_search(state: &mut State, tx: &mut Tx) {
    if let Some(picker) = selected_picker(state) {
        picker.active = None;
        picker.error = None;
        picker.loading = true;
        picker.due = Some(Instant::now() + DEBOUNCE);
        picker.highlighted = 0;
        picker.results.clear();
        tx.wake_after(DEBOUNCE);
    }
}

fn select_highlighted(state: &mut State, tx: &mut Tx) {
    let path = selected_picker(state)
        .and_then(|picker| picker.results.get(picker.highlighted)?.path.clone());
    if let Some(path) = path {
        state.save_project_icon_file(path, tx);
    }
}

fn navigate(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if event.key == Key::Escape {
        state.close_project_icon_files(tx);
        return true;
    }
    if !matches!(event.key, Key::Up | Key::Down)
        || event.mods.ctrl
        || event.mods.alt
        || event.mods.cmd
    {
        return false;
    }
    if let Some(picker) = selected_picker(state).filter(|picker| !picker.results.is_empty()) {
        picker.highlighted = if event.key == Key::Up {
            picker.highlighted.saturating_sub(1)
        } else {
            (picker.highlighted + 1).min(picker.results.len() - 1)
        };
        tx.scroll_into_view_in(LIST, result_key(picker.highlighted));
    }
    true
}

fn result_key(index: usize) -> String {
    format!("project-icon-file-{index}")
}

fn results(workspace: &Workspace, picker: &Picker, palette: Palette, height: f32) -> El {
    let mut list = col()
        .key(LIST)
        .role("listbox")
        .label("Project image files")
        .w_full()
        .max_h(height)
        .min_h(96.0)
        .px(8.0)
        .pb(8.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            row().w_full().h(26.0).px(8.0).items_center().child(
                text(&picker.name)
                    .font(KEY_FONT)
                    .color(palette.muted)
                    .truncate(),
            ),
        );
    for (index, target) in picker.results.iter().enumerate() {
        let Some(path) = target.path.as_deref() else {
            continue;
        };
        let name = path.rsplit('/').next().unwrap_or(path);
        let owned = path.to_owned();
        let selected = picker.highlighted == index;
        let preview = if let Some(image) = workspace.favicons.preview(target) {
            el().size(18.0, 18.0)
                .radius(3.0)
                .pixels_fit(image.pixels(), SurfaceFit::Fit)
        } else if let Some(glyph) = project_badge::lookup_icon("Image") {
            icon(glyph, 18.0).color(palette.muted)
        } else {
            el().size(18.0, 18.0)
        };
        list = list.child(
            row()
                .key(result_key(index))
                .role("option")
                .label(path)
                .focus_selected(selected)
                .w_full()
                .h(48.0)
                .gap(10.0)
                .px(8.0)
                .items_center()
                .radius(6.0)
                .fill(if selected {
                    palette.hover
                } else {
                    Color::TRANSPARENT
                })
                .on_hover(move |state: &mut State| {
                    if let Some(picker) = selected_picker(state) {
                        picker.highlighted = index
                    }
                })
                .on_press_tx(move |state: &mut State, tx| {
                    state.save_project_icon_file(owned.clone(), tx)
                })
                .child(preview.a11y_hidden())
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .child(text(name).font(CONTROL).color(palette.ink).truncate())
                        .child(text(path).font(KEY_FONT).color(palette.muted).truncate()),
                ),
        );
    }
    list
}

fn footer(state: &State, picker: &Picker, palette: Palette) -> El {
    let mut footer = row()
        .w_full()
        .h(40.0)
        .gap(10.0)
        .px(12.0)
        .items_center()
        .border_sides([1.0, 0.0, 0.0, 0.0], palette.line)
        .child(hint("↑ ↓", "Navigate", palette))
        .child(hint("Enter", "Select icon", palette))
        .child(hint("Esc", "Close", palette));
    if state.local_environment && (!cfg!(windows) || Path::new(&picker.root).is_absolute()) {
        let file_manager = match std::env::consts::OS {
            "macos" => "Open in Finder",
            "windows" => "Open in File Explorer",
            _ => "Open in Files",
        };
        footer = footer.child(el().grow(1.0)).child(
            el().key("project-icon-file-external")
                .role("button")
                .label(file_manager)
                .focusable(!picker.external_open)
                .disabled(picker.external_open)
                .focus_ring(1.0, 1.0, palette.accent)
                .radius(6.0)
                .h(26.0)
                .px(8.0)
                .center()
                .border(1.0, palette.line)
                .hover(palette.hover)
                .on_press_tx(State::browse_external_icon_file)
                .child(text(file_manager).font(KEY_FONT).color(palette.ink)),
        );
    }
    footer
}

fn hint(key: &str, label: &str, palette: Palette) -> El {
    row()
        .role("group")
        .label(label)
        .gap(4.0)
        .items_center()
        .child(
            el().h(16.0)
                .px(3.0)
                .radius(3.0)
                .center()
                .fill(palette.hover)
                .child(text(key).font(KEY_FONT).color(palette.muted)),
        )
        .child(text(label).font(KEY_FONT).color(palette.muted))
}
