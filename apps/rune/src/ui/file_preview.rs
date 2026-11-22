mod binary;
mod image;

pub(super) use image::expanded_is_open;

use std::borrow::Cow;
use std::sync::Arc;

use rune::color::hex;
use rune::{
    Align, Color, El, Family, GrowthSpec, Icon, Key, ScrollSpec, TextStyle, Tx, col, el, icon, row,
    text,
};
use uuid::Uuid;

use crate::connection::file_preview::{AssetPurpose, Contents, ReadFailure, ReadRequest};

use super::conversation::{self, Layout};
use super::{Palette, ProjectView, State, Workspace};

const TAB_CLOSE: &str = "close-file-preview";
const SCROLLER: &str = "file-preview-source";
const LINE_HEIGHT: f32 = 20.0;
const SOURCE_FONT: Family = if cfg!(target_os = "linux") {
    Family::Named("Liberation Mono, DejaVu Sans Mono")
} else {
    Family::SystemMono
};
const MONO: TextStyle = TextStyle::new(13.0, 400.0)
    .lh(LINE_HEIGHT)
    .family(SOURCE_FONT);
const CRUMB: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const TAB: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);
const NOTICE: TextStyle = TextStyle::new(11.0, 400.0).lh(16.5);
const ERROR: TextStyle = TextStyle::new(12.0, 400.0).lh(19.5);

macro_rules! glyph {
    ($name:literal, $paths:literal) => {
        Icon {
            name: $name,
            view: 24.0,
            body: concat!(
                r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#,
                $paths,
                "</g>"
            ),
        }
    };
}

static FILE: Icon = glyph!(
    "file-text",
    r#"<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8Z"/><path d="M14 2v6h6M8 13h8M8 17h8"/>"#
);
static CLOSE: Icon = glyph!("x", r#"<path d="m18 6-12 12M6 6l12 12"/>"#);
static CHEVRON: Icon = glyph!("chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#);
static WRAP: Icon = glyph!(
    "wrap-text",
    r#"<path d="M3 6h18M3 12h15a3 3 0 1 1 0 6h-4M3 18h5m9-3-3 3 3 3"/>"#
);
static COPY: Icon = glyph!(
    "copy",
    r#"<rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2"/>"#
);

struct Target {
    path: String,
    line: Option<usize>,
}

fn workspace_target(href: &str) -> Option<Target> {
    let (raw_path, line) = match href.trim().split_once('#') {
        Some((path, anchor)) => {
            let line: usize = anchor.strip_prefix('L')?.parse().ok()?;
            if line == 0 {
                return None;
            }
            (path, Some(line))
        }
        None => (href.trim(), None),
    };
    let path = percent_decode(raw_path)?.replace('\\', "/");
    if path.starts_with(['/', '~'])
        || path.ends_with('/')
        || path
            .chars()
            .any(|c| c.is_control() || matches!(c, ':' | '?' | '#'))
        || path.encode_utf16().count() > 512
        || path.split('/').any(|part| part == "..")
    {
        return None;
    }
    let path = path
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect::<Vec<_>>()
        .join("/");
    if !(path.contains('/') || path.contains('.')) {
        return None;
    }
    Some(Target { path, line })
}

fn percent_decode(path: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).ok()
}

struct TextFile {
    data: Contents,
    line_starts: Vec<usize>,
}

impl TextFile {
    fn new(contents: Contents) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            contents
                .contents
                .match_indices('\n')
                .map(|(index, _)| index + 1),
        );
        Self {
            data: contents,
            line_starts,
        }
    }

    fn line(&self, index: usize) -> &str {
        let start = self.line_starts[index];
        let end = self
            .line_starts
            .get(index + 1)
            .map_or(self.data.contents.len(), |next| next - 1);
        let line = &self.data.contents[start..end];
        line.strip_suffix('\r').unwrap_or(line)
    }
}

enum Load {
    Pending,
    Text(Arc<TextFile>),
    Binary(binary::Phase),
    Image(image::PreviewImage),
    Failed(ReadFailure),
}

