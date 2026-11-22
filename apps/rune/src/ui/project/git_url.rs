use reqwest::Url;
use rune::color::{hex, oklch};
use rune::{
    Color, El, Icon, Key, KeyEvent, ScrollSpec, TextStyle, Tx, col, el, icon, row, text, text_input,
};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{AddProject, Browse, FolderListing, Palette, ProjectStep, State, error_text};

const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(20.0);
const INPUT: TextStyle = TextStyle::new(14.0, 400.0).lh(34.0);
const FOOTER: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
const SMALL: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0);
const KEY_CAP: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);

static LINK: Icon = Icon {
    name: "git-url-link",
    view: 24.0,
    body: r#"<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71m2.25 5.82a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static BACK: Icon = Icon {
    name: "git-url-back",
    view: 24.0,
    body: r#"<path d="m12 19-7-7 7-7m-7 7h14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static FOLDER: Icon = Icon {
    name: "git-url-folder",
    view: 24.0,
    body: r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.7-.9l-.8-1.2A2 2 0 0 0 7.9 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static UP: Icon = Icon {
    name: "git-url-parent",
    view: 24.0,
    body: r#"<path d="m14 9-5-5-5 5m16 11h-7a4 4 0 0 1-4-4V4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub(super) struct Destination {
    raw: String,
    remote: String,
    leaf: String,
    command: Option<(String, String)>,
}

impl Destination {
    fn new(raw: &str) -> Self {
        let remote = normalized_remote(raw);
        Self {
            raw: raw.into(),
            leaf: directory_name(&remote).into(),
            remote,
            command: None,
        }
    }

    pub(super) fn browse_query<'a>(&self, path: &'a str) -> Option<&'a str> {
        let name = path.rsplit(['/', '\\']).next()?;
        if name != self.leaf || name.is_empty() {
            return None;
        }
        let base = &path[..path.len() - name.len()];
        (!base.is_empty()).then_some(base)
    }

    pub(super) fn browse_destination(&self, path: &str) -> String {
        let path = directory_path(path);
        let leaf = path
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next();
        if leaf == Some(self.leaf.as_str()) {
            path
        } else {
            format!("{path}{}", self.leaf)
        }
    }

    pub(super) fn filter_entries(&self, path: &str, listing: &mut FolderListing) -> bool {
        let typed = path.rsplit(['/', '\\']).next().unwrap_or_default();
        let normalized = typed.nfc().collect::<String>();
        let exact = !typed.is_empty()
            && (listing.entries.iter().any(|entry| entry.name == typed)
                || listing
                    .entries
                    .iter()
                    .filter(|entry| entry.name.nfc().eq(normalized.chars()))
                    .count()
                    == 1);
        if typed == self.leaf || !typed.starts_with('.') {
            listing.entries.retain(|entry| !entry.name.starts_with('.'));
        }
        exact
    }

    fn project_id(&mut self, destination: &str) -> String {
        if let Some((path, id)) = &self.command
            && path == destination
        {
            return id.clone();
        }
        let id = Uuid::new_v4().to_string();
        self.command = Some((destination.into(), id.clone()));
        id
    }
}

fn normalized_remote(raw: &str) -> String {
    let raw = raw.trim();
    if let Some((owner, repo)) = raw.split_once('/')
        && !owner.is_empty()
        && owner.len() <= 39
        && owner.as_bytes()[0].is_ascii_alphanumeric()
        && owner
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        && !repo.is_empty()
        && repo
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"._-".contains(&ch))
    {
        return format!(
            "https://github.com/{owner}/{}.git",
            repo.strip_suffix(".git").unwrap_or(repo)
        );
    }
    if let Ok(mut url) = Url::parse(raw)
        && matches!(url.scheme(), "http" | "https")
        && let Some(repository_path) = repository_page_path(&url)
    {
        url.set_path(&repository_path);
        url.set_query(None);
        url.set_fragment(None);
        return url.into();
    }
    raw.into()
}

