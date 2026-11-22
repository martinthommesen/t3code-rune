use reqwest::Url;
use rusqlite::types::ValueRef;
use rusqlite::{Transaction, params};

const ATTACHMENT_PLACEHOLDER: &str = "[User attached one or more files without additional text. Respond using the conversation context and the attached files.]";
const JS_WHITESPACE: &str = "\t\n\u{b}\u{c}\r \u{a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

pub(super) fn migrate_69(db: &Transaction<'_>) -> rusqlite::Result<()> {
    match db.execute_batch(
        "CREATE VIRTUAL TABLE orchestration_v2_message_search USING fts5(
           text, content = '', contentless_delete = 1,
           tokenize = 'trigram remove_diacritics 1'
         )",
    ) {
        Ok(()) => {}
        // Some SQLite builds lack the FTS5 options. Readers can scan saved messages.
        Err(rusqlite::Error::SqliteFailure(error, _)) if error.extended_code == 1 => return Ok(()),
        Err(error) => return Err(error),
    }
    create_triggers(db)?;
    let retain = indexed_message("messages");
    let text = indexed_text("messages", false);
    db.execute_batch(&format!(
        "INSERT INTO orchestration_v2_message_search (rowid, text)
         SELECT messages.rowid, {text}
         FROM orchestration_v2_projection_messages AS messages
         WHERE {retain} AND json_extract(messages.payload_json, '$.text') IS NOT NULL;"
    ))
}

pub(super) fn migrate_74(db: &Transaction<'_>) -> rusqlite::Result<()> {
    recreate_triggers(db)
}

