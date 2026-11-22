use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use super::remote::public_host;

const MAX_CONFIG_BYTES: u64 = 64_000;
const MAX_CONFIG_FILES: usize = 64;
const MAX_INCLUDE_DEPTH: usize = 8;

#[derive(Clone)]
pub(super) struct Config {
    home: Option<PathBuf>,
    system: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        #[cfg(windows)]
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
        #[cfg(not(windows))]
        let home = std::env::var_os("HOME").map(PathBuf::from);
        #[cfg(windows)]
        let system =
            std::env::var_os("ProgramData").map(|path| PathBuf::from(path).join("ssh/ssh_config"));
        #[cfg(not(windows))]
        let system = Some(PathBuf::from("/etc/ssh/ssh_config"));
        Self { home, system }
    }
}

impl Config {
    #[cfg(test)]
    pub(super) fn isolated(config: PathBuf) -> Self {
        Self {
            home: None,
            system: Some(config),
        }
    }

    pub(super) fn resolve(&self, url: &str) -> String {
        let Some((prefix, host, suffix)) = alias_parts(url) else {
            return url.to_owned();
        };
        if public_host(host) {
            return url.to_owned();
        }
        let mut reader = Reader {
            host: host.to_ascii_lowercase(),
            home: self.home.as_deref(),
            seen: HashSet::new(),
        };
        let mut result = None;
        if let Some(home) = &self.home {
            let directory = home.join(".ssh");
            result = reader.hostname(&directory.join("config"), &directory, true, 0);
        }
        if result.is_none()
            && let Some(system) = &self.system
            && let Some(directory) = system.parent()
        {
            result = reader.hostname(system, directory, true, 0);
        }
        let Some(result) = result else {
            return url.to_owned();
        };
        if result == reader.host || (host.contains('.') && !public_host(&result)) {
            return url.to_owned();
        }
        format!("{prefix}{result}{suffix}")
    }
}

fn alias_parts(value: &str) -> Option<(&str, &str, &str)> {
    let without_scheme = value.strip_prefix("ssh://").unwrap_or(value);
    if without_scheme == value && (value.contains("://") || value.starts_with(['-', '/'])) {
        return None;
    }
    let prefix_len = value.len() - without_scheme.len();
    let end_authority = without_scheme
        .find([':', '/'])
        .unwrap_or(without_scheme.len());
    let authority = &without_scheme[..end_authority];
    let (user, host) = authority
        .split_once('@')
        .map_or(("", authority), |(user, host)| (user, host));
    if user.starts_with('-')
        || user.chars().any(char::is_whitespace)
        || host.starts_with('-')
        || host.is_empty()
        || !host.as_bytes()[0].is_ascii_alphanumeric()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return None;
    }
    let suffix = &without_scheme[end_authority..];
    if suffix.is_empty()
        || (prefix_len == 0 && !suffix.starts_with(':'))
        || suffix.starts_with("::")
    {
        return None;
    }
    let end_prefix = prefix_len + authority.len() - host.len();
    Some((&value[..end_prefix], host, suffix))
}

struct Reader<'a> {
    host: String,
    home: Option<&'a Path>,
    seen: HashSet<PathBuf>,
}

impl Reader<'_> {
    fn hostname(
        &mut self,
        path: &Path,
        include_root: &Path,
        mut active: bool,
        depth: usize,
    ) -> Option<String> {
        if depth >= MAX_INCLUDE_DEPTH || self.seen.len() >= MAX_CONFIG_FILES {
            return None;
        }
        let path = path.canonicalize().ok()?;
        if !self.seen.insert(path.clone())
            || !path
                .metadata()
                .is_ok_and(|entry| entry.is_file() && entry.len() <= MAX_CONFIG_BYTES)
        {
            return None;
        }
        let mut document = String::new();
        if std::fs::File::open(path)
            .ok()?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut document)
            .ok()? as u64
            > MAX_CONFIG_BYTES
        {
            return None;
        }
        for line in document.lines() {
            let mut fields = ssh_words(line);
            let Some(directive) = fields.first().map(|word| word.to_ascii_lowercase()) else {
                continue;
            };
            fields.remove(0);
            match directive.as_str() {
                "host" => active = matches_host(fields.iter().map(String::as_str), &self.host),
                "match" => active = matches_static_condition(&fields, &self.host),
                "hostname" if active => {
                    let hostname = fields
                        .first()?
                        .replace("%h", &self.host)
                        .to_ascii_lowercase();
                    return is_hostname(&hostname).then_some(hostname);
                }
                "include" if active => {
                    for include in fields {
                        for path in self.include_paths(include_root, &include) {
                            if let Some(hostname) =
                                self.hostname(&path, include_root, active, depth + 1)
                            {
                                return Some(hostname);
                            }
                        }
                    }
                }
                _ => (),
            }
        }
        None
    }

    fn include_paths(&self, directory: &Path, value: &str) -> Vec<PathBuf> {
        let path = if let Some(relative) = value.strip_prefix("~/") {
            let Some(home) = self.home else {
                return Vec::new();
            };
            home.join(relative)
        } else {
            directory.join(value)
        };
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            return Vec::new();
        };
        if !name.contains(['*', '?']) {
            return vec![path];
        }
        let Some(directory) = path.parent() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|file| wildcard(name, file))
            })
            .map(|entry| entry.path())
            .take(MAX_CONFIG_FILES + 1)
            .collect();
        if paths.len() > MAX_CONFIG_FILES {
            return Vec::new();
        }
        paths.sort();
        paths
    }
}

fn is_hostname(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
        && !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

fn ssh_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(escaped) = characters.next() {
                word.push(escaped);
            }
            continue;
        }
        if let Some(opening) = quote {
            if character == opening {
                quote = None
            } else {
                word.push(character)
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '#' => break,
            '=' if words.is_empty() && !word.is_empty() => words.push(std::mem::take(&mut word)),
            character if character.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word))
                }
            }
            character => word.push(character),
        }
    }
    if !word.is_empty() && quote.is_none() {
        words.push(word);
    }
    if words.get(1).is_some_and(|word| word == "=") {
        words.remove(1);
    }
    words
}

fn matches_static_condition(fields: &[String], host: &str) -> bool {
    match fields {
        [condition] => condition.eq_ignore_ascii_case("all"),
        [condition, patterns] => {
            let (criterion, negated) = condition
                .strip_prefix('!')
                .map_or((condition.as_str(), false), |criterion| (criterion, true));
            // Evaluate only conditions that depend on the original hostname.
            criterion.eq_ignore_ascii_case("originalhost")
                && matches_host(patterns.split(','), host) != negated
        }
        _ => false,
    }
}

fn matches_host<'a>(patterns: impl IntoIterator<Item = &'a str>, host: &str) -> bool {
    let mut matched = false;
    for pattern in patterns {
        let pattern = pattern.to_ascii_lowercase();
        if let Some(excluded) = pattern.strip_prefix('!') {
            if wildcard(excluded, host) {
                return false;
            }
        } else if wildcard(&pattern, host) {
            matched = true;
        }
    }
    matched
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v, mut star) = (0, 0, None);
    while v < value.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, v));
                p += 1;
            }
            Some(byte) if *byte == b'?' || *byte == value[v] => {
                p += 1;
                v += 1;
            }
            _ => {
                let Some((later_pattern, earlier_value)) = star else {
                    return false;
                };
                p = later_pattern + 1;
                v = earlier_value + 1;
                star = Some((later_pattern, v));
            }
        }
    }
    pattern[p..].iter().all(|byte| *byte == b'*')
}