fn repository_page_path(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let named = |name| host.split('.').any(|part| part == name);
    let path = url.path();
    let segments: Vec<_> = path.strip_prefix('/')?.split('/').collect();
    let hosted_tree = named("github")
        || host.ends_with(".ghe.com")
        || matches!(host, "git.cafe" | "staging.git.cafe")
        || (host == "source.tanstack.com"
            && segments.first().is_some_and(|owner| owner.starts_with('@')));
    let bitbucket = named("bitbucket") || host.ends_with(".bitbucket.org");
    if let [owner, repo, route, branch, ..] = segments.as_slice()
        && !owner.is_empty()
        && !repo.is_empty()
        && !branch.is_empty()
        && (hosted_tree && matches!(*route, "tree" | "blob") || bitbucket && *route == "src")
    {
        return Some(format!("/{owner}/{repo}"));
    }
    let forgejo = segments
        .windows(3)
        .enumerate()
        .find(|(index, route)| {
            *index >= 2
                && route[0] == "src"
                && matches!(route[1], "branch" | "tag" | "commit")
                && !route[2].is_empty()
        })
        .map(|(index, _)| index);
    let gitlab = segments
        .iter()
        .enumerate()
        .find(|(index, part)| *index >= 2 && **part == "-")
        .filter(|(index, _)| *index + 1 < segments.len())
        .map(|(index, _)| index);
    let end = if hosted_tree || bitbucket {
        None
    } else if (host == "codeberg.org" || named("forgejo") || named("gitea"))
        && !path.trim_end_matches('/').ends_with(".git")
    {
        forgejo
    } else {
        gitlab.filter(|gitlab| named("gitlab") || forgejo.is_none_or(|forgejo| *gitlab < forgejo))
    }?;
    segments[..end]
        .iter()
        .all(|part| !part.is_empty())
        .then(|| format!("/{}", segments[..end].join("/")))
}

fn directory_name(remote: &str) -> &str {
    let clean = remote.split(['?', '#']).next().unwrap_or_default().trim();
    let (value, hosted) = clean.split_once("://").map_or_else(
        || {
            (
                clean,
                clean
                    .split_once(':')
                    .is_some_and(|(address, _)| address.contains('@')),
            )
        },
        |(_, path)| (path, true),
    );
    let mut segments = value
        .rsplit(['/', '\\', ':'])
        .filter(|segment| !segment.trim().is_empty());
    let Some(last) = segments.next().map(str::trim) else {
        return "";
    };
    if hosted
        && (segments.next().is_none()
            || (last.bytes().all(|c| c.is_ascii_digit()) && segments.next().is_none()))
    {
        return "";
    }
    last.strip_suffix(".git").unwrap_or(last)
}

fn directory_path(path: &str) -> String {
    if path.ends_with(['/', '\\']) {
        return path.into();
    }
    let separator = if path.contains('\\') && !path.contains('/') {
        '\\'
    } else {
        '/'
    };
    format!("{path}{separator}")
}

pub(super) fn browse_parent(input: &str, directory: &str) -> Option<String> {
    let at_home = input.trim_end_matches(['/', '\\']) == "~"
        || input
            .rsplit_once(['/', '\\'])
            .is_some_and(|(base, _)| base == "~");
    if at_home {
        return None;
    }
    let directory = directory.trim_end_matches(['/', '\\']);
    let separator = directory.rfind(['/', '\\'])?;
    let end = if separator == 0 || (separator == 2 && directory.as_bytes()[1] == b':') {
        separator + 1
    } else {
        separator
    };
    Some(directory[..end].into())
}