pub(super) struct Preview {
    id: Uuid,
    thread_id: String,
    project: String,
    cwd: String,
    target: Target,
    origin: String,
    href: String,
    wrap: bool,
    load: Load,
}

pub(super) fn activate_link(
    state: &mut State,
    thread_id: &str,
    origin: &str,
    href: &str,
    tx: &mut Tx,
) {
    let Some(target) = workspace_target(href) else {
        let _ = tx.open_url(href);
        return;
    };
    let Some(workspace) = state
        .workspace
        .as_ref()
        .filter(|workspace| workspace.filesystem_read && !workspace.reconnecting)
    else {
        return;
    };
    let Some(selection) = workspace.selection.as_ref().filter(|selection| {
        matches!(&selection.view, ProjectView::Conversation(conversation) if conversation.id == thread_id)
    }) else {
        return;
    };
    let Some((thread, project, connection)) = workspace
        .thread(thread_id)
        .zip(workspace.shell.as_ref().and_then(|shell| {
            shell
                .projects
                .iter()
                .find(|project| project.id == selection.id)
        }))
        .zip(state.connection.as_ref())
        .map(|((thread, project), connection)| (thread, project, connection))
    else {
        return;
    };
    let cwd = thread
        .worktree_path
        .as_ref()
        .unwrap_or(&project.path)
        .clone();
    let id = Uuid::new_v4();
    let request = ReadRequest {
        id,
        thread_id: thread_id.to_owned(),
        cwd: cwd.clone(),
        relative_path: target.path.clone(),
    };
    let load = if image::supports(&target.path) {
        connection.prepare_file_asset(request, AssetPurpose::ShowImage);
        Load::Image(image::PreviewImage::new())
    } else {
        connection.read_file_preview(request);
        Load::Pending
    };
    state.file_preview = Some(Preview {
        id,
        thread_id: thread_id.to_owned(),
        project: project.name.clone(),
        cwd,
        target,
        origin: origin.to_owned(),
        href: href.to_owned(),
        wrap: true,
        load,
    });
    tx.focus_when_shown(TAB_CLOSE, false);
}

impl State {
    pub(super) fn file_preview_loaded(
        &mut self,
        id: Uuid,
        result: Result<Contents, ReadFailure>,
        tx: &mut Tx,
    ) {
        let Some(file) = self.file_preview.as_mut().filter(|file| file.id == id) else {
            return;
        };
        file.load = match result {
            Ok(contents) => {
                let contents = Arc::new(TextFile::new(contents));
                if let Some(line) = file
                    .target
                    .line
                    .filter(|line| *line <= contents.line_starts.len())
                {
                    tx.scroll_center_in_instant(SCROLLER, row_key(line - 1));
                }
                Load::Text(contents)
            }
            Err(error) if error.binary => {
                let Some(connection) = &self.connection else {
                    return;
                };
                connection.prepare_file_asset(
                    file.request(),
                    crate::connection::file_preview::AssetPurpose::Inspect,
                );
                Load::Binary(binary::Phase::Checking)
            }
            Err(error) => Load::Failed(error),
        };
    }

    fn close_file_preview(&mut self, tx: &mut Tx) {
        let Some(preview) = self.file_preview.take() else {
            return;
        };
        if let Some(connection) = &self.connection {
            connection.cancel_file_preview(preview.id);
        }
        tx.focus_link(preview.origin, preview.href, tx.by_keys());
    }
}

pub(super) fn dock_width(window: f32, workspace: f32) -> f32 {
    (window * 0.7)
        .floor()
        .min(workspace.floor() - 360.0)
        .clamp(360.0, 540.0)
        .min((workspace - 120.0).max(0.0))
}

pub(super) fn expanded(state: &State, layout: Layout, palette: Palette) -> Option<El> {
    image::expanded::view(state, layout, palette)
}

#[derive(Clone, Copy)]
struct Colors {
    background: Color,
    code: Color,
    ink: Color,
    source_ink: Color,
    muted: Color,
    accent: Color,
    separator: Color,
    error: Color,
}

