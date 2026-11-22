use serde_json::Value;

pub(crate) fn parse(content: &str) -> Option<Value> {
    let mut bytes = content.as_bytes().to_vec();
    let mut quoted = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => quoted = !quoted,
            b'\\' if quoted => index += 1,
            b'/' if !quoted && bytes.get(index + 1) == Some(&b'/') => {
                let end = index
                    + bytes[index..]
                        .iter()
                        .position(|byte| matches!(byte, b'\r' | b'\n'))
                        .unwrap_or(bytes.len() - index);
                bytes[index..end].fill(b' ');
                index = end;
                continue;
            }
            b'/' if !quoted && bytes.get(index + 1) == Some(&b'*') => {
                let end = index
                    + bytes[index + 2..]
                        .windows(2)
                        .position(|pair| pair == b"*/")?
                    + 4;
                bytes[index..end].fill(b' ');
                index = end;
                continue;
            }
            _ => {}
        }
        index += 1;
    }

    quoted = false;
    index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => quoted = !quoted,
            b'\\' if quoted => index += 1,
            b',' if !quoted => {
                let following = bytes[index + 1..]
                    .iter()
                    .find(|byte| !matches!(byte, b' ' | b'\r' | b'\n' | b'\t'));
                if matches!(following, Some(b'}' | b']')) {
                    bytes[index] = b' ';
                }
            }
            _ => {}
        }
        index += 1;
    }
    serde_json::from_slice(&bytes).ok()
}
