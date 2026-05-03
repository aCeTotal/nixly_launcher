use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const ICON_SIZE: u32 = 48;

pub const DEFAULT_APP: &str = "application-x-executable";
pub const DEFAULT_FILE: &str = "text-x-generic";
pub const DEFAULT_DIR: &str = "folder";

pub struct Resolver {
    index: HashMap<String, PathBuf>,
}

impl Resolver {
    pub fn build(extra_dirs: Vec<PathBuf>) -> Self {
        let mut search_dirs = standard_search_dirs();
        search_dirs.extend(extra_dirs);
        search_dirs.sort();
        search_dirs.dedup();
        let themes = preferred_themes();
        let index = build_index(&search_dirs, &themes);
        log::info!(
            "icon index: {} entries from {} dirs",
            index.len(),
            search_dirs.len()
        );
        Self { index }
    }

    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() {
            return None;
        }
        if name.starts_with('/') && std::fs::metadata(name).is_ok() {
            return Some(PathBuf::from(name));
        }
        let bare = strip_known_ext(name);
        self.index.get(bare).cloned()
    }
}

fn strip_known_ext(name: &str) -> &str {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if matches!(
            ext,
            "png" | "PNG" | "jpg" | "JPG" | "jpeg" | "svg" | "xpm"
        ) {
            return stem;
        }
    }
    name
}

fn standard_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(xdg_data_home) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(xdg_data_home).join("icons"));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/icons"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".icons"));
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_DIRS") {
        for d in xdg.split(':').filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(d).join("icons"));
            dirs.push(PathBuf::from(d).join("pixmaps"));
        }
    }
    for fallback in [
        "/run/current-system/sw/share/icons",
        "/run/current-system/sw/share/pixmaps",
        "/usr/share/icons",
        "/usr/share/pixmaps",
        "/usr/local/share/icons",
        "/usr/local/share/pixmaps",
    ] {
        dirs.push(PathBuf::from(fallback));
    }
    dirs
}

fn preferred_themes() -> Vec<String> {
    let mut themes: Vec<String> = Vec::new();
    if let Some(t) = std::env::var_os("XDG_ICON_THEME") {
        themes.push(t.to_string_lossy().into_owned());
    }
    for t in ["Adwaita", "Papirus", "breeze", "hicolor"] {
        if !themes.iter().any(|x| x == t) {
            themes.push(t.to_string());
        }
    }
    themes
}

// Walks every `<dir>/<theme>/<size>/<category>` and every flat pixmaps dir
// once, accumulating `name -> best path` based on a fixed priority. Priority
// favours raster sizes close to ICON_SIZE so downscaling stays sharp; SVGs are
// indexed only when no raster alternative exists, since we don't yet rasterise
// them (resvg is a deferred dep — the value of the index lives on PNG-only).
fn build_index(search_dirs: &[PathBuf], themes: &[String]) -> HashMap<String, PathBuf> {
    let mut best: HashMap<String, (u32, PathBuf)> = HashMap::new();

    let put = |best: &mut HashMap<String, (u32, PathBuf)>,
               stem: &str,
               prio: u32,
               path: PathBuf| {
        match best.get(stem) {
            Some((p, _)) if *p >= prio => {}
            _ => {
                best.insert(stem.to_string(), (prio, path));
            }
        }
    };

    // read_dir transparently follows symlinks (NixOS profile aggregation
    // produces symlinks rather than copies), so explicit file_type checks are
    // skipped — read_dir simply fails on non-directory entries.
    for dir in search_dirs {
        for theme in themes {
            let theme_dir = dir.join(theme);
            let Ok(sizes) = std::fs::read_dir(&theme_dir) else {
                continue;
            };
            for size in sizes.flatten() {
                let size_name = size.file_name();
                let Some(size_str) = size_name.to_str() else { continue };
                let size_prio = size_priority(size_str);
                let Ok(cats) = std::fs::read_dir(size.path()) else { continue };
                for cat in cats.flatten() {
                    let Ok(files) = std::fs::read_dir(cat.path()) else { continue };
                    for f in files.flatten() {
                        let path = f.path();
                        let Some((stem, ext_prio)) = stem_and_ext_priority(&path) else {
                            continue;
                        };
                        let prio = size_prio * 100 + ext_prio;
                        put(&mut best, &stem, prio, path);
                    }
                }
            }
        }
        if dir.ends_with("pixmaps") {
            let Ok(files) = std::fs::read_dir(dir) else { continue };
            for f in files.flatten() {
                let path = f.path();
                let Some((stem, ext_prio)) = stem_and_ext_priority(&path) else { continue };
                let prio = 100 + ext_prio; // pixmaps rank below smallest themed size
                put(&mut best, &stem, prio, path);
            }
        }
    }

    best.into_iter().map(|(k, (_, v))| (k, v)).collect()
}