impl Colors {
    fn new(dark: bool) -> Self {
        let [
            background,
            code,
            ink,
            source_ink,
            muted,
            accent,
            separator,
            error,
        ] = if dark {
            [
                "#000000", "#101010", "#f5f5f5", "#f5f5f5", "#a3a3a3", "#0a0a0a", "#181818",
                "#fb414a",
            ]
        } else {
            [
                "#fcfcfc", "#ffffff", "#18181b", "#18181b", "#71717b", "#f4f4f5", "#e4e4e7",
                "#fb2c36",
            ]
        }
        .map(|value| hex(value).expect("static file preview color"));
        Self {
            background,
            code,
            ink,
            source_ink,
            muted,
            accent,
            separator,
            error,
        }
    }
}

impl Preview {
    fn request(&self) -> ReadRequest {
        ReadRequest {
            id: self.id,
            thread_id: self.thread_id.clone(),
            cwd: self.cwd.clone(),
            relative_path: self.target.path.clone(),
        }
    }

    fn save_phase(&mut self) -> Option<&mut binary::Phase> {
        match &mut self.load {
            Load::Binary(phase) => Some(phase),
            Load::Image(image) => Some(&mut image.save),
            _ => None,
        }
    }
}

pub(super) fn view(
    state: &State,
    workspace: &Workspace,
    width: f32,
    layout: Layout,
    palette: Palette,
) -> Option<El> {
    let preview = state.file_preview.as_ref()?;
    let thread = workspace.thread(&preview.thread_id)?;
    let colors = Colors::new(palette.dark);
    let name = filename(&preview.target.path);
    let mut tabs = state
        .window_chrome
        .drag_region(
            row()
                .key("file-preview-tabbar")
                .w_full()
                .h(layout.header_height)
                .pl(8.0)
                .pr(layout.native_controls_inset() + 32.0)
                .center_y(),
        )
        .child(
            row()
                .role("tab")
                .label(name)
                .checked(true)
                .h(24.0)
                .max_w(224.0)
                .px(6.0)
                .gap(4.0)
                .center_y()
                .radius(7.0)
                .fill(colors.accent)
                .hover_group_focus()
                .child(
                    el().key(TAB_CLOSE)
                        .role("button")
                        .label(format!("Close {name}"))
                        .size(16.0, 16.0)
                        .min_w(16.0)
                        .center()
                        .focusable(true)
                        .focus_ring(2.0, 0.0, palette.accent)
                        .on_press_tx(State::close_file_preview)
                        .on_key(|state: &mut State, event, tx| {
                            if event.key != Key::Enter {
                                return false;
                            }
                            if !tx.key_repeat() {
                                state.close_file_preview(tx);
                            }
                            true
                        })
                        .child(
                            icon(&FILE, 12.0)
                                .color(colors.muted)
                                .group_opacity(1.0, 0.0)
                                .a11y_hidden(),
                        )
                        .child(
                            icon(&CLOSE, 13.0)
                                .color(colors.ink)
                                .absolute(1.5, 1.5)
                                .group_opacity(0.0, 1.0)
                                .a11y_hidden(),
                        ),
                )
                .child(text(name).font(TAB).color(colors.ink).truncate()),
        )
        .child(el().grow(1.0));
    if workspace.terminal && !workspace.reconnecting {
        tabs = tabs.child(conversation::terminal_control(
            state,
            thread,
            conversation::Colors::new(palette),
        ));
    }
    let mut pane = col()
        .key("file-preview-dock")
        .role("region")
        .label("Workspace panel")
        .w(width)
        .h_full()
        .min_h(0.0)
        .border_sides([0.0, 0.0, 0.0, 1.0], colors.separator)
        .border_layout()
        .fill(colors.background)
        .child(tabs)
        .child(toolbar(preview, colors, palette))
        .child(el().h(12.0));
    if !workspace.filesystem_write {
        pane = pane.child(
            row()
                .key("file-preview-read-only")
                .w_full()
                .h(29.0)
                .px(12.0)
                .center_y()
                .border_sides([0.0, 0.0, 1.0, 0.0], colors.separator)
                .border_layout()
                .child(
                    text(
                        "Read-only connection. Unsaved edits are kept until write access returns.",
                    )
                    .font(NOTICE)
                    .color(colors.muted)
                    .truncate(),
                ),
        );
    }
    let body = match &preview.load {
        Load::Pending => col().w_full().min_h(0.0).grow(1.0).center().child(
            text("Loading…")
                .font(CRUMB)
                .color(colors.muted)
                .role("status"),
        ),
        Load::Text(file) => source(preview, file, colors, palette),
        Load::Binary(phase) => binary::view(preview, phase, colors, palette),
        Load::Image(image) => image::view(
            preview,
            image,
            image::Bounds::new(width, layout, workspace.filesystem_write),
            colors,
            palette,
        ),
        Load::Failed(error) => error_view(preview, error, colors),
    };
    Some(pane.child(body))
}