impl State {
    fn choose_project_git(&mut self, tx: &mut Tx) {
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| form.sources.git && form.pending.is_none())
        else {
            return;
        };
        form.step = ProjectStep::GitUrl;
        form.browse = None;
        form.clone = None;
        form.error = None;
        form.path.clear();
        tx.focus_when_shown("project-git-url", false);
    }

    pub(super) fn back_to_project_sources(&mut self, tx: &mut Tx) {
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| form.pending.is_none())
        else {
            return;
        };
        form.step = ProjectStep::Sources;
        form.browse = None;
        form.clone = None;
        form.error = None;
        form.path.clear();
        tx.focus_when_shown("project-source-git", false);
    }

    fn continue_git_project(&mut self, tx: &mut Tx) {
        let cwd = self.project_browse_cwd();
        let can_browse = self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.filesystem_read);
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| matches!(form.step, ProjectStep::GitUrl))
        else {
            return;
        };
        let raw = form.path.text.trim();
        if raw.is_empty() {
            return;
        }
        let clone = Destination::new(raw);
        form.path.replace_text(format!("~/{}", clone.leaf));
        form.clone = Some(clone);
        form.browse = can_browse.then(|| Browse {
            cwd,
            ..Browse::default()
        });
        form.step = ProjectStep::GitDestination;
        tx.focus_when_shown("project-git-destination", false);
    }

    pub(super) fn submit_git_project(&mut self, _: &mut Tx) {
        let cwd = self.project_browse_cwd();
        let Some(connection) = self.connection.as_ref().filter(|_| {
            self.workspace.as_ref().is_some_and(|workspace| {
                workspace.clone_writes && workspace.project_writes && !workspace.reconnecting
            })
        }) else {
            return;
        };
        let Some(form) = self
            .add_project
            .as_mut()
            .filter(|form| form.pending.is_none())
        else {
            return;
        };
        let Some(clone) = form.clone.as_mut() else {
            return;
        };
        let path = form.path.text.trim();
        if path.is_empty() {
            return;
        }
        let relative = path.starts_with("./")
            || path.starts_with("../")
            || path.starts_with(".\\")
            || path.starts_with("..\\");
        let destination = if relative {
            let Some(cwd) = cwd.as_deref() else {
                form.error = Some("Relative paths require an active project.".into());
                return;
            };
            format!("{}{path}", directory_path(cwd))
        } else {
            path.into()
        };
        let title = destination
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(path);
        let id = clone.project_id(&destination);
        connection.clone_project(
            id.clone(),
            clone.remote.clone(),
            destination.clone(),
            title.into(),
        );
        form.pending = Some(id);
        form.error = None;
    }

    fn git_project_keyboard(&mut self, event: &KeyEvent, tx: &mut Tx) -> bool {
        if event.key == Key::Backspace
            && self
                .add_project
                .as_ref()
                .is_some_and(|form| form.path.text.is_empty() && form.pending.is_none())
        {
            self.back_to_project_sources(tx);
            return true;
        }
        self.browse_keyboard(event, tx)
    }
}