fn size_priority(name: &str) -> u32 {
    match name {
        "48x48" => 10,
        "64x64" => 9,
        "32x32" => 8,
        "128x128" => 7,
        "256x256" => 6,
        "scalable" => 5,
        "24x24" => 4,
        "16x16" => 3,
        _ => 2,
    }
}

fn stem_and_ext_priority(path: &Path) -> Option<(String, u32)> {
    let stem = path.file_stem()?.to_str()?.to_string();
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let prio = match ext.as_str() {
        "png" => 30,
        "jpg" | "jpeg" => 20,
        "svg" => 15,
        _ => return None,
    };
    Some((stem, prio))
}

pub fn load_rgba(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    if ext.as_deref() == Some("svg") {
        return load_svg(path);
    }
    let img = image::open(path).ok()?;
    let resized = img.resize_exact(
        ICON_SIZE,
        ICON_SIZE,
        image::imageops::FilterType::Lanczos3,
    );
    let rgba = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut data = rgba.into_raw();
    premultiply(&mut data);
    Some((data, w, h))
}

// resvg renders into a tiny_skia::Pixmap which is already premultiplied RGBA —
// no extra conversion needed before atlas upload.
fn load_svg(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let bytes = std::fs::read(path).ok()?;
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&bytes, &opt).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(ICON_SIZE, ICON_SIZE)?;
    let size = tree.size();
    let scale_x = ICON_SIZE as f32 / size.width();
    let scale_y = ICON_SIZE as f32 / size.height();
    let scale = scale_x.min(scale_y);
    let dx = (ICON_SIZE as f32 - size.width() * scale) * 0.5;
    let dy = (ICON_SIZE as f32 - size.height() * scale) * 0.5;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale)
        .post_translate(dx, dy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some((pixmap.data().to_vec(), ICON_SIZE, ICON_SIZE))
}

fn premultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        px[0] = ((px[0] as u32 * a) / 255) as u8;
        px[1] = ((px[1] as u32 * a) / 255) as u8;
        px[2] = ((px[2] as u32 * a) / 255) as u8;
    }
}

// Rounded slate square. Used when the resolver returns nothing, so the row
// keeps its rhythm regardless of icon coverage.
pub fn default_rgba() -> Vec<u8> {
    let size = ICON_SIZE;
    let mut out = vec![0u8; (size * size * 4) as usize];
    let half = (size as f32) * 0.5;
    let radius = (size as f32) * 0.18;
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 + 0.5) - half;
            let dy = (y as f32 + 0.5) - half;
            let extent = half - 1.0;
            let sx = (dx.abs() - (extent - radius)).max(0.0);
            let sy = (dy.abs() - (extent - radius)).max(0.0);
            let dist = (sx * sx + sy * sy).sqrt() - radius;
            let cov = (0.5 - dist).clamp(0.0, 1.0);
            let alpha = (cov * 255.0) as u8;
            let t = y as f32 / size as f32;
            let r = (110.0 + 12.0 * t) as u32;
            let g = (118.0 + 18.0 * t) as u32;
            let b = (140.0 + 28.0 * t) as u32;
            let i = ((y * size + x) * 4) as usize;
            let a = alpha as u32;
            out[i] = ((r * a) / 255) as u8;
            out[i + 1] = ((g * a) / 255) as u8;
            out[i + 2] = ((b * a) / 255) as u8;
            out[i + 3] = alpha;
        }
    }
    out
}