pub(super) fn migrate_81(db: &Transaction<'_>) -> rusqlite::Result<()> {
    db.execute_batch(
        "ALTER TABLE orchestration_v2_projection_messages ADD COLUMN search_text TEXT",
    )?;
    recreate_triggers(db)?;

    const PAGE_SIZE: usize = 500;
    let mut after_message_id = String::new();
    // Node SQLite replaces invalid UTF-8 from JSON strings with lone surrogates.
    let mut select = db.prepare(
        "SELECT message_id, json_extract(payload_json, '$.text') AS text
         FROM orchestration_v2_projection_messages
         WHERE message_id > ?1
           AND payload_json LIKE '%t3-citation://%'
           AND json_type(payload_json, '$.text') = 'text'
         ORDER BY message_id
         LIMIT ?2",
    )?;
    let mut update = db.prepare(
        "UPDATE orchestration_v2_projection_messages SET search_text = ?1 WHERE message_id = ?2",
    )?;
    loop {
        let mut rows = select
            .query_map(params![after_message_id, PAGE_SIZE], |row| {
                let text = row.get_ref(1)?;
                let ValueRef::Text(text) = text else {
                    return Err(rusqlite::Error::InvalidColumnType(
                        1,
                        "text".to_owned(),
                        text.data_type(),
                    ));
                };
                Ok((row.get::<_, String>(0)?, text.to_vec()))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (message_id, text) in &rows {
            if let Some(readable) = readable_search_text(&String::from_utf8_lossy(text)) {
                update.execute(params![readable, message_id])?;
            }
        }
        if rows.len() < PAGE_SIZE {
            return Ok(());
        }
        if let Some((id, _)) = rows.pop() {
            after_message_id = id;
        }
    }
}

fn recreate_triggers(db: &Transaction<'_>) -> rusqlite::Result<()> {
    let has_index = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'orchestration_v2_message_search')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !has_index {
        return Ok(());
    }
    db.execute_batch(
        "DROP TRIGGER orchestration_v2_message_search_insert;
         DROP TRIGGER orchestration_v2_message_search_update;
         DROP TRIGGER orchestration_v2_message_search_delete;",
    )?;
    create_triggers(db)
}

fn indexed_text(row: &str, has_search_text: bool) -> String {
    let stored = format!("json_extract({row}.payload_json, '$.text')");
    let readable = if has_search_text {
        format!("COALESCE({row}.search_text, {stored})")
    } else {
        stored
    };
    format!("replace(replace({readable}, '’', ''''), '‘', '''')")
}

fn indexed_message(row: &str) -> String {
    format!(
        "{row}.streaming = 0
         AND (
           {row}.role = 'assistant'
           OR (
             {row}.role = 'user'
             AND COALESCE(json_extract({row}.payload_json, '$.createdBy'), 'user') = 'user'
           )
         )
         AND NOT EXISTS (
           SELECT 1 FROM orchestration_v2_projection_threads AS subagent_threads
           WHERE subagent_threads.thread_id = {row}.thread_id
             AND json_extract(subagent_threads.payload_json, '$.lineage.relationshipToParent') = 'subagent'
         )
         AND CASE
           WHEN substr(json_extract({row}.payload_json, '$.text'), 1, 11) = 'Ultrathink:' COLLATE NOCASE
             THEN ltrim(substr(json_extract({row}.payload_json, '$.text'), 12), '{JS_WHITESPACE}')
           ELSE json_extract({row}.payload_json, '$.text')
         END COLLATE BINARY <> '{ATTACHMENT_PLACEHOLDER}'",
    )
}

fn create_triggers(db: &Transaction<'_>) -> rusqlite::Result<()> {
    let has_search_text = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('orchestration_v2_projection_messages') WHERE name = 'search_text')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    let new_text = indexed_text("new", has_search_text);
    let old_text = indexed_text("old", has_search_text);
    let indexed_message = indexed_message("new");
    db.execute_batch(&format!(
        "CREATE TRIGGER orchestration_v2_message_search_insert
         AFTER INSERT ON orchestration_v2_projection_messages
         WHEN {indexed_message}
         BEGIN
           INSERT INTO orchestration_v2_message_search (rowid, text)
           SELECT new.rowid, {new_text}
           WHERE json_extract(new.payload_json, '$.text') IS NOT NULL;
         END;

         CREATE TRIGGER orchestration_v2_message_search_update
         AFTER UPDATE ON orchestration_v2_projection_messages
         WHEN (old.streaming = 0 OR new.streaming = 0)
           AND (
             old.streaming IS NOT new.streaming
             OR old.role IS NOT new.role
             OR {old_text} IS NOT {new_text}
             OR json_extract(old.payload_json, '$.createdBy')
               IS NOT json_extract(new.payload_json, '$.createdBy')
           )
         BEGIN
           DELETE FROM orchestration_v2_message_search WHERE rowid = old.rowid AND old.streaming = 0;
           INSERT INTO orchestration_v2_message_search (rowid, text)
           SELECT new.rowid, {new_text}
           WHERE {indexed_message}
             AND json_extract(new.payload_json, '$.text') IS NOT NULL;
         END;

         CREATE TRIGGER orchestration_v2_message_search_delete
         AFTER DELETE ON orchestration_v2_projection_messages
         WHEN old.streaming = 0
         BEGIN
           DELETE FROM orchestration_v2_message_search WHERE rowid = old.rowid;
         END;",
    ))
}

const CITATION_HREF_PREFIX: &str = "t3-citation://v1/";
const CITATION_LINK_PREFIX: &str = "[Assistant quote](t3-citation://v1/";
const MAX_TEXT_LENGTH: usize = 8_000;
const MAX_COMMENT_LENGTH: usize = 8_000;
const MAX_CONTEXT_LENGTH: usize = 32;
const MAX_ID_LENGTH: usize = 4_096;
const MAX_HREF_LENGTH: usize =
    9 * (MAX_TEXT_LENGTH + MAX_COMMENT_LENGTH + 3 * MAX_ID_LENGTH + 2 * MAX_CONTEXT_LENGTH) + 1_000;

struct Citation {
    text: String,
    comment: Option<String>,
}

pub(in crate::server) fn readable_search_text(text: &str) -> Option<String> {
    let expanded = expand_citations(text);
    let readable = unescape_link_labels(&expanded);
    (readable != text).then_some(readable)
}

fn expand_citations(text: &str) -> String {
    let mut expanded = String::new();
    let mut copied = 0;
    for (start, _) in text.match_indices(CITATION_LINK_PREFIX) {
        if start < copied {
            continue;
        }
        let href_start = start + CITATION_LINK_PREFIX.len() - CITATION_HREF_PREFIX.len();
        let href = &text[href_start..];
        let Some(end) = link_target_end(href) else {
            continue;
        };
        if let Some(citation) = parse_citation(&href[..end]) {
            expanded.push_str(&text[copied..start]);
            expanded.push_str(&citation.text);
            if let Some(comment) = citation.comment {
                expanded.push_str("\nComment: ");
                expanded.push_str(&comment);
            }
            copied = href_start + end + 1;
        }
    }
    expanded.push_str(&text[copied..]);
    expanded
}