pub(super) fn source_item(palette: Palette) -> El {
    row()
        .key("project-source-git")
        .w_full()
        .px(8.0)
        .py(6.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .role("button")
        .label("Git URL")
        .focusable(true)
        .hover(palette.selected)
        .focus_ring(1.0, -1.0, palette.accent)
        .on_press_tx(State::choose_project_git)
        .child(icon(&LINK, 16.0).color(palette.muted).a11y_hidden())
        .child(
            col()
                .child(text("Git URL").font(BODY).color(palette.ink))
                .child(
                    text("Clone from a remote URL")
                        .font(SMALL)
                        .color(palette.muted),
                ),
        )
}

pub(super) fn dialog(form: &AddProject, palette: Palette) -> El {
    let clone = form.clone.as_ref();
    let destination = clone.is_some();
    let busy = form.pending.is_some();
    let has_path = !form.path.text.trim().is_empty();
    let highlighted = form
        .browse
        .as_ref()
        .is_some_and(|browse| browse.active_row > 0);
    let will_create = form.browse.as_ref().is_some_and(|browse| {
        browse.pending.is_none()
            && browse.error.is_none()
            && browse.listing.is_some()
            && !browse.exact_entry
            && !form.path.text.ends_with(['/', '\\'])
    });
    let label = if busy {
        "Cloning"
    } else if !destination {
        "Continue"
    } else if has_path && will_create && !highlighted {
        "Create & Clone"
    } else {
        "Clone"
    };
    let key_hint = if highlighted {
        if cfg!(target_os = "macos") {
            "⌘ Enter"
        } else {
            "Ctrl Enter"
        }
    } else {
        "Enter"
    };
    let key = if destination {
        "project-git-destination"
    } else {
        "project-git-url"
    };
    let placeholder = if destination {
        "Enter path (e.g. ~/projects/my-app)"
    } else {
        "Enter Git clone URL"
    };
    let enter = if destination {
        State::activate_project_browse
    } else {
        State::continue_git_project
    };
    let submit = if destination {
        State::submit_git_project
    } else {
        State::continue_git_project
    };
    let focus_label = if destination {
        "Destination path"
    } else {
        "Git clone URL"
    };
    let edit = text_input(&form.path, INPUT)
        .key(key)
        .h(34.0)
        .min_w(0.0)
        .grow(1.0)
        .single_line()
        .disabled(busy)
        .label(focus_label)
        .color(palette.ink)
        .placeholder(placeholder, palette.muted)
        .selection_color(palette.selected)
        .on_edit(|state: &mut State| {
            &mut state
                .add_project
                .as_mut()
                .expect("open project dialog")
                .path
        })
        .on_submit(enter)
        .on_input_key(State::git_project_keyboard);
    let back = el()
        .key("project-git-back")
        .size(32.0, 34.0)
        .center()
        .role("button")
        .label("Back")
        .focusable(!busy)
        .disabled(busy)
        .on_press_tx(State::back_to_project_sources)
        .child(icon(&BACK, 14.0).color(palette.muted).a11y_hidden());
    let (panel, line, cap, control, control_line, footer_fill) = if palette.dark {
        (
            Color::BLACK.alpha(0.8),
            Color::WHITE.alpha(0.08),
            oklch(0.97, 0.0, 0.0).alpha(0.08),
            Color::WHITE.alpha(0.0352),
            Color::WHITE.alpha(0.11),
            oklch(0.97, 0.0, 0.0).alpha(0.025),
        )
    } else {
        let ink = oklch(0.21, 0.006, 285.885);
        (
            hex("#fcfcfc").expect("static command palette color"),
            ink.alpha(0.10),
            ink.alpha(0.08),
            Color::WHITE,
            hex("#d4d4d8").expect("static command palette color"),
            hex("#f8f8f8").expect("static command palette color"),
        )
    };
    let button = row()
        .key(if destination {
            "clone-git-project"
        } else {
            "continue-git-project"
        })
        .h(24.0)
        .px(7.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .fill(control)
        .border(1.0, control_line)
        .border_layout()
        .role("button")
        .label(format!("{label} ({key_hint})"))
        .focusable(has_path && !busy)
        .disabled(!has_path || busy)
        .focus_ring(1.0, 1.0, palette.accent)
        .on_press_tx(submit)
        .child(
            text(label)
                .font(KEY_CAP)
                .color(if has_path { palette.ink } else { palette.muted }),
        )
        .child(key_cap(key_hint, cap, palette.muted));
    let header = row()
        .w_full()
        .h(48.0)
        .pl(9.0)
        .pr(10.0)
        .center_y()
        .child(back)
        .child(edit)
        .child(button);
    let body = if let Some(clone) = clone {
        git_destination(form, clone, palette)
    } else {
        col().w_full().h(100.0).center().child(
            text("Enter a Git clone URL and press Enter to continue.")
                .font(BODY)
                .color(palette.muted),
        )
    };
    let mut footer = row()
        .w_full()
        .h(if destination { 45.0 } else { 41.0 })
        .px(16.0)
        .gap(12.0)
        .center_y()
        .corners([0.0, 0.0, 15.0, 15.0])
        .border_sides([1.0, 0.0, 0.0, 0.0], line)
        .fill(footer_fill)
        .child(
            row()
                .gap(4.0)
                .center_y()
                .child(key_cap("↑", cap, palette.muted))
                .child(key_cap("↓", cap, palette.muted))
                .child(footer_label("Navigate", palette)),
        );
    if !destination || highlighted {
        footer = footer.child(
            row()
                .gap(8.0)
                .center_y()
                .child(key_cap("Enter", cap, palette.muted))
                .child(footer_label(
                    if destination { "Select" } else { "Continue" },
                    palette,
                )),
        );
    }
    footer = footer
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(key_cap("Backspace", cap, palette.muted))
                .child(footer_label("Back", palette)),
        )
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(key_cap("Esc", cap, palette.muted))
                .child(footer_label("Close", palette)),
        );
    let card = col()
        .key("git-project-dialog")
        .w_full()
        .max_w(576.0)
        .max_h(420.0)
        .radius(16.0)
        .fill(panel)
        .border(1.0, line)
        .border_layout()
        .role("dialog")
        .label("Command palette")
        .on_key(|state: &mut State, event, _| {
            if event.key == Key::Escape
                && state
                    .add_project
                    .as_ref()
                    .is_some_and(|form| form.pending.is_none())
            {
                state.add_project = None;
                return true;
            }
            false
        })
        .child(header)
        .child(body)
        .child(footer);
    col()
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .items_center()
        .pt(74.0)
        .fill(Color::BLACK.alpha(0.55))
        .child(card)
}

fn key_cap(label: &str, fill: Color, ink: Color) -> El {
    el().h(20.0)
        .min_w(20.0)
        .px(4.0)
        .center()
        .radius(4.0)
        .fill(fill)
        .child(text(label).font(KEY_CAP).color(ink))
}

fn footer_label(label: &str, palette: Palette) -> El {
    text(label).font(FOOTER).color(palette.muted)
}

