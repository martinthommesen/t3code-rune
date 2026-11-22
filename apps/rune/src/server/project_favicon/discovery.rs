use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex_lite::Regex;
use serde_json::{Value, json};

use crate::server::project_file;

const FAVICON_CANDIDATES: &[&str] = &[
    "favicon.svg",
    "favicon.ico",
    "favicon.png",
    "public/favicon.svg",
    "public/favicon.ico",
    "public/favicon.png",
    "app/favicon.ico",
    "app/favicon.png",
    "app/icon.svg",
    "app/icon.png",
    "app/icon.ico",
    "src/favicon.ico",
    "src/favicon.svg",
    "src/app/favicon.ico",
    "src/app/icon.svg",
    "src/app/icon.png",
    "assets/icon.svg",
    "assets/icon.png",
    "assets/logo.svg",
    "assets/logo.png",
    ".idea/icon.svg",
    "Resources/AppIcon.icns",
    "AppIcon.icns",
];
const ICON_SOURCE_FILES: &[&str] = &[
    "index.html",
    "public/index.html",
    "app/routes/__root.tsx",
    "src/routes/__root.tsx",
    "app/root.tsx",
    "src/root.tsx",
    "src/index.html",
];
const ICON_SOURCE_SCAN_BYTES: u64 = 128 * 1024;

static HTML_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<link\b[^<>]*>").unwrap());
static HTML_ICON_REL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\brel=["'](?:icon|shortcut icon)["']"#).unwrap());
static HTML_ICON_HREF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bhref=["']([^"'?]+)"#).unwrap());
static SOURCE_ICON_REL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\brel",
        r"[\t-\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]*",
        r":",
        r"[\t-\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]*",
        r#"["'](?:icon|shortcut icon)["']"#
    ))
    .unwrap()
});
static SOURCE_ICON_HREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\bhref",
        r"[\t-\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]*",
        r":",
        r"[\t-\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]*",
        r#"["']([^"'?]+)"#
    ))
    .unwrap()
});

#[derive(Debug)]
pub(super) struct ProbeError {
    operation: &'static str,
    method: &'static str,
    path: PathBuf,
    error: io::Error,
}

impl ProbeError {
    pub(super) fn stat(path: &Path, error: io::Error) -> Self {
        Self {
            operation: "stat-candidate",
            method: "stat",
            path: path.to_owned(),
            error,
        }
    }

    pub(super) fn cause(&self, root: &Path) -> Value {
        json!({
            "name": "ProjectFaviconResolutionError",
            "message": format!(
                "Failed to resolve project favicon during {} for workspace {}.",
                self.operation,
                root.display()
            ),
            "cause": filesystem_cause(self.method, &self.path, &self.error),
        })
    }
}

pub(super) fn filesystem_cause(method: &str, path: &Path, error: &io::Error) -> Value {
    let (reason, detail) = match error.kind() {
        io::ErrorKind::NotADirectory => ("BadResource", "ENOTDIR: not a directory".to_owned()),
        io::ErrorKind::PermissionDenied => {
            ("PermissionDenied", "EACCES: permission denied".to_owned())
        }
        io::ErrorKind::NotFound => ("NotFound", "ENOENT: no such file or directory".to_owned()),
        _ => ("Unknown", error.to_string()),
    };
    let path = path.display();
    json!({
        "name": "PlatformError",
        "message": format!("{reason}: FileSystem.{method} ({path})"),
        "cause": { "name": "Error", "message": format!("{detail}, {method} '{path}'") },
    })
}

pub(super) fn resolve(
    root: &Path,
    override_path: Option<&str>,
) -> Result<Option<PathBuf>, ProbeError> {
    if let Some(value) = override_path {
        let path = Path::new(value);
        let resolved = if path.is_absolute() {
            try_file(path.to_owned())?
        } else {
            candidate(root, path)?
        };
        if resolved.is_some() {
            return Ok(resolved);
        }
    }

    if let Some(icon_path) = project_file::icon_path(root)
        && let Some(found) = candidate(root, Path::new(&icon_path))?
    {
        return Ok(Some(found));
    }
    for value in FAVICON_CANDIDATES {
        if let Some(found) = candidate(root, Path::new(value))? {
            return Ok(Some(found));
        }
    }
    for source in ICON_SOURCE_FILES {
        if let Some(found) = from_source(root, source)? {
            return Ok(Some(found));
        }
    }
    candidate(root, Path::new("web/favicon.png"))
}

fn from_source(root: &Path, source: &str) -> Result<Option<PathBuf>, ProbeError> {
    let Some(path) = candidate(root, Path::new(source))? else {
        return Ok(None);
    };
    let Some(file) = not_found(File::open(&path)).map_err(|error| ProbeError {
        operation: "read-source",
        method: "open",
        path: path.clone(),
        error,
    })?
    else {
        return Ok(None);
    };
    let mut content = Vec::new();
    file.take(ICON_SOURCE_SCAN_BYTES)
        .read_to_end(&mut content)
        .map_err(|error| ProbeError {
            operation: "read-source",
            method: "read",
            path,
            error,
        })?;
    let source = String::from_utf8_lossy(&content);
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    let Some(href) = extract_icon_href(source) else {
        return Ok(None);
    };
    let clean = trim(href);
    let clean = clean.strip_prefix('/').unwrap_or(clean);
    let public = format!("public/{clean}");
    for value in [&public, clean] {
        if let Some(found) = candidate(root, Path::new(value))? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

fn extract_icon_href(source: &str) -> Option<&str> {
    fn find<'a>(
        sections: impl Iterator<Item = &'a str>,
        rel: &Regex,
        href: &Regex,
    ) -> Option<&'a str> {
        sections
            .filter(|section| rel.is_match(section))
            .find_map(|section| href.captures(section)?.get(1).map(|value| value.as_str()))
    }

    find(
        HTML_LINK.find_iter(source).map(|link| link.as_str()),
        &HTML_ICON_REL,
        &HTML_ICON_HREF,
    )
    .or_else(|| find(source.split('}'), &SOURCE_ICON_REL, &SOURCE_ICON_HREF))
}

fn candidate(root: &Path, path: &Path) -> Result<Option<PathBuf>, ProbeError> {
    match within_workspace(root, path) {
        Some(path) => try_file(path),
        None => Ok(None),
    }
}

fn try_file(path: PathBuf) -> Result<Option<PathBuf>, ProbeError> {
    Ok(is_file(&path)
        .map_err(|error| ProbeError::stat(&path, error))?
        .then_some(path))
}

pub(super) fn is_file(path: &Path) -> io::Result<bool> {
    Ok(not_found(fs::metadata(path))?.is_some_and(|value| value.is_file()))
}

pub(super) fn not_found<T>(value: io::Result<T>) -> io::Result<Option<T>> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn within_workspace(root: &Path, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let absolute = normalize(&root.join(path));
    (absolute != root && absolute.starts_with(root)).then_some(absolute)
}

pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir {
            result.pop();
        } else {
            result.push(component.as_os_str());
        }
    }
    result
}

pub(in crate::server) fn trim(raw: &str) -> &str {
    raw.trim_matches(|character: char| {
        (character.is_whitespace() && character != '\u{85}') || character == '\u{feff}'
    })
}
