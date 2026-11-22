mod git;
mod input;
mod links;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fff_search::{
    FilePicker, FilePickerOptions, FuzzySearchOptions, MixedItemRef, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};
use serde::Serialize;
use serde_json::Value;
use t3_rune::project_icon::FILE_EXTENSIONS;

pub(super) use self::input::Input;
use self::input::Kind;

const MAX_PAGE: usize = 25_002;
const IDLE: Duration = Duration::from_secs(15 * 60);
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);
const LINK_REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
pub(super) struct Indexes(Arc<Mutex<HashMap<PathBuf, ActiveIndex>>>);

struct ActiveIndex {
    index: Arc<WorkspaceIndex>,
    last_read: Instant,
}

struct WorkspaceIndex {
    picker: SharedFilePicker,
    links: Mutex<Option<(Instant, Arc<links::Links>)>>,
}

impl WorkspaceIndex {
    fn link_entries(&self, root: &Path, picker: &FilePicker, complete: bool) -> Arc<links::Links> {
        let Ok(mut cache) = self.links.lock() else {
            return Arc::new(links::Links::scan(root, picker));
        };
        if let Some((start, existing)) = &*cache
            && start.elapsed() < LINK_REFRESH
        {
            return Arc::clone(existing);
        }
        let entries = Arc::new(links::Links::scan(root, picker));
        if complete {
            *cache = Some((Instant::now(), Arc::clone(&entries)));
        }
        entries
    }
}

#[derive(Clone, Serialize)]
struct Entry {
    path: String,
    kind: Kind,
}

impl Entry {
    fn new(path: String, kind: Kind) -> Option<Self> {
        let path = normalize_path(path);
        (!path.is_empty()).then_some(Self { path, kind })
    }
}

#[derive(Serialize)]
pub(super) struct SearchResult {
    entries: Vec<Entry>,
    truncated: bool,
}

impl Indexes {
    pub(super) async fn search(&self, input: Input) -> Result<SearchResult, Value> {
        let root = resolve_workspace(&input)?;
        let root_label = root.to_string_lossy();
        let omitted = git::ignored_paths(&root).await;
        let host = self.clone();
        let query_root = root.clone();
        let query_input = input.clone();
        let mut result = tokio::task::spawn_blocking(move || {
            host.search_index(&query_root, &query_input, &omitted)
        })
        .await
        .map_err(|_| {
            input.failure(
                &root_label,
                "search_index_search_failed",
                "WorkspaceSearchIndexSearchFailed",
                format!("Workspace search failed for '{root_label}'."),
            )
        })??;
        let mut attached = git::nested_worktree_files(&root, &input, &result.entries).await;
        if !attached.is_empty() {
            attached.append(&mut result.entries);
            result.truncated |= attached.len() > input.limit;
            attached.truncate(input.limit);
            result.entries = attached;
        }
        Ok(result)
    }

    fn search_index(
        &self,
        root: &Path,
        input: &Input,
        omitted: &HashSet<String>,
    ) -> Result<SearchResult, Value> {
        let root_label = root.to_string_lossy();
        let engine_error = |error: String| {
            let mut failure = input.failure(
                &root_label,
                "search_index_create_failed",
                "WorkspaceSearchIndexCreateFailed",
                format!("Failed to create the workspace search index for '{root_label}'."),
            );
            failure["detail"] = Value::String(error);
            failure
        };
        let index = self.index(root).map_err(engine_error)?;
        let incomplete = !index.picker.wait_for_scan(SCAN_TIMEOUT);
        let finder = index
            .picker
            .read()
            .map_err(|error| engine_error(error.to_string()))?;
        let picker = finder
            .as_ref()
            .ok_or_else(|| engine_error("Workspace index is no longer available".into()))?;
        let links = index.link_entries(root, picker, !incomplete);
        let query = input
            .query
            .trim_start_matches(['@', '.', '/'])
            .to_lowercase()
            .replace('[', "");
        let mut result = find(picker, &query, &links, input, omitted);
        if result.entries.is_empty() && query.contains('{') {
            result = find(picker, &query.replace('{', ""), &links, input, omitted);
        }
        result.truncated |= incomplete;
        Ok(result)
    }