fn filename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn toolbar(preview: &Preview, colors: Colors, palette: Palette) -> El {
    let mut breadcrumbs = row()
        .key("file-preview-breadcrumbs")
        .role("navigation")
        .label("File path")
        .grow(1.0)
        .min_w(0.0)
        .center_y()
        .gap(7.0)
        .child(
            text(&preview.project)
                .font(CRUMB)
                .color(colors.muted)
                .truncate(),
        );
    let segments: Vec<_> = preview.target.path.split('/').collect();
    let count = segments.len();
    for (index, segment) in segments.into_iter().enumerate() {
        let is_file = index + 1 == count;
        breadcrumbs = breadcrumbs
            .child(
                icon(&CHEVRON, 10.0)
                    .color(colors.muted.alpha(0.65))
                    .a11y_hidden(),
            )
            .child(
                text(segment)
                    .font(if is_file { TAB } else { CRUMB })
                    .color(if is_file { colors.ink } else { colors.muted })
                    .truncate(),
            );
    }
    let mut toolbar = row()
        .key("file-preview-toolbar")
        .w_full()
        .h(28.0)
        .pl(12.0)
        .pr(48.0)
        .center_y()
        .gap(8.0)
        .child(breadcrumbs);
    if let Load::Text(file) = &preview.load {
        toolbar = toolbar.child(
            control(
                "file-preview-wrap",
                if preview.wrap {
                    "Disable word wrap"
                } else {
                    "Enable word wrap"
                },
                &WRAP,
                colors,
                palette,
                |state, _| {
                    if let Some(file) = state.file_preview.as_mut() {
                        file.wrap = !file.wrap;
                    }
                },
            )
            .checked(preview.wrap)
            .fill(if preview.wrap {
                colors.accent
            } else {
                Color::TRANSPARENT
            }),
        );
        if !file.data.truncated && !file.data.contents.is_empty() {
            toolbar = toolbar.child(control(
                "file-preview-copy",
                "Copy contents",
                &COPY,
                colors,
                palette,
                |state, tx| {
                    if let Some(Preview {
                        load: Load::Text(file),
                        ..
                    }) = &state.file_preview
                    {
                        tx.copy(file.data.contents.clone());
                    }
                },
            ));
        }
    }
    toolbar
}

fn control(
    key: &'static str,
    label: &'static str,
    glyph: &'static Icon,
    colors: Colors,
    palette: Palette,
    action: impl Fn(&mut State, &mut Tx) + Copy + 'static,
) -> El {
    el().key(key)
        .role("button")
        .label(label)
        .help(label)
        .size(28.0, 28.0)
        .radius(8.0)
        .center()
        .hover(colors.accent)
        .focusable(true)
        .focus_ring(2.0, -1.0, palette.accent)
        .on_press_tx(action)
        .on_key(move |state: &mut State, event, tx| {
            if event.key != Key::Enter {
                return false;
            }
            if !tx.key_repeat() {
                action(state, tx);
            }
            true
        })
        .child(icon(glyph, 14.0).color(colors.muted).a11y_hidden())
}

fn row_key(index: usize) -> String {
    format!("file-line-{index}")
}