fn git_destination(form: &AddProject, clone: &Destination, palette: Palette) -> El {
    let summary = col()
        .w_full()
        .h(84.0)
        .padding(8.0)
        .child(
            text("Repository")
                .font(SMALL)
                .color(palette.muted)
                .px(8.0)
                .py(6.0),
        )
        .child(
            row()
                .w_full()
                .px(8.0)
                .gap(8.0)
                .center_y()
                .child(icon(&LINK, 16.0).color(palette.muted).a11y_hidden())
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .child(text(&clone.raw).font(BODY).color(palette.ink).truncate())
                        .child(
                            text(&clone.remote)
                                .font(SMALL)
                                .color(palette.muted)
                                .truncate(),
                        ),
                ),
        );
    let mut contents = col().w_full().child(summary);
    if let Some(browse) = &form.browse
        && let Some(listing) = &browse.listing
        && browse.row_count() > 0
    {
        let mut dirs = col().w_full().padding(8.0).child(
            text("Select where to clone")
                .font(SMALL)
                .color(palette.muted)
                .px(8.0)
                .py(6.0),
        );
        let busy = form.pending.is_some();
        if let Some(parent) = &browse.parent {
            dirs = dirs.child(destination_entry(
                "..",
                parent,
                &UP,
                browse.active_row == 1,
                busy,
                palette,
            ));
        }
        for (index, entry) in listing.entries.iter().enumerate() {
            let selected = browse.active_row == index + 1 + usize::from(browse.parent.is_some());
            dirs = dirs.child(destination_entry(
                &entry.name,
                &entry.full_path,
                &FOLDER,
                selected,
                busy,
                palette,
            ));
        }
        contents = contents.child(
            col()
                .key("project-folder-list")
                .w_full()
                .max_h(233.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(dirs),
        );
    } else {
        contents = contents.child(
            col().w_full().h(76.0).center().child(
                text("Choose a destination path and press Enter to clone.")
                    .font(BODY)
                    .color(palette.muted),
            ),
        );
    }
    if let Some(error) = form
        .error
        .as_ref()
        .or_else(|| form.browse.as_ref()?.error.as_ref())
    {
        contents = contents.child(
            col()
                .w_full()
                .px(16.0)
                .pb(8.0)
                .child(error_text(error, palette)),
        );
    }
    contents
}

fn destination_entry(
    name: &str,
    path: &str,
    symbol: &'static Icon,
    selected: bool,
    busy: bool,
    palette: Palette,
) -> El {
    let key = format!("project-entry-{path}");
    let path = path.to_owned();
    row()
        .key(key)
        .w_full()
        .h(32.0)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .role("button")
        .label(name)
        .focusable(!busy)
        .disabled(busy)
        .hover(palette.selected)
        .fill(if selected {
            palette.selected
        } else {
            Color::TRANSPARENT
        })
        .on_press_tx(move |state: &mut State, tx| state.navigate_project_folder(&path, tx))
        .child(icon(symbol, 16.0).color(palette.muted).a11y_hidden())
        .child(text(name).font(BODY).color(palette.ink).truncate())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_urls_keep_the_actual_remote_and_pin_the_inferred_destination() {
        assert_eq!(
            normalized_remote("anomalyco/rune"),
            "https://github.com/anomalyco/rune.git"
        );
        assert_eq!(
            normalized_remote(
                "http://user@github.corp.example:1234/a/b/blob/main/doc.md?raw=1#test"
            ),
            "http://user@github.corp.example:1234/a/b"
        );
        assert_eq!(
            normalized_remote("https://github.com/a/b"),
            "https://github.com/a/b"
        );
        for (url, repository) in [
            ("https://git.cafe/a/b/tree/main", "https://git.cafe/a/b"),
            (
                "https://code.example.test/g/sub/g/-/blob/main/README",
                "https://code.example.test/g/sub/g",
            ),
            (
                "https://user@bitbucket.org/team/project/src/master/",
                "https://user@bitbucket.org/team/project",
            ),
            (
                "https://code.example.test/team/repo/src/branch/main/-/README.md",
                "https://code.example.test/team/repo/src/branch/main/-/README.md",
            ),
        ] {
            assert_eq!(normalized_remote(url), repository);
        }
        let mut clone = Destination::new("git@example.org:owner/native.git");
        assert_eq!(clone.leaf, "native");
        assert_eq!(clone.browse_query("~/native"), Some("~/"));
        assert_eq!(clone.browse_query("~/another"), None);
        assert_eq!(clone.browse_destination("/work/native"), "/work/native/");
        assert_eq!(clone.browse_destination("/work/new"), "/work/new/native");
        let pending = clone.project_id("/work/new/native");
        assert_eq!(clone.project_id("/work/new/native"), pending);
        assert_ne!(clone.project_id("/work/different"), pending);
    }
}