    fn index(&self, root: &Path) -> Result<Arc<WorkspaceIndex>, String> {
        let now = Instant::now();
        let mut indexes = self
            .0
            .lock()
            .map_err(|_| "Workspace search index lock failed")?;
        indexes.retain(|_, index| now.duration_since(index.last_read) < IDLE);
        if let Some(index) = indexes.get_mut(root) {
            index.last_read = now;
            return Ok(Arc::clone(&index.index));
        }

        let picker = SharedFilePicker::default();
        FilePicker::new_with_shared_state(
            picker.clone(),
            SharedFrecency::default(),
            FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;
        let index = Arc::new(WorkspaceIndex {
            picker,
            links: Mutex::new(None),
        });
        indexes.insert(
            root.to_owned(),
            ActiveIndex {
                index: Arc::clone(&index),
                last_read: now,
            },
        );
        Ok(index)
    }
}

fn resolve_workspace(input: &Input) -> Result<PathBuf, Value> {
    let failure = |root: &str, kind: &str, name: &str, description: &str| {
        input.failure(root, kind, name, format!("{description}: {root}"))
    };
    let root = super::filesystem_browse::resolve_target(&input.cwd, None).map_err(|_| {
        failure(
            &input.cwd,
            "workspace_root_stat_failed",
            "WorkspaceRootStatFailedError",
            "Failed to stat workspace root",
        )
    })?;
    let label = root.to_string_lossy();
    match fs::metadata(&root) {
        Ok(meta) if meta.is_dir() => Ok(root),
        Ok(_) => Err(failure(
            &label,
            "workspace_root_not_directory",
            "WorkspaceRootNotDirectoryError",
            "Workspace root is not a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(failure(
            &label,
            "workspace_root_not_found",
            "WorkspaceRootNotExistsError",
            "Workspace root does not exist",
        )),
        Err(_) => Err(failure(
            &label,
            "workspace_root_stat_failed",
            "WorkspaceRootStatFailedError",
            "Failed to stat workspace root",
        )),
    }
}

fn find(
    picker: &FilePicker,
    query: &str,
    links: &links::Links,
    input: &Input,
    omitted: &HashSet<String>,
) -> SearchResult {
    let visible_links = links
        .relevant(input.kind, input.images)
        .into_iter()
        .filter(|entry| !git::is_ignored(omitted, &entry.path))
        .collect::<Vec<_>>();
    let reserved = links.len().saturating_sub(visible_links.len());
    let page = (input.limit + reserved + 1).min(MAX_PAGE);
    let mut fetch = page;
    let (listed, engine_has_more) = loop {
        let (mut items, more) = indexed_entries(picker, query, input, fetch);
        items.retain(|entry| !git::is_ignored(omitted, &entry.path));
        if items.len() >= page || !more || fetch == MAX_PAGE {
            break (items, more);
        }
        fetch = (fetch * 4).min(MAX_PAGE);
    };
    let mut entries = links.merge(listed, visible_links, query);
    let truncated = engine_has_more || entries.len() > input.limit;
    entries.truncate(input.limit);
    SearchResult { entries, truncated }
}

fn indexed_entries(
    picker: &FilePicker,
    query: &str,
    input: &Input,
    page: usize,
) -> (Vec<Entry>, bool) {
    let parser = QueryParser::default();
    let options = |limit| FuzzySearchOptions {
        pagination: PaginationArgs { offset: 0, limit },
        ..Default::default()
    };
    let plain = parser.parse(query);
    if input.images {
        let constrained = format!("{query} {}", image_constraint());
        let parsed = parser.parse(&constrained);
        let lone_path = !query.chars().any(char::is_whitespace)
            && (query.starts_with('/')
                || query.ends_with('/') && !query.starts_with(['*', '!', '\\']));
        let mut result = if lone_path {
            picker.fuzzy_search(&plain, None, options(MAX_PAGE))
        } else {
            picker.fuzzy_search(&parsed, None, options(page))
        };
        if !lone_path
            && !query.is_empty()
            && result.total_files > 15_000
            && result.total_matched > 0
            && picker.fuzzy_search(&plain, None, options(1)).total_matched > 15_000
        {
            result = picker.fuzzy_search(&plain, None, options(MAX_PAGE));
        }
        let more = result.total_matched > result.items.len();
        return (
            result
                .items
                .into_iter()
                .filter_map(|item| Entry::new(item.relative_path(picker), Kind::File))
                .filter(|entry| image_path(&entry.path))
                .collect(),
            more,
        );
    }
    match input.kind {
        Some(Kind::File) => {
            let result = picker.fuzzy_search(&plain, None, options(page));
            let more = result.total_matched > result.items.len();
            (
                result
                    .items
                    .into_iter()
                    .filter_map(|item| Entry::new(item.relative_path(picker), Kind::File))
                    .collect(),
                more,
            )
        }
        Some(Kind::Directory) => {
            let result = picker.fuzzy_search_directories(&plain, options(page));
            let more = result.total_matched > result.items.len();
            (
                result
                    .items
                    .into_iter()
                    .filter_map(|item| Entry::new(item.relative_path(picker), Kind::Directory))
                    .collect(),
                more,
            )
        }
        None => {
            let result = picker.fuzzy_search_mixed(&plain, None, options(page));
            let more = result.total_matched > result.items.len();
            (
                result
                    .items
                    .into_iter()
                    .filter_map(|item| match item {
                        MixedItemRef::File(item) => {
                            Entry::new(item.relative_path(picker), Kind::File)
                        }
                        MixedItemRef::Dir(item) => {
                            Entry::new(item.relative_path(picker), Kind::Directory)
                        }
                    })
                    .collect(),
                more,
            )
        }
    }
}

fn normalize_path(path: String) -> String {
    let path = if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path
    };
    path.trim_end_matches('/').to_owned()
}

fn image_path(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, ext)| {
        FILE_EXTENSIONS
            .iter()
            .any(|format| ext.eq_ignore_ascii_case(format))
    })
}

fn image_constraint() -> &'static str {
    static GLOB: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let choices = FILE_EXTENSIONS
            .iter()
            .map(|extension| {
                extension
                    .chars()
                    .map(|letter| format!("[{letter}{}]", letter.to_ascii_uppercase()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("**/*.{{{choices}}}")
    });
    &GLOB
}