fn source(preview: &Preview, file: &Arc<TextFile>, colors: Colors, palette: Palette) -> El {
    let count = file.line_starts.len();
    let gutter = 25.402 + 7.8013 * count.to_string().len() as f32;
    let wrap = preview.wrap;
    let selected = preview.target.line;
    let source_text = Arc::clone(file);
    let rows = col()
        .key("file-preview-lines")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(
            count,
            LINE_HEIGHT,
            |_: &State, index| Cow::Owned(row_key(index)),
            move |_: &State, index| {
                let line = source_text.line(index);
                let mut content = text(if line.is_empty() { " " } else { line })
                    .font(MONO)
                    .color(colors.source_ink)
                    .min_w(0.0)
                    .grow(1.0)
                    .selectable(true)
                    .selection_color(palette.selected);
                if wrap {
                    content = content.wrap_anywhere();
                }
                row()
                    .w_full()
                    .min_h(LINE_HEIGHT)
                    .pr(8.0)
                    .items_start()
                    .gap(7.8013)
                    .fill(if selected == Some(index + 1) {
                        palette.selected
                    } else {
                        colors.code
                    })
                    .child(
                        row()
                            .w(gutter)
                            .min_w(gutter)
                            .h(LINE_HEIGHT)
                            .pr(9.8)
                            .justify_end()
                            .center_y()
                            .child(
                                text((index + 1).to_string())
                                    .font(MONO)
                                    .color(colors.muted)
                                    .a11y_hidden(),
                            ),
                    )
                    .child(content)
            },
        )
        .virtual_rows_version((preview.id, wrap));
    let mut viewport = col()
        .key(SCROLLER)
        .label("File contents")
        .role("document")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        });
    if !wrap {
        viewport = viewport.scroll_xy();
    }
    let mut contents = col().w_full().min_h(0.0).grow(1.0);
    if file.data.truncated {
        contents = contents.child(
            text(format!(
                "Preview limited to the first 1 MB of a {} byte file.",
                file.data.byte_length
            ))
            .font(NOTICE)
            .color(colors.muted)
            .wrap()
            .padding(12.0),
        );
    }
    contents.child(
        viewport.child(
            col()
                .w_full()
                .py(8.0)
                .fill(colors.code)
                .selection_group(true)
                .child(rows),
        ),
    )
}

fn error_view(preview: &Preview, error: &ReadFailure, colors: Colors) -> El {
    let mut details = col()
        .key("file-preview-error")
        .role("alert")
        .w_full()
        .items_center()
        .padding(24.0)
        .gap(12.0)
        .child(
            text(&error.message)
                .font(ERROR)
                .color(colors.error)
                .wrap()
                .align(Align::Center),
        );
    if let Some(path) = error.attempted_path.as_deref() {
        details = details.child(
            col()
                .w_full()
                .items_center()
                .child(text("Attempted path").font(ERROR).color(colors.muted))
                .child(
                    text(path)
                        .font(ERROR.family(SOURCE_FONT))
                        .color(colors.ink)
                        .wrap_anywhere()
                        .selectable(true)
                        .align(Align::Center),
                ),
        );
    }
    let cwd = error.cwd.as_deref().unwrap_or(&preview.cwd);
    details = details.child(
        text(format!(
            "Workspace folder: {cwd}. Check the link's path or locate the file in Files."
        ))
        .font(ERROR)
        .color(colors.muted)
        .wrap_anywhere()
        .align(Align::Center),
    );
    col().w_full().grow(1.0).min_h(0.0).center().child(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_links_decode_relative_paths_without_accepting_host_or_parent_paths() {
        let target = workspace_target("./notes/hello%20there.txt#L2").unwrap();
        assert_eq!(target.path, "notes/hello there.txt");
        assert_eq!(target.line, Some(2));
        for path in [
            "https://example.invalid/notes.txt",
            "file:///tmp/notes.txt",
            "C:/notes.txt",
            "%2fabsolute.txt",
            "../other.txt",
            "notes/%2e%2e/other.txt",
            "notes/file%00.txt",
            "notes/file.txt#L0",
        ] {
            assert!(workspace_target(path).is_none(), "{path}");
        }
    }
}
