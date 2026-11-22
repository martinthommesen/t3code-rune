use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use futures_util::stream;
use time::OffsetDateTime;

use crate::server::SharedEnvironment;

const SVG_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";
const HTML_CSP: &str = "sandbox allow-scripts allow-forms allow-popups allow-downloads";
const IMAGE_CACHE_CONTROL: &str = "private, max-age=3600";

pub(in crate::server) async fn get(
    State(environment): State<SharedEnvironment>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let Some(operations) = environment.local.as_ref() else {
        return text(StatusCode::NOT_FOUND, "Not Found");
    };
    let Some((token, _)) = uri
        .path()
        .strip_prefix("/api/assets/")
        .and_then(|suffix| suffix.split_once('/'))
    else {
        return text(StatusCode::NOT_FOUND, "Not Found");
    };
    let token = token.to_owned();
    let operations = Arc::clone(operations);
    tokio::task::spawn_blocking(move || {
        let Some(path) = operations.project_favicons.file(&token) else {
            return text(StatusCode::NOT_FOUND, "Not Found");
        };
        let head = method == Method::HEAD;
        let range = (!head && !headers.contains_key(header::IF_RANGE))
            .then(|| headers.get(header::RANGE)?.to_str().ok())
            .flatten();
        let result = if path
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".icns")
        {
            return fs::read(&path)
                .ok()
                .and_then(|bytes| icns_png(&bytes).map(ToOwned::to_owned))
                .map_or_else(
                    || text(StatusCode::NOT_FOUND, "Not Found"),
                    |bytes| icon_png(bytes, head),
                );
        } else {
            file_response(&path, head, range)
        };
        result.unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"))
    })
    .await
    .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"))
}

fn text(status: StatusCode, message: &'static str) -> Response {
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

fn image_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(IMAGE_CACHE_CONTROL),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

fn file_response(path: &Path, head: bool, requested_range: Option<&str>) -> io::Result<Response> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let size = metadata.len();
    let range = requested_range.map_or(ByteRange::Full, |value| byte_range(value, size));
    let mut response = Response::new(Body::empty());
    image_headers(&mut response);
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let extension = path_extension(path);
    let extension = extension.as_deref().unwrap_or("");
    let lower_path = path.to_string_lossy().to_ascii_lowercase();
    let html = lower_path.ends_with(".html") || lower_path.ends_with(".htm");
    if let Some(policy) = if lower_path.ends_with(".svg") {
        Some(SVG_CSP)
    } else {
        html.then_some(HTML_CSP)
    } {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(policy),
        );
    }
    let (status, offset, length) = match range {
        ByteRange::Full => (StatusCode::OK, 0, size),
        ByteRange::Unreachable => {
            set_header(headers, header::CONTENT_RANGE, format!("bytes */{size}"));
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            *response.body_mut() = Body::from_stream(stream::empty::<io::Result<&'static [u8]>>());
            return Ok(response);
        }
        ByteRange::Partial { first, last } => {
            set_header(
                headers,
                header::CONTENT_RANGE,
                format!("bytes {first}-{last}/{size}"),
            );
            (StatusCode::PARTIAL_CONTENT, first, last - first + 1)
        }
    };
    let mime = if html {
        "text/html; charset=utf-8"
    } else {
        file_mime(extension)
    };
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    set_header(headers, header::CONTENT_LENGTH, length.to_string());
    validators(headers, &metadata);
    *response.status_mut() = status;
    if !head && length != 0 {
        file.seek(SeekFrom::Start(offset))?;
        *response.body_mut() = Body::from_stream(stream::unfold(
            (file, length),
            |(mut file, remaining)| async move {
                if remaining == 0 {
                    return None;
                }
                match tokio::task::spawn_blocking(move || {
                    let mut buffer = vec![0; remaining.min(64 * 1024) as usize];
                    let result = file.read(&mut buffer);
                    (file, buffer, result)
                })
                .await
                {
                    Ok((file, mut buffer, Ok(count))) if count != 0 => {
                        buffer.truncate(count);
                        Some((Ok(buffer), (file, remaining - count as u64)))
                    }
                    Ok((file, _, result)) => Some((
                        Err(result
                            .err()
                            .unwrap_or(io::Error::from(io::ErrorKind::UnexpectedEof))),
                        (file, 0),
                    )),
                    Err(_) => None,
                }
            },
        ));
    }
    Ok(response)
}