fn unescape_link_labels(text: &str) -> String {
    let mut readable = String::new();
    let mut cursor = 0;
    let mut copied = 0;
    while let Some(start) = text[cursor..].find('[') {
        let label_start = cursor + start + 1;
        let Some(length) = text[label_start..].find([']', '\n']) else {
            break;
        };
        let label_end = label_start + length;
        cursor = label_end + 1;
        if text.as_bytes()[label_end] != b']' {
            continue;
        }
        let Some(target) = text[cursor..].strip_prefix('(') else {
            continue;
        };
        let Some(end) = link_target_end(target) else {
            continue;
        };
        readable.push_str(&text[copied..label_start]);
        readable.push_str(
            &text[label_start..label_end]
                .replace("\\*", "*")
                .replace("\\_", "_"),
        );
        copied = label_end;
        cursor += end + 2;
    }
    readable.push_str(&text[copied..]);
    readable
}

fn link_target_end(target: &str) -> Option<usize> {
    let end = target.find(|ch| ch == ')' || JS_WHITESPACE.contains(ch))?;
    (target.as_bytes()[end] == b')').then_some(end)
}

fn parse_citation(href: &str) -> Option<Citation> {
    if !within_utf16_limit(href, MAX_HREF_LENGTH) {
        return None;
    }
    let url = Url::parse(href).ok()?;
    if url.scheme() != "t3-citation"
        || url.host_str() != Some("v1")
        || !url.username().is_empty()
        || url.password().is_some_and(|password| !password.is_empty())
        || url.port().is_some()
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
    {
        return None;
    }
    let mut parts = url.path().strip_prefix('/')?.split('/');
    for _ in 0..3 {
        let part = decode_path_part(parts.next()?)?;
        let trimmed = part.trim_matches(|ch| JS_WHITESPACE.contains(ch));
        if trimmed.is_empty() || !within_utf16_limit(trimmed, MAX_ID_LENGTH) {
            return None;
        }
    }
    if parts.next().is_some() {
        return None;
    }

    let mut fields = [None, None, None, None, None, None];
    for (key, value) in url.query_pairs() {
        let index = match key.as_ref() {
            "text" => 0,
            "start" => 1,
            "end" => 2,
            "prefix" => 3,
            "suffix" => 4,
            "comment" => 5,
            _ => return None,
        };
        if fields[index].replace(value).is_some() {
            return None;
        }
    }
    let [
        Some(text),
        Some(start),
        Some(end),
        Some(prefix),
        Some(suffix),
        comment,
    ] = fields
    else {
        return None;
    };
    if parse_offset(&start)? >= parse_offset(&end)?
        || text
            .trim_matches(|ch| JS_WHITESPACE.contains(ch))
            .is_empty()
        || !within_utf16_limit(&text, MAX_TEXT_LENGTH)
        || !within_utf16_limit(&prefix, MAX_CONTEXT_LENGTH)
        || !within_utf16_limit(&suffix, MAX_CONTEXT_LENGTH)
        || comment
            .as_ref()
            .is_some_and(|comment| !within_utf16_limit(comment, MAX_COMMENT_LENGTH))
    {
        return None;
    }
    Some(Citation {
        text: text.into_owned(),
        comment: comment.map(|comment| comment.into_owned()),
    })
}

fn parse_offset(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 16 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let offset = text.parse().ok()?;
    (offset <= 9_007_199_254_740_991).then_some(offset)
}

fn within_utf16_limit(text: &str, limit: usize) -> bool {
    text.len() <= limit || text.encode_utf16().take(limit + 1).count() <= limit
}

fn decode_path_part(text: &str) -> Option<String> {
    let mut bytes = text.as_bytes().iter().copied();
    let mut decoded = Vec::new();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            ((high << 4) | low) as u8
        } else {
            byte
        });
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests;
