use std::path::PathBuf;
use std::thread;
use std::time::SystemTime;

use calloop::channel::Sender;
use walkdir::{DirEntry, WalkDir};

use crate::mounts;
use crate::recents::Recent;

const MAX_DEPTH: usize = 12;
// Cap the file index. Beyond this, fuzzy match is unusable and RAM cost
// dominates the daemon's RSS. 5k recent files covers any realistic search.
const MAX_FILES: usize = 5_000;

// Hidden dirs are no longer blanket-skipped — that hides files like
// `~/.config/foo.toml` or `~/.dotfiles/...` that the user may want to find.
// Instead we maintain an explicit deny-list of known-heavy noise dirs.
const SKIP_NAMES: &[&str] = &[
    // Build / dependency caches (visible names)
    "node_modules",
    "target",
    "build",
    "dist",
    "Trash",
    // Git internals (we still index files in repos, just not inside .git)
    ".git",
    // Tool/lang caches
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".gradle",
    ".m2",
    ".gem",
    ".pnpm-store",
    ".yarn",
    ".bundle",
    ".cabal",
    ".stack",
    ".gradle",
    ".cargo-target",
    ".tox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".venv",
    "venv",
    "__pycache__",
    // Heavy app data dirs (XDG-style)
    ".local",
    ".mozilla",
    ".thunderbird",
    ".steam",
    ".vscode",
    ".vscode-server",
    ".cursor-server",
    ".thumbnails",
    // Browser profiles / data
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/discord",
];

pub fn spawn(tx: Sender<Vec<Recent>>) {
    thread::Builder::new()
        .name("fileindex".into())
        .spawn(move || {
            let roots = mounts::walk_roots();
            log::info!("fileindex: roots = {:?}", roots);
            // Stream per-root: home arrives within seconds, NFS shares append
            // incrementally. The UI replaces files_list with each sorted batch
            // so the user sees real results long before slow NFS finishes.
            let mut accumulated: Vec<Recent> = Vec::new();
            for root in &roots {
                log::info!("fileindex: walking {}", root.display());
                let chunk = walk_one(root);
                log::info!(
                    "fileindex: +{} files from {}",
                    chunk.len(),
                    root.display()
                );
                accumulated.extend(chunk);
                accumulated.sort_by(|a, b| b.mtime.cmp(&a.mtime));
                accumulated.truncate(MAX_FILES);
                accumulated.shrink_to_fit();
                if tx.send(accumulated.clone()).is_err() {
                    return;
                }
            }
            log::info!("fileindex: done, {} files total", accumulated.len());
        })
        .ok();
}

// Bounded-heap walk: keep at most MAX_FILES entries by descending mtime.
// Drops the cold tail in O(log N) per insert, so peak RSS stays flat
// regardless of how many files the root contains.
fn walk_one(root: &PathBuf) -> Vec<Recent> {
    use std::collections::BinaryHeap;
    use std::cmp::Reverse;

    let mut heap: BinaryHeap<Reverse<(u64, PathBuf)>> = BinaryHeap::with_capacity(MAX_FILES + 1);
    for entry in WalkDir::new(root)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_skipped(e))
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if heap.len() < MAX_FILES {
            heap.push(Reverse((mtime, entry.into_path())));
        } else if let Some(Reverse((min_mtime, _))) = heap.peek() {
            if mtime > *min_mtime {
                heap.pop();
                heap.push(Reverse((mtime, entry.into_path())));
            }
        }
    }
    heap.into_iter()
        .map(|Reverse((mtime, path))| Recent { path, mtime })
        .collect()
}

fn is_skipped(entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name();
    let Some(s) = name.to_str() else {
        return false;
    };
    if entry.depth() == 0 {
        return false;
    }
    // Per-uid trash dirs (`.Trash-1000`, `.Trash-1001` …) — the FreeDesktop
    // trash spec puts them on every mount. Skip the prefix unconditionally.
    if s.starts_with(".Trash-") {
        return true;
    }
    SKIP_NAMES.iter().any(|b| s == *b)
}