fn validators(headers: &mut HeaderMap, metadata: &Metadata) {
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let modified = OffsetDateTime::from(modified);
    let millis = modified.unix_timestamp_nanos() / 1_000_000;
    set_header(
        headers,
        header::ETAG,
        format!("\"{:x}-{millis:x}\"", metadata.len()),
    );
    let weekday = modified.weekday().to_string();
    let month = modified.month().to_string();
    set_header(
        headers,
        header::LAST_MODIFIED,
        format!(
            "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
            &weekday[..3],
            modified.day(),
            &month[..3],
            modified.year(),
            modified.hour(),
            modified.minute(),
            modified.second(),
        ),
    );
}

fn set_header(headers: &mut HeaderMap, name: HeaderName, value: String) {
    headers.insert(name, HeaderValue::from_str(&value).expect("numeric header"));
}

enum ByteRange {
    Full,
    Partial { first: u64, last: u64 },
    Unreachable,
}

fn byte_range(raw: &str, size: u64) -> ByteRange {
    let raw = super::trim(raw);
    let Some(unit) = raw.get(..6) else {
        return ByteRange::Full;
    };
    if !unit.eq_ignore_ascii_case("bytes=") {
        return ByteRange::Full;
    }
    let Some((first, last)) = raw[6..].split_once('-') else {
        return ByteRange::Full;
    };
    if (first.is_empty() && last.is_empty())
        || !first
            .bytes()
            .chain(last.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return ByteRange::Full;
    }
    if !first.is_empty() && !last.is_empty() {
        let left = first.trim_start_matches('0');
        let right = last.trim_start_matches('0');
        if (right.len(), right) < (left.len(), left) {
            return ByteRange::Full;
        }
    }
    let first = (!first.is_empty()).then(|| first.parse::<u128>().unwrap_or(u128::MAX));
    let last = (!last.is_empty()).then(|| last.parse::<u128>().unwrap_or(u128::MAX));
    let size_u128 = u128::from(size);
    if size == 0
        || first.is_some_and(|first| first >= size_u128)
        || (first.is_none() && last == Some(0))
    {
        return ByteRange::Unreachable;
    }
    let (first, last) = match first {
        Some(first) => (first, last.unwrap_or(size_u128 - 1).min(size_u128 - 1)),
        None => (
            size_u128.saturating_sub(last.unwrap_or(size_u128)),
            size_u128 - 1,
        ),
    };
    ByteRange::Partial {
        first: first as u64,
        last: last as u64,
    }
}

fn path_extension(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

fn file_mime(extension: &str) -> &'static str {
    match extension {
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        "txt" => "text/plain",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn icon_png(bytes: Vec<u8>, head: bool) -> Response {
    let len = bytes.len();
    let mut response = Response::new(if head {
        Body::empty()
    } else {
        Body::from(bytes)
    });
    image_headers(&mut response);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    set_header(headers, header::CONTENT_LENGTH, len.to_string());
    response
}

fn icns_png(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.get(..4)? != b"icns" {
        return None;
    }
    let read_u32 = |offset: usize| -> Option<u32> {
        Some(u32::from_be_bytes(
            bytes.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let end = (read_u32(4)? as usize).min(bytes.len());
    let mut offset = 8;
    let mut best: Option<(u32, &[u8])> = None;
    while offset + 8 <= end {
        let length = read_u32(offset + 4)? as usize;
        let next = offset.checked_add(length)?;
        if length < 8 || next > end {
            break;
        }
        let start = offset + 8;
        offset = next;
        let png = &bytes[start..next];
        if png.len() < 24 || !png.starts_with(b"\x89PNG") || &png[12..16] != b"IHDR" {
            continue;
        }
        let width = read_u32(start + 16)?;
        if width == 0 || read_u32(start + 20)? == 0 {
            continue;
        }
        if best.is_none_or(|(existing, _)| {
            if existing < 96 {
                width > existing
            } else {
                (96..existing).contains(&width)
            }
        }) {
            best = Some((width, png));
        }
    }
    best.map(|(_, png)| png)
}
