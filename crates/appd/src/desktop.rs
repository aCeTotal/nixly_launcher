use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Entry {
    pub name: String,
    pub exec: Vec<String>,
    pub icon: Option<String>,
    pub path: PathBuf,
}

// XDG precedence: user dir first.
pub fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".local/share/applications"));
        paths.push(home.join(".nix-profile/share/applications"));
    }
    if let Ok(user) = std::env::var("USER") {
        paths.push(PathBuf::from(format!(
            "/etc/profiles/per-user/{user}/share/applications"
        )));
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_DIRS") {
        paths.extend(xdg.split(':').map(|d| PathBuf::from(d).join("applications")));
    }
    paths.extend(
        [
            "/run/current-system/sw/share/applications",
            "/var/lib/flatpak/exports/share/applications",
            "/usr/local/share/applications",
            "/usr/share/applications",
        ]
        .map(PathBuf::from),
    );
    paths
}

pub fn index() -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    // First desktop ID wins.
    let mut seen_id: std::collections::HashSet<std::ffi::OsString> =
        std::collections::HashSet::new();
    for dir in search_paths() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            if !seen_id.insert(ent.file_name()) {
                continue;
            }
            if let Some(e) = parse(&p) {
                out.push(e);
            }
        }
    }
    // Same app, different IDs.
    let mut seen_pair: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    out.retain(|e| {
        let key = (
            e.name.to_lowercase(),
            e.exec.first().cloned().unwrap_or_default(),
        );
        seen_pair.insert(key)
    });
    Ok(out)
}

// Minimal stand-in parser: pulls Name=, Exec=, Icon=, NoDisplay=, Hidden=
// from the [Desktop Entry] section. Replace with `freedesktop-desktop-entry`
// crate once locale handling and Type=Application filtering matter.
fn parse(path: &Path) -> Option<Entry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_section = false;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut no_display = false;
    let mut hidden = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_section = section == "Desktop Entry";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "Name" => name = Some(v.trim().to_string()),
            "Exec" => exec = Some(v.trim().to_string()),
            "Icon" => icon = Some(v.trim().to_string()),
            "NoDisplay" => no_display = v.trim().eq_ignore_ascii_case("true"),
            "Hidden" => hidden = v.trim().eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    if no_display || hidden {
        return None;
    }
    let name = name?;
    let exec_raw = exec?;
    Some(Entry {
        name,
        exec: split_exec(&exec_raw),
        icon,
        path: path.to_path_buf(),
    })
}

// Strips %f/%F/%u/%U/%i/%c/%k field codes per Desktop Entry Spec.
// Quoting handling is deliberately simplified — full spec uses backslash
// escapes inside double quotes.
fn split_exec(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quote = !in_quote,
            ' ' | '\t' if !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '%' if !in_quote => {
                let _ = chars.next();
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
