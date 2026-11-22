const RESET_SGR: &str = "\x1b[0m";

pub(super) fn utf16_len(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.encode_utf16().count()
    }
}

pub(super) fn utf16_boundary(text: &str, target: usize) -> (usize, usize) {
    if text.is_ascii() {
        return (target.min(text.len()), 0);
    }
    let mut offset = 0;
    for (byte, character) in text.char_indices() {
        if offset >= target {
            return (byte, offset - target);
        }
        offset += character.len_utf16();
    }
    (text.len(), offset.saturating_sub(target))
}

pub(super) fn trim_tail(text: &mut String, max_units: usize) -> bool {
    let units = utf16_len(text);
    if units <= max_units {
        return false;
    }
    let (start, skipped_surrogate) = utf16_boundary(text, units - max_units);
    if let Some(offset) = text[start..].find('\n')
        && (utf16_len(&text[start..start + offset]) + skipped_surrogate) * 4 < max_units
    {
        text.drain(..start + offset + 1);
        return true;
    }
    let (start, pen) = style_at(text, start);
    if pen.is_empty() {
        text.drain(..start);
    } else {
        let pen = pen.to_owned();
        text.replace_range(..start, &pen);
    }
    true
}

fn style_at(text: &str, index: usize) -> (usize, &str) {
    let mut cursor = text[..index].rfind('\n').map_or(0, |newline| newline + 1);
    let mut pen = "";
    while cursor < index {
        let Some(offset) = text[cursor..].find("\x1b[") else {
            break;
        };
        let found = cursor + offset;
        if found >= index {
            break;
        }
        let Some(offset) = text[found..].find('m') else {
            break;
        };
        let end = found + offset;
        pen = &text[found..=end];
        if end >= index {
            return (end + 1, pen);
        }
        cursor = end + 1;
    }
    (index, if pen == RESET_SGR { "" } else { pen })
}
