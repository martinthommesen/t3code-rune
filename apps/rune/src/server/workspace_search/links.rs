use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path};

use fff_search::FilePicker;

use super::{Entry, Kind, MAX_PAGE, image_path, normalize_path};

#[derive(Default)]
pub(super) struct Links(BTreeMap<String, Option<Entry>>);

impl Links {
    pub(super) fn scan(root: &Path, picker: &FilePicker) -> Self {
        let Ok(canonical_root) = root.canonicalize() else {
            return Self::default();
        };
        let mut links = BTreeMap::new();
        let mut directories = picker
            .get_dirs()
            .iter()
            .take(MAX_PAGE)
            .map(|entry| normalize_path(entry.relative_path(picker)))
            .collect::<HashSet<_>>();
        directories.insert(String::new());

        for directory in directories {
            let Ok(children) = fs::read_dir(root.join(&directory)) else {
                continue;
            };
            for child in children.flatten() {
                if !child.file_type().is_ok_and(|kind| kind.is_symlink()) {
                    continue;
                }
                let path = child.path();
                let Some(relative) = path.strip_prefix(root).ok().and_then(Path::to_str) else {
                    continue;
                };
                let relative = normalize_path(relative.to_owned());
                let target = verify_target(&canonical_root, &path)
                    .and_then(|kind| Entry::new(relative.clone(), kind));
                links.insert(relative, target);
            }
        }
        Self(links)
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn relevant(&self, kind: Option<Kind>, images: bool) -> Vec<&Entry> {
        self.0
            .values()
            .filter_map(Option::as_ref)
            .filter(|entry| {
                if images {
                    entry.kind == Kind::File && image_path(&entry.path)
                } else {
                    kind.is_none_or(|kind| entry.kind == kind)
                }
            })
            .collect()
    }

    pub(super) fn merge(
        &self,
        indexed: Vec<Entry>,
        symlinks: Vec<&Entry>,
        query: &str,
    ) -> Vec<Entry> {
        if self.0.is_empty() {
            return indexed;
        }
        let present = indexed
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<HashSet<_>>();
        let mut pending = symlinks
            .iter()
            .filter_map(|entry| {
                (!present.contains(entry.path.as_str()))
                    .then(|| score(&entry.path, query).map(|value| (*entry, value)))
                    .flatten()
            })
            .collect::<Vec<_>>();
        pending.sort_by_key(|(_, value)| *value);
        let mut pending = pending.into_iter().peekable();
        let visible = symlinks
            .into_iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect::<HashMap<_, _>>();
        let mut entries = Vec::with_capacity(indexed.len());
        for item in indexed {
            let entry = if self.0.contains_key(&item.path) {
                let Some(entry) = visible.get(item.path.as_str()) else {
                    continue;
                };
                (*entry).clone()
            } else {
                item
            };
            let value = score(&entry.path, query).unwrap_or(usize::MAX);
            while pending.peek().is_some_and(|(_, rank)| *rank < value) {
                if let Some((entry, _)) = pending.next() {
                    entries.push(entry.clone());
                }
            }
            entries.push(entry);
        }
        entries.extend(pending.map(|(entry, _)| entry.clone()));
        entries
    }
}

fn verify_target(root: &Path, link: &Path) -> Option<Kind> {
    let canonical = link.canonicalize().ok()?;
    let relative = canonical.strip_prefix(root).ok()?;
    if relative
        .components()
        .any(|part| part == Component::Normal(".git".as_ref()))
    {
        return None;
    }
    let target = fs::metadata(&canonical).ok()?;
    if target.is_file() {
        Some(Kind::File)
    } else if target.is_dir() {
        Some(Kind::Directory)
    } else {
        None
    }
}

fn score(value: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let value = value.to_lowercase();
    let mut units = query.encode_utf16().peekable();
    let mut first = None;
    let mut previous = None;
    let mut gaps = 0;
    let length = query.encode_utf16().count();
    for (index, unit) in value.encode_utf16().enumerate() {
        if units.peek() != Some(&unit) {
            continue;
        }
        units.next();
        let start = *first.get_or_insert(index);
        if let Some(previous) = previous {
            gaps += index - previous - 1;
        }
        previous = Some(index);
        if units.peek().is_none() {
            return Some(
                start * 2 + gaps * 3 + index + 1 - start - length
                    + (value.encode_utf16().count() - length).min(64),
            );
        }
    }
    None
}
