use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct Recent {
    pub path: PathBuf,
    pub mtime: u64,
}

pub fn collect(limit: usize) -> Vec<Recent> {
    let mut all: Vec<Recent> = Vec::new();
    all.extend(parse_xbel());
    for sub in ["Downloads", "Documents", "Desktop"] {
        all.extend(walk_home_subdir(sub));
    }

    // dedupe by path, keep latest mtime
    all.sort_by(|a, b| a.path.cmp(&b.path).then(b.mtime.cmp(&a.mtime)));
    all.dedup_by(|a, b| a.path == b.path);

    all.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    all.truncate(limit);
    all
}

fn xbel_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/recently-used.xbel"))
}

// Hand-rolled XBEL extraction: walks bookmark elements, pulls href and modified.
// Avoids a heavy XML dep — XBEL grammar is small enough that we can scan attrs.
fn parse_xbel() -> Vec<Recent> {
    let Some(path) = xbel_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };

    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(start) = text[cursor..].find("<bookmark") {
        let abs_start = cursor + start;
        let Some(end) = text[abs_start..].find('>') else { break };
        let tag = &text[abs_start..abs_start + end];
        cursor = abs_start + end + 1;

        let href = extract_attr(tag, "href");
        let modified = extract_attr(tag, "modified")
            .or_else(|| extract_attr(tag, "added"));
        let (Some(href), Some(modified)) = (href, modified) else { continue };
        let Some(path) = decode_file_url(&href) else { continue };
        if !path.exists() { continue; }
        let mtime = parse_iso8601(&modified).unwrap_or(0);
        out.push(Recent { path, mtime });
    }
    out
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let i = tag.find(&needle)?;
    let after = &tag[i + needle.len()..];
    let j = after.find('"')?;
    Some(after[..j].to_string())
}

fn decode_file_url(s: &str) -> Option<PathBuf> {
    let rest = s.strip_prefix("file://")?;
    let mut out = String::with_capacity(rest.len());
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            out.push(byte as char);
            i += 3;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Some(PathBuf::from(out))
}

// Parses ISO-8601 like "2024-05-03T12:34:56Z" into a unix timestamp.
// Approximate (ignores leap seconds, fractional, timezone offsets); good
// enough for sort ordering of "recently used" entries.
fn parse_iso8601(s: &str) -> Option<u64> {
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let y: i64 = date_parts.next()?.parse().ok()?;
    let mo: i64 = date_parts.next()?.parse().ok()?;
    let d: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let h: i64 = time_parts.next()?.parse().ok()?;
    let mi: i64 = time_parts.next()?.parse().ok()?;
    let se: i64 = time_parts
        .next()?
        .split('.')
        .next()?
        .parse()
        .ok()?;
    let days = days_from_civil(y, mo, d);
    Some((days * 86400 + h * 3600 + mi * 60 + se).max(0) as u64)
}

// Howard Hinnant's days_from_civil — proleptic Gregorian, days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn walk_home_subdir(name: &str) -> Vec<Recent> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    walk_dir(&PathBuf::from(home).join(name))
}

fn walk_dir(dir: &Path) -> Vec<Recent> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for ent in rd.flatten() {
        let path = ent.path();
        let Ok(meta) = ent.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(Recent { path, mtime });
    }
    out
}
